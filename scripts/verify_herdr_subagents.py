#!/usr/bin/env python3
"""Opt-in live verifier: real daemon, Python kernel, Herdr panes, and Codex.

Build pa-daemon first. This script creates one temporary Herdr workspace and one
isolated daemon, verifies two automatic parent wakeups, then closes its resources.
Artifacts remain under the reported temporary directory for inspection.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid


REPO = Path(__file__).resolve().parents[1]
PROTOCOL = {"name": "prime-agent.daemon", "version": 7}


def herdr(*arguments: str) -> dict:
    result = subprocess.run(["herdr", *arguments], text=True, capture_output=True, timeout=30)
    if result.returncode:
        raise RuntimeError(f"herdr {' '.join(arguments)} failed: {result.stderr or result.stdout}")
    response = json.loads(result.stdout)
    if "error" in response:
        raise RuntimeError(f"Herdr rejected {' '.join(arguments)}: {response['error']}")
    return response["result"]


def focus_context() -> dict:
    snapshot = herdr("api", "snapshot")["snapshot"]
    return {key: snapshot[key] for key in ("focused_workspace_id", "focused_tab_id", "focused_pane_id")}


def check_focus(report: dict, workspace: str, phase: str, owned_panes: list[str]):
    current = focus_context()
    original = report["original_focus"]
    changed = current != original
    report.setdefault("focus_observations", []).append(
        {"phase": phase, "original": original, "current": current, "changed": changed})
    if current["focused_workspace_id"] == workspace or current["focused_pane_id"] in owned_panes:
        raise RuntimeError(f"Verifier resource became focused during {phase}: {current}")
    if changed:
        report.setdefault("concurrent_focus_changes", []).append({"phase": phase, "current": current})
        print(f"Concurrent user focus change during {phase}: {original} -> {current}", file=sys.stderr)


def validate_collected(result: dict, turn: int, token: str, root: Path, child_pane: str):
    info = result.get("external", {})
    if result.get("status") != "done" or not result.get("settled") or token not in (result.get("answer_text") or ""):
        raise RuntimeError(f"Turn {turn} did not collect its real answer file: {result}")
    answer = Path(info["answer_path"])
    if info.get("turn") != turn or info.get("pane_id") != child_pane or not answer.is_relative_to(root) or answer.read_text() != result["answer_text"]:
        raise RuntimeError(f"Turn {turn} answer/identity differs from its collect result: {result}")


class Client:
    def __init__(self, path: Path, daemon: subprocess.Popen):
        deadline = time.monotonic() + 15
        self.socket = socket.socket(socket.AF_UNIX)
        while True:
            try:
                self.socket.connect(str(path))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if daemon.poll() is not None:
                    raise RuntimeError("Verifier supervisor exited before opening its socket")
                if time.monotonic() >= deadline:
                    raise TimeoutError("Verifier supervisor socket never became ready")
                time.sleep(0.02)  # Poll observable socket readiness, not a startup delay.
        self.socket.settimeout(30)
        self.reader = self.socket.makefile("r", encoding="utf-8")
        self.frames: list[dict] = []
        hello = self.read()
        if hello.get("type") != "daemon_hello":
            raise RuntimeError(f"Expected daemon hello, got {hello}")
        self.counter = 0

    def read(self) -> dict:
        line = self.reader.readline()
        if not line:
            raise RuntimeError("Verifier daemon closed its connection")
        frame = json.loads(line)
        self.frames.append(frame)
        return frame

    def request(self, command: dict) -> dict:
        self.counter += 1
        request_id = f"live-{self.counter}"
        envelope = {"type": "command", "id": request_id, "protocol": PROTOCOL, "command": command}
        self.socket.sendall((json.dumps(envelope) + "\n").encode())
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            response = self.read()
            if response.get("id") == request_id and response.get("type") == "response":
                if not response.get("success"):
                    raise RuntimeError(f"{command['type']} failed: {response.get('error', response)}")
                return response.get("data")
        raise TimeoutError(f"No response for {command['type']}")

    def close(self) -> None:
        self.reader.close()
        self.socket.close()


def objects(value):
    if isinstance(value, dict):
        yield value
        for child in value.values():
            yield from objects(child)
    elif isinstance(value, list):
        for child in value:
            yield from objects(child)


def assistant_has_text(messages, expected: str) -> bool:
    for message in objects(messages):
        if message.get("role") != "assistant":
            continue
        content = message.get("content")
        if content == expected:
            return True
        if isinstance(content, list) and any(item.get("type") == "text" and item.get("text") == expected
                                             for item in content if isinstance(item, dict)):
            return True
    return False


def wait_for_parent(client: Client, parent: str, record: Path, acknowledgement: str, timeout: int):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        messages = client.request({"type": "get_messages", "activeSessionId": parent})
        if record.exists() and assistant_has_text(messages, acknowledgement):
            # Kernel records use atomic replace. A malformed artifact is an error.
            return json.loads(record.read_text()), messages
        state = client.request({"type": "get_rlm_children", "activeSessionId": parent})
        errors = [item for item in objects(state) if item.get("taskState", item.get("status")) == "error"]
        if errors:
            raise RuntimeError(f"External child failed before parent acknowledgement: {errors}")
        time.sleep(0.1)  # Wait for observable transcript + artifact readiness.
    raise TimeoutError(f"Parent was not automatically awakened for {acknowledgement}")


def make_script(root: Path, child_name: str, agent_args: list[str], first: str, second: str, *, worktree: bool = False) -> Path:
    def tool(code: str) -> dict:
        return {"content": [{"type": "toolCall", "name": "python_repl", "arguments": {"code": code}}]}

    spawn = f'''import dataclasses, json, os, sys
from pathlib import Path
import rlm
sys.path.insert(0, {str(REPO / 'skills/agent-message/src')!r})
import agent_message
records = Path({str(root / 'records')!r})
def record(name, value):
    path = records / (name + '.json')
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, default=str))
    temporary.replace(path)
handle = await rlm.spawn({first!r}, name={child_name!r}, agent='codex', agent_args={agent_args!r}, worktree={('task/' + child_name) if worktree else False!r})
record('spawn', dataclasses.asdict(handle))'''
    collect1 = "results = await rlm.collect(handle)\nassert len(results) == 1 and results[0].settled\nrecord('wake-one', dataclasses.asdict(results[0]))"
    send = f"receipt = await agent_message.send({second!r}, receiver_role='child', receiver_name=handle.name)\nrecord('followup', receipt)"
    collect2 = "results = await rlm.collect(handle)\nassert len(results) == 1 and results[0].settled\nrecord('wake-two', dataclasses.asdict(results[0]))"
    cleanup = "deleted = await rlm.delete_subagent(handle)\nrecord('deleted', dataclasses.asdict(deleted))"
    responses = [tool(spawn), {"text": "live-wait-one"}, tool(collect1), {"text": "live-wake-one-ack"},
                 tool(send), {"text": "live-wait-two"}, tool(collect2), {"text": "live-wake-two-ack"},
                 tool(cleanup), {"text": "live-cleanup-ack"}]
    script = root / "parent-script.json"
    script.write_text(json.dumps({"engine": "faux", "responses": responses}))
    return script


def verify(args) -> dict:
    if os.environ.get("HERDR_ENV") != "1":
        raise RuntimeError("Run this verifier inside Herdr (HERDR_ENV=1)")
    daemon_binary = args.daemon.expanduser().resolve()
    kernel_python = args.kernel_python.expanduser().absolute()
    if not daemon_binary.is_file() or not os.access(daemon_binary, os.X_OK):
        raise RuntimeError(f"Build pa-daemon first; executable missing: {daemon_binary}")
    if not kernel_python.is_file():
        raise RuntimeError(f"Kernel Python missing: {kernel_python}; pass --kernel-python explicitly")
    for binary in ("herdr", "codex", "git"):
        if shutil.which(binary) is None:
            raise RuntimeError(f"Required executable missing: {binary}")
    env = os.environ.copy()
    env["PYTHONPATH"] = str(REPO / "prime-agent-runtime/src") + (os.pathsep + env["PYTHONPATH"] if env.get("PYTHONPATH") else "")
    subprocess.run([str(kernel_python), "-c", "import inspect, ipykernel, zmq, rlm; assert 'agent' in inspect.signature(rlm.spawn).parameters"],
                   env=env, check=True, timeout=30)
    root = Path(tempfile.mkdtemp(prefix="pa-herdr-live-", dir="/tmp")).resolve()
    print(f"Verifier artifacts: {root}", flush=True)
    (root / "records").mkdir()
    (root / "agent").mkdir()
    subprocess.run(["git", "init", "--quiet", str(root)], check=True, capture_output=True)
    if args.worktree:
        (root / "worktree-probe.txt").write_text("parent checkout\n")
        subprocess.run(["git", "-C", str(root), "add", "worktree-probe.txt"], check=True, capture_output=True)
        # A throwaway test repo uses the ambient identity, just like real commits.
        subprocess.run(["git", "-C", str(root), "commit", "--quiet", "-m", "Worktree verifier baseline"], check=True, capture_output=True)
    child_name = "live-codex-" + uuid.uuid4().hex[:8]
    token1, token2 = "PRIME_HERDR_ONE_" + uuid.uuid4().hex, "PRIME_HERDR_TWO_" + uuid.uuid4().hex
    first = f"Write exactly this line as your final answer to the answer file named above: {token1}. Do not modify other files."
    second = f"Write exactly this line as your final answer to the new answer file named above: {token2}. Do not modify other files."
    if args.worktree:
        first = f"In your current checkout, replace worktree-probe.txt with exactly {token1}. Write {token1} to the final answer file named above. Do not modify other files."
        second = f"Check that worktree-probe.txt contains {token1}, then replace its contents with exactly {token2}. Write {token2} to the new final answer file named above. Do not modify other files."
    agent_args = args.agent_arg if args.agent_arg is not None else ["--sandbox", "workspace-write", "--ask-for-approval", "never", "--no-daemon",
                  "-c", 'model_reasoning_effort="low"',
                  "-c", f'projects={{{json.dumps(str(root))}={{trust_level="trusted"}}}}']
    if args.worktree and args.agent_arg is None:
        agent_args += ["--add-dir", str(root)]
    script = make_script(root, child_name, agent_args, first, second, worktree=args.worktree)
    workspace = None
    daemon = None
    client = None
    child_pane = None
    child_workspace = None
    checkout = None
    cleanup_errors = []
    report = {"artifacts": str(root), "success": False}
    original_focus = focus_context()
    report["original_focus"] = original_focus
    try:
        created = herdr("workspace", "create", "--cwd", str(root), "--label", child_name, "--no-focus")
        workspace = created["workspace"]["workspace_id"]
        caller = created["root_pane"]
        report["workspace_id"] = workspace
        check_focus(report, workspace, "workspace creation", [caller["pane_id"]])
        parent_env = {"HERDR_ENV": "1", "HERDR_WORKSPACE_ID": workspace, "HERDR_PANE_ID": caller["pane_id"],
                      "HERDR_TAB_ID": caller["tab_id"]}
        if os.environ.get("HERDR_SOCKET_PATH"):
            parent_env["HERDR_SOCKET_PATH"] = os.environ["HERDR_SOCKET_PATH"]
        env["PRIME_AGENT_KERNEL_PYTHON"] = str(kernel_python)
        env["PRIME_AGENT_INTERNAL_WORKER_SUPERVISOR_LOST_EXIT_MS"] = "1000"
        env["DO_NOT_TRACK"] = "1"
        daemon_log = open(root / "daemon.log", "w")
        try:
            daemon = subprocess.Popen([str(daemon_binary), "supervisor", "--socket", str(root / "daemon.sock"), "--agent-dir", str(root / "agent")],
                                      env=env, stdout=daemon_log, stderr=subprocess.STDOUT, start_new_session=True)
        finally:
            daemon_log.close()
        client = Client(root / "daemon.sock", daemon)
        parent = client.request({"type": "create", "name": "herdr-live-parent", "telemetryDisabled": True, "env": parent_env,
                                 "config": {"cwd": str(root), "sessionDir": str(root / "sessions"), "script": str(script)}})
        active = parent["activeSessionId"]
        report["parent_active_session_id"] = active
        client.request({"type": "prompt", "activeSessionId": active, "message": "Start the live subagent verifier"})
        one, messages = wait_for_parent(client, active, root / "records/wake-one.json", "live-wake-one-ack", args.timeout)
        spawn = json.loads((root / "records/spawn.json").read_text())
        external = spawn["external"]
        child_pane = external["pane_id"]
        child_workspace = external["workspace_id"]
        expected_cwd = str(root)
        if args.worktree:
            worktree = external.get("worktree")
            if not worktree or worktree["branch"] != "task/" + child_name or child_workspace == workspace:
                raise RuntimeError(f"Spawn omitted its isolated worktree/workspace: {external}")
            checkout = Path(worktree["path"])
            expected_cwd = str(checkout)
            if (checkout / "worktree-probe.txt").read_text().strip() != token1 or (root / "worktree-probe.txt").read_text() != "parent checkout\n":
                raise RuntimeError("First worktree edit escaped or did not occur in the isolated checkout")
        report["first"] = one
        validate_collected(one, 1, token1, root, child_pane)
        if external["agent"] != "codex" or (not args.worktree and child_workspace != workspace) or child_pane == caller["pane_id"] or spawn["model"] is not None:
            raise RuntimeError(f"External spawn identity is invalid: {spawn}")
        owned_panes = herdr("pane", "list", "--workspace", child_workspace)["panes"]
        pane = next((pane for pane in owned_panes if pane["pane_id"] == child_pane), None)
        if pane is None:
            raise RuntimeError("Spawn returned a pane outside the verifier workspace")
        if pane["workspace_id"] != child_workspace or pane["terminal_id"] != external["terminal_id"] or pane.get("foreground_cwd", pane.get("cwd")) != expected_cwd:
            raise RuntimeError(f"Real Codex pane identity/cwd differs from spawn: {pane}")
        check_focus(report, workspace, "external spawn", [caller["pane_id"], child_pane])
        if child_workspace != workspace:
            check_focus(report, child_workspace, "worktree spawn", [child_pane])
        roster = client.request({"type": "roster_subscribe"})["roster"]
        rows = [entry for entry in roster if entry["summary"].get("rlmChildId") == spawn["rlm_child_id"]]
        if len(rows) != 1:
            raise RuntimeError(f"Expected one native external roster row, got {rows}")
        row = rows[0]["summary"]
        if row.get("activeSessionId") is not None or row.get("sessionId") is not None or "sessionFile" in row or row.get("parentActiveSessionId") != active:
            raise RuntimeError(f"External row fabricated child session identity: {row}")
        if rows[0]["status"] != "idle" or row.get("runtimeResident") is not True or row.get("backend") != "herdr":
            raise RuntimeError(f"Live settled external runtime has incorrect native roster status: {rows[0]}")
        client.request({"type": "prompt", "activeSessionId": active, "message": "Send the follow-up through agent_message.send"})
        two, messages = wait_for_parent(client, active, root / "records/wake-two.json", "live-wake-two-ack", args.timeout)
        report["second"] = two
        validate_collected(two, 2, token2, root, child_pane)
        if checkout:
            if (checkout / "worktree-probe.txt").read_text().strip() != token2 or (root / "worktree-probe.txt").read_text() != "parent checkout\n":
                raise RuntimeError("Follow-up turn did not reuse the isolated checkout")
        notices = [entry for entry in objects(messages) if isinstance(entry.get("details"), dict) and entry["details"].get("backend") == "herdr"
                   and entry.get("customType") == "rlm_child_terminal_notice"]
        if sorted(entry["details"]["turn"] for entry in notices) != [1, 2]:
            raise RuntimeError(f"Expected one daemon completion notice per turn: {notices}")
        receipt = json.loads((root / "records/followup.json").read_text())
        target = receipt["target"]
        if target.get("activeSessionId") is not None or target.get("sessionId") is not None or target.get("rlmChildId") != spawn["rlm_child_id"]:
            raise RuntimeError(f"External follow-up receipt fabricated a native session identity: {receipt}")
        report.update({"spawn": spawn, "first": one, "second": two, "roster_row": rows[0], "notices": notices,
                       "followup_receipt": receipt})
        client.request({"type": "prompt", "activeSessionId": active, "message": "Delete the verifier child"})
        deleted, _ = wait_for_parent(client, active, root / "records/deleted.json", "live-cleanup-ack", 30)
        report["deleted"] = deleted
        if checkout:
            if (checkout / "worktree-probe.txt").read_text().strip() != token2:
                raise RuntimeError("Deleting the child lost its uncommitted worktree edit")
            report["worktree_retained_after_delete"] = True
        removal_deadline = time.monotonic() + 15
        while True:
            remaining = client.request({"type": "roster_subscribe"})["roster"]
            if not any(entry["summary"].get("rlmChildId") == spawn["rlm_child_id"] for entry in remaining):
                break
            if time.monotonic() >= removal_deadline:
                raise RuntimeError("Deleted external child remained in native roster")
            time.sleep(0.05)  # Wait for the asynchronous authoritative roster delta.
        report["success"] = True
        return report
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        if not report["success"] and child_pane:
            try:
                owned_panes = herdr("pane", "list", "--workspace", child_workspace or workspace)["panes"]
                if any(pane["pane_id"] == child_pane for pane in owned_panes):
                    terminal = subprocess.run(
                        ["herdr", "pane", "read", child_pane, "--source", "visible", "--lines", "80"],
                        text=True, capture_output=True, timeout=30,
                    )
                    report["failure_terminal"] = {"stdout": terminal.stdout, "stderr": terminal.stderr,
                                                  "exit_status": terminal.returncode}
                    if terminal.returncode:
                        report["failure_terminal_error"] = f"herdr pane read exited {terminal.returncode}"
            except Exception as error:
                report["failure_terminal_error"] = str(error)
        if client:
            try:
                client.request({"type": "shutdown"})
            except Exception as error:
                cleanup_errors.append(f"Daemon shutdown failed: {error}")
            finally:
                client.close()
        if daemon:
            try:
                daemon.wait(timeout=15)
            except subprocess.TimeoutExpired:
                cleanup_errors.append("Daemon did not exit after shutdown; terminated owned process group")
                os.killpg(daemon.pid, signal.SIGTERM)
                daemon.wait(timeout=15)
        if child_workspace and child_workspace != workspace:
            try:
                remaining = herdr("workspace", "list")["workspaces"]
                if any(item["workspace_id"] == child_workspace for item in remaining):
                    herdr("workspace", "close", child_workspace)
            except Exception as error:
                cleanup_errors.append(f"Closing verifier worktree workspace {child_workspace} failed: {error}")
        if checkout:
            try:
                subprocess.run(["git", "-C", str(root), "worktree", "remove", "--force", str(checkout)],
                               check=True, capture_output=True, text=True, timeout=30)
            except Exception as error:
                cleanup_errors.append(f"Removing verifier-owned worktree {checkout} failed: {error}")
        if workspace:
            try:
                herdr("workspace", "close", workspace)
            except Exception as error:
                cleanup_errors.append(f"Closing verifier workspace {workspace} failed: {error}")
        report["cleanup_errors"] = cleanup_errors
        (root / "evidence.json").write_text(json.dumps(report, indent=2))
        if cleanup_errors:
            raise RuntimeError("; ".join(cleanup_errors))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-live", action="store_true", help="Create an isolated Herdr workspace and run a real Codex agent")
    parser.add_argument("--worktree", action="store_true", help="Verify an isolated Git worktree, follow-up edits, and checkout retention")
    parser.add_argument("--daemon", type=Path, default=REPO / "target/debug/pa-daemon")
    default_python = os.environ.get("PRIME_AGENT_KERNEL_PYTHON") or str(Path.home() / ".prime/agent/kernel-venv-py314-personal/bin/python")
    parser.add_argument("--kernel-python", type=Path, default=Path(default_python))
    parser.add_argument("--timeout", type=int, default=240, help="Seconds allowed for each real Codex turn")
    parser.add_argument("--agent-arg", action="append", help="Override Codex arguments; repeat, using --agent-arg=--flag for flags")
    args = parser.parse_args()
    if not args.run_live:
        parser.error("Live resources require explicit --run-live")
    try:
        report = verify(args)
    except Exception as error:
        print(f"Live verifier failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps({"success": report["success"], "artifacts": report["artifacts"], "workspace_closed": report["workspace_id"]}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
