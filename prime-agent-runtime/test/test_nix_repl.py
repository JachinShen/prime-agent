from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest


class NixProcess:
    def __init__(self):
        self.proc = subprocess.Popen(
            [sys.executable, "-m", "rlm.nix_repl"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, env={**os.environ, "PYTHONPATH": str(Path(__file__).resolve().parents[1] / "src")},
        )
        self.events = queue.Queue()
        threading.Thread(target=self.read, daemon=True).start()
        assert self.event()["event"] == "ready"

    def read(self):
        for line in self.proc.stdout:
            self.events.put(json.loads(line))
        self.events.put(None)

    def event(self):
        event = self.events.get(timeout=15)
        if event is None:
            raise AssertionError("Runtime exited: " + self.proc.stderr.read())
        return event

    def send(self, data):
        self.proc.stdin.write(json.dumps(data) + "\n")
        self.proc.stdin.flush()

    def done(self, rid):
        events = []
        while True:
            event = self.event()
            events.append(event)
            if event.get("event") == "done" and event.get("id") == rid:
                return events

    def execute(self, code, rid="cell"):
        self.send({"type": "execute", "id": rid, "code": code})
        return self.done(rid)

    def close(self):
        if self.proc.poll() is None:
            self.send({"type": "shutdown", "id": "bye"})
            self.done("bye")
            self.proc.wait(timeout=5)
        for stream in [self.proc.stdin, self.proc.stdout, self.proc.stderr]:
            stream.close()


@unittest.skipUnless(shutil.which("nix") and shutil.which("nix-instantiate"), "requires real Nix")
class NixReplTest(unittest.TestCase):
    def setUp(self):
        self.runtime = NixProcess()
        self.addCleanup(self.runtime.close)

    def result(self, code):
        events = self.runtime.execute(code)
        self.assertEqual(events[-1], {"event": "done", "id": "cell", "status": "ok"})
        return "".join(e["text"] for e in events if e["event"] == "result")

    def test_persistence_multiline_errors_and_python_rejection(self):
        self.result("answer = 40 + 2")
        self.assertEqual(self.result("answer"), "42")
        self.assertEqual(self.result("let\n  x = answer;\nin x + 1"), "43")
        for bad in ["(", "builtins.throw \"oops\"", "import os; os.system('false')", ":sh null"]:
            events = self.runtime.execute(bad)
            self.assertEqual(events[-1]["status"], "error", events)
        self.assertEqual(self.result("answer"), "42")

    def test_host_bridge_uses_live_nix_binding(self):
        self.result('target = "hello"')
        self.runtime.send({"type": "execute", "id": "host", "code": ':host { type = "test"; value = "${target} world"; }'})
        event = self.runtime.event()
        self.assertEqual(event["event"], "host_request")
        self.assertEqual(event["data"], {"type": "test", "value": "hello world"})
        self.runtime.send({"type": "host_reply", "id": event["id"], "data": {"status": "ok", "result": {"value": 7}}})
        events = self.runtime.done("host")
        self.assertEqual(json.loads(events[0]["text"]), {"status": "ok", "result": {"value": 7}})
        self.assertEqual(self.result("lastHostReply.result.value"), "7")

    def test_snapshot_never_forces_or_replays_thunks(self):
        self.result("literal = 12")
        self.result("computed = 40 + 2")
        self.result('lazy = builtins.throw "must not force"')
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / "snapshot.json")
            self.runtime.send({"type": "snapshot", "id": "save", "path": path, "manifest_path": path + ".manifest"})
            done = self.runtime.done("save")[-1]
            self.assertEqual(done["status"], "ok")
            self.assertEqual(done["saved"], ["literal"])
            self.assertEqual([x["name"] for x in done["skipped"]], ["computed", "lazy"])
            fresh = NixProcess()
            try:
                fresh.send({"type": "restore", "id": "restore", "path": path})
                self.assertEqual(fresh.done("restore")[-1]["restored"], ["literal"])
                self.assertEqual(fresh.execute("literal")[0]["text"], "12")
                self.assertEqual(fresh.execute("computed")[-1]["status"], "error")
            finally:
                fresh.close()

    def test_interrupt_host_wait_and_keep_state(self):
        self.result("keep = 99")
        self.runtime.send({"type": "execute", "id": "wait", "code": ':host { type = "wait"; }'})
        self.assertEqual(self.runtime.event()["event"], "host_request")
        self.runtime.send({"type": "interrupt", "id": "wait"})
        events = self.runtime.done("wait")
        self.assertEqual(events[-1]["status"], "error")
        self.assertEqual(self.result("keep"), "99")

    def test_relative_paths_large_output_and_flake(self):
        self.assertEqual(json.loads(self.result("toString ./.") ), os.getcwd())
        self.assertTrue(self.result('builtins.concatStringsSep "" (builtins.genList (_: "x") 1100000)').startswith('"xxx'))
        self.assertEqual(self.result("1 + 1"), "2")
        text = json.loads(self.result('builtins.concatStringsSep "" (builtins.genList (_: "汉字") 50000)'))
        self.assertEqual(text, "汉字" * 50000)
        with tempfile.TemporaryDirectory() as directory:
            Path(directory, "flake.nix").write_text('{ outputs = { self }: { answer = 42; }; }')
            self.result("flake = builtins.getFlake " + json.dumps(directory))
            self.assertEqual(self.result("flake.answer"), "42")

    def test_queued_interrupt_and_host_error(self):
        self.runtime.send({"type": "execute", "id": "cancel", "code": "1 + 1"})
        self.runtime.send({"type": "interrupt", "id": "cancel"})
        self.assertEqual(self.runtime.done("cancel")[-1]["status"], "error")
        self.runtime.send({"type": "execute", "id": "host", "code": ':host { type = "bad"; }'})
        event = self.runtime.event()
        self.runtime.send({"type": "host_reply", "id": event["id"], "data": {"status": "error", "error": "bad"}})
        self.assertEqual(self.runtime.done("host")[-1]["status"], "error")
        self.assertEqual(self.result("lastHostReply.status"), '"error"')

    def test_host_reply_control_characters_roundtrip(self):
        value = "\b\f\x01你好${notInterpolation}"
        self.runtime.send({"type": "execute", "id": "host", "code": ':host { type = "echo"; }'})
        event = self.runtime.event()
        self.runtime.send({"type": "host_reply", "id": event["id"], "data": {"status": "ok", "result": value}})
        self.assertEqual(self.runtime.done("host")[-1]["status"], "ok")
        self.runtime.send({"type": "execute", "id": "again", "code": ':host { type = "echo"; value = lastHostReply.result; }'})
        event = self.runtime.event()
        self.assertEqual(event["data"]["value"], value)
        self.runtime.send({"type": "host_reply", "id": event["id"], "data": {"status": "ok", "result": {}}})
        self.assertEqual(self.runtime.done("again")[-1]["status"], "ok")

    def test_active_nix_evaluation_interrupt(self):
        from rlm.nix_repl import Runtime

        async def check():
            runtime = Runtime()
            await runtime.start()
            try:
                await runtime.execute("keep = 73")
                ready = asyncio.Event()
                original = runtime.proc.stdout.read

                async def observed_read(size):
                    data = await original(size)
                    if b"interrupt-ready" in data:
                        ready.set()
                    return data

                runtime.proc.stdout.read = observed_read
                runtime.active = "busy"
                runtime.pending.add("busy")
                task = asyncio.create_task(runtime.execute('builtins.trace "interrupt-ready" (builtins.foldl\' (a: b: a + b) 0 (builtins.genList (x: x) 1000000))'))
                await asyncio.wait_for(ready.wait(), 10)
                runtime.interrupt("busy")
                with self.assertRaises(ValueError):
                    await asyncio.wait_for(task, 5)
                runtime.active = None
                self.assertEqual(await runtime.execute("keep"), "73")
            finally:
                await runtime.close()

        asyncio.run(check())

    def test_eof_cleans_up(self):
        self.runtime.proc.stdin.close()
        self.assertEqual(self.runtime.proc.wait(timeout=5), 0)


if __name__ == "__main__":
    unittest.main()
