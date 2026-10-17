"""JSONL transport for a persistent, real Nix REPL (no Python cell execution)."""
from __future__ import annotations

import asyncio
import codecs
import json
import os
from pathlib import Path
import re
import signal
import sys
import tempfile
import uuid

ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
ASSIGN = re.compile(r"^([A-Za-z_][A-Za-z0-9_'\-]*)\s*=(?!=)(.*)$", re.S)
CAP = 1_048_576
PROTOCOL_VERSION = 3


def send(event: dict) -> None:
    print(json.dumps(event, separators=(",", ":")), flush=True)


def nix_string(value: str) -> str:
    return ('"' + value.replace("\\", "\\\\").replace('"', '\\"')
            .replace("\n", "\\n").replace("\r", "\\r").replace("\t", "\\t")
            .replace("${", r"\${") + '"')



class Runtime:
    def __init__(self) -> None:
        self.proc = None
        self.queue = asyncio.Queue()
        self.active = None
        self.interrupted = set()
        self.pending = set()
        self.replies = {}
        self.names = set()
        self.literals = {}
        self.closing = False

    async def start(self) -> None:
        self.proc = await asyncio.create_subprocess_exec(
            "nix", "repl", "--extra-experimental-features", "nix-command flakes",
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT, start_new_session=True,
            limit=2 * CAP,
        )
        await self.evaluate("null")

    async def evaluate(self, source: str) -> str:
        if self.proc.returncode is not None:
            raise RuntimeError("Nix REPL exited; restart the kernel")
        marker = "prime_agent_fence_" + uuid.uuid4().hex
        self.proc.stdin.write((source + "\n" + nix_string(marker) + "\n").encode())
        await self.proc.stdin.drain()
        chunks = []
        size = 0
        pending = b""
        decoder = codecs.getincrementaldecoder("utf-8")("replace")
        saw_error = False
        previous = ""
        fence = (json.dumps(marker) + "\n").encode()
        while True:
            chunk = await self.proc.stdout.read(65536)
            if not chunk:
                raise RuntimeError("Nix REPL exited unexpectedly")
            pending += chunk
            # Strip terminal colour before matching; Nix adds ANSI even on pipes.
            pending = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", pending)
            end = pending.find(fence)
            if end >= 0:
                data = pending[:end]
            else:
                retain = len(fence) + 32
                if len(pending) <= retain:
                    continue
                data, pending = pending[:-retain], pending[-retain:]
            text = decoder.decode(data, final=end >= 0)
            if re.search(r"(^|\n)error:", previous + text):
                saw_error = True
            previous = (previous + text)[-8:]
            if size < CAP:
                chunks.append(text[:CAP-size])
                size += len(text)
            if end >= 0:
                break
        output = "".join(chunks).strip()
        if saw_error:
            raise ValueError(output)
        return output

    async def parse(self, expression: str) -> str:
        # Nix's parser normalizes multiline input without evaluating any thunks.
        source = "{ " + ", ".join(sorted(self.names)) + " }: (\n" + expression + "\n)"
        proc = await asyncio.create_subprocess_exec(
            "nix-instantiate", "--parse", "--expr", source,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            start_new_session=True,
        )
        try:
            out, err = await proc.communicate()
        finally:
            if proc.returncode is None:
                proc.kill()
                await proc.wait()
        if proc.returncode:
            raise ValueError(ANSI.sub("", err.decode(errors="replace")))
        return "(" + out.decode().strip() + ") { " + " ".join(f"inherit {name};" for name in sorted(self.names)) + " }"

    async def execute(self, code: str) -> str:
        code = code.strip()
        if not code:
            return ""
        if code.startswith(":host "):
            expr = code[len(":host "):]
            expr = await self.parse(expr)
            encoded = await self.evaluate(":p (builtins.toJSON (" + expr + "))")
            data = json.loads(encoded)
            if self.active in self.interrupted:
                raise InterruptedError("Host request interrupted before dispatch")
            if not isinstance(data, dict):
                raise ValueError(":host requires a Nix attribute set")
            rid = uuid.uuid4().hex
            future = asyncio.get_running_loop().create_future()
            self.replies[rid] = future
            send({"event": "host_request", "id": rid, "data": data})
            try:
                reply = await future
            finally:
                self.replies.pop(rid, None)
            encoded_reply = json.dumps(reply, ensure_ascii=False)
            await self.evaluate("lastHostReply = builtins.fromJSON " + nix_string(encoded_reply))
            self.names.add("lastHostReply")
            self.literals.pop("lastHostReply", None)
            if not isinstance(reply, dict) or reply.get("status") != "ok":
                raise ValueError("Host request failed: " + encoded_reply)
            return encoded_reply
        if code.startswith(":"):
            command, _, expr = code.partition(" ")
            if command not in {":b", ":p", ":t", ":doc", ":?", ":help"}:
                raise ValueError(f"Unsupported REPL command {command}; interactive shells/editors and scope-loading commands are disabled. Use :host tool.bash for nix develop/nix shell.")
            if "\n" in code:
                raise ValueError("REPL commands must occupy one line")
            if expr:
                expr = await self.parse(expr)
            if self.active in self.interrupted:
                raise InterruptedError("Nix command interrupted before execution")
            return await self.evaluate(command + (" " + expr if expr else ""))
        match = ASSIGN.match(code)
        name, expr = (match[1], match[2].strip()) if match else (None, code)
        parsed = await self.parse(expr)
        if self.active in self.interrupted:
            raise InterruptedError("Nix evaluation interrupted")
        source = f"{name} = {parsed}" if name else parsed
        output = await self.evaluate(source)
        if name:
            self.names.add(name)
            self.literals.pop(name, None)
            # Only literal scalars are checkpointable without forcing lazy values.
            try:
                value = json.loads(expr)
                if (value is None or isinstance(value, (str, bool, int, float))) and "${" not in expr and not re.search(r"\\(?:u|b|f|/)", expr):
                    self.literals[name] = value
            except (ValueError, OverflowError):
                pass
        return output

    async def state(self, req: dict) -> dict:
        path = Path(req["path"])
        if req["type"] == "restore":
            if not path.exists():
                return {"restored": [], "failed": [], "reason": "snapshot not found"}
            payload = json.loads(path.read_text())
            if payload.get("format") != "prime-agent-nix-literals-v1":
                raise ValueError("Not a Nix literal snapshot; Python snapshots cannot be restored")
            restored = []
            for name, value in payload["values"].items():
                if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_'\-]*", name):
                    raise ValueError("Invalid snapshot binding name")
                if isinstance(value, (dict, list)):
                    raise ValueError("Invalid snapshot literal")
                expr = nix_string(value) if isinstance(value, str) else json.dumps(value, allow_nan=False)
                await self.execute(f"{name} = {expr}")
                restored.append(name)
            return {"restored": restored, "failed": []}
        values = {}
        skipped = []
        for name in sorted(self.names):
            if name not in self.literals:
                skipped.append({"name": name, "reason": "Lazy or non-literal value; not forced or replayed"})
                continue
            value = self.literals[name]
            if len(json.dumps(value).encode()) > req.get("max_variable_bytes", 16 * 1024 * 1024):
                skipped.append({"name": name, "reason": "per-variable limit"})
                continue
            values[name] = value
        payload = json.dumps({"format": "prime-agent-nix-literals-v1", "values": values}, allow_nan=False)
        if len(payload.encode()) > req.get("max_bytes", 256 * 1024 * 1024):
            raise ValueError("Nix literal snapshot exceeds max_bytes")
        manifest = {"version": 1, "savedNames": sorted(values), "skipped": skipped, "pruned": [], "bytes": len(payload.encode()), "runtime": "nix"}
        for target, content in [(path, payload), (Path(req["manifest_path"]), json.dumps(manifest))]:
            target.parent.mkdir(parents=True, exist_ok=True)
            with tempfile.NamedTemporaryFile(mode="w", dir=target.parent, delete=False) as file:
                file.write(content)
                tmp = file.name
            try:
                os.replace(tmp, target)
            finally:
                if os.path.exists(tmp):
                    os.unlink(tmp)
        return {"saved": sorted(values), "skipped": skipped, "pruned": [], "bytes": len(payload.encode())}

    def interrupt(self, rid=None) -> None:
        target = rid or self.active or next(iter(self.pending), None)
        if target not in self.pending:
            return
        self.interrupted.add(target)
        if target == self.active:
            if self.proc and self.proc.returncode is None:
                os.killpg(self.proc.pid, signal.SIGINT)
            for future in self.replies.values():
                if not future.done():
                    future.set_exception(InterruptedError("Host request interrupted"))

    async def read(self) -> None:
        reader = asyncio.StreamReader(limit=16 * 1024 * 1024)
        protocol = asyncio.StreamReaderProtocol(reader)
        await asyncio.get_running_loop().connect_read_pipe(lambda: protocol, sys.stdin)
        while raw := await reader.readline():
            try:
                req = json.loads(raw)
                if not isinstance(req, dict):
                    raise ValueError("request must be an object")
                kind, rid = req.get("type"), req.get("id")
                if kind == "interrupt":
                    self.interrupt(rid)
                    continue
                if kind == "host_reply":
                    future = self.replies.get(rid)
                    if future is not None and not future.done():
                        future.set_result(req.get("data"))
                    continue
                if kind == "shutdown":
                    self.closing = True
                    self.interrupt()
                    await self.close()
                    await self.queue.put(req)
                    return
                if kind not in {"execute", "snapshot", "restore", "list_names", "mcp_status", "bash_activity", "factory_activity"}:
                    raise ValueError(f"unknown request type: {kind}")
                if not isinstance(rid, str) or rid in self.pending:
                    raise ValueError("request needs unique string id")
                if kind == "execute" and not isinstance(req.get("code"), str):
                    raise ValueError("execute needs string code")
                self.pending.add(rid)
                await self.queue.put(req)
            except (ValueError, TypeError) as exc:
                send({"event": "error", "id": None, "ename": "ProtocolError", "evalue": str(exc), "traceback": []})
        self.closing = True
        self.interrupt()
        await self.close()
        await self.queue.put({"type": "shutdown"})

    async def close(self) -> None:
        if self.proc and self.proc.returncode is None:
            os.killpg(self.proc.pid, signal.SIGTERM)
            try:
                await asyncio.wait_for(self.proc.wait(), 2)
            except asyncio.TimeoutError:
                os.killpg(self.proc.pid, signal.SIGKILL)
                await self.proc.wait()

    async def serve(self) -> None:
        try:
            await self.start()
        except BaseException:
            await self.close()
            raise
        send({"event": "ready", "protocol": PROTOCOL_VERSION, "runtime": "nix"})
        reader = asyncio.create_task(self.read())
        try:
            while True:
                req = await self.queue.get()
                rid, kind = req.get("id"), req["type"]
                if kind == "shutdown":
                    await self.close()
                    if rid:
                        send({"event": "done", "id": rid, "status": "ok"})
                    break
                self.active = rid
                status, extra = "ok", {}
                try:
                    if rid in self.interrupted or self.closing:
                        raise InterruptedError("Nix evaluation interrupted")
                    if kind == "execute":
                        output = await self.execute(req["code"])
                        if rid in self.interrupted:
                            raise InterruptedError("Nix evaluation interrupted")
                        if output:
                            send({"event": "result", "id": rid, "text": output})
                    elif kind in {"snapshot", "restore"}:
                        extra = await self.state(req)
                    elif kind == "list_names":
                        extra = {"names": sorted(self.names)}
                    else:
                        raise ValueError(f"{kind} is not supported by the Nix runtime")
                except Exception as exc:
                    status = "error"
                    extra = {"reason": str(exc)[:CAP]}
                    send({"event": "error", "id": rid, "ename": "KeyboardInterrupt" if rid in self.interrupted else type(exc).__name__, "evalue": str(exc)[:CAP], "traceback": []})
                self.pending.discard(rid)
                self.interrupted.discard(rid)
                self.active = None
                send({"event": "done", "id": rid, "status": status, **extra})
        finally:
            reader.cancel()
            await self.close()


def main() -> None:
    runtime = Runtime()
    try:
        asyncio.run(runtime.serve())
    except (OSError, RuntimeError) as exc:
        print(f"Nix runtime failed: {exc}", file=sys.stderr)
        raise SystemExit(1) from exc


if __name__ == "__main__":
    main()
