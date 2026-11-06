from __future__ import annotations

import asyncio
import importlib
import unittest
from unittest.mock import AsyncMock, patch

from test_repl import ReplProcess, reply_ok, stream_text, wait_for_host_request

rlm_module = importlib.import_module("rlm")


class NotifyTest(unittest.TestCase):
    def test_module_and_namespace_notify_wait_only_for_admission(self) -> None:
        for target in (rlm_module, rlm_module.rlm):
            with self.subTest(target=target):
                host = AsyncMock(return_value={})
                with patch.object(rlm_module, "host_request", host):
                    self.assertIsNone(asyncio.run(target.notify("  stage complete  ")))
                host.assert_awaited_once_with("rlm.notify", {"message": "stage complete"})

    def test_invalid_messages_never_reach_host(self) -> None:
        for message, error in ((None, TypeError), (42, TypeError), (" \n\t", ValueError)):
            with self.subTest(message=message):
                host = AsyncMock()
                with patch.object(rlm_module, "host_request", host):
                    with self.assertRaises(error):
                        asyncio.run(rlm_module.rlm.notify(message))
                host.assert_not_awaited()

    def test_admission_failure_is_reported_to_coroutine(self) -> None:
        host = AsyncMock(side_effect=RuntimeError("session is closed"))
        with patch.object(rlm_module, "host_request", host):
            with self.assertRaisesRegex(RuntimeError, "session is closed"):
                asyncio.run(rlm_module.rlm.notify("finished"))

    def test_background_coroutine_notifies_after_cell_returns(self) -> None:
        repl = ReplProcess()
        self.addCleanup(repl.close)
        repl.ready()
        events = repl.execute("start", """
import asyncio
from rlm import rlm
stage = asyncio.Event()
async def work():
    await stage.wait()
    await rlm.notify("stage one")
    await rlm.notify("finished")
    return "admitted"
job = asyncio.create_task(work())
""")
        self.assertFalse(any(e.get("event") == "host_request" for e in events))
        events = repl.execute("release", "stage.set()")
        first = wait_for_host_request(repl, events)
        self.assertEqual(first["data"], {"type": "rlm.notify", "message": "stage one"})
        # A pending notification does not block subsequent cells.
        events = repl.execute("pending", "print(job.done())")
        self.assertEqual(stream_text(events, "stdout"), "False\n")
        reply_ok(repl, first)
        second = wait_for_host_request(repl, [])
        self.assertEqual(second["data"], {"type": "rlm.notify", "message": "finished"})
        reply_ok(repl, second)
        events = repl.execute("result", "print(await job)")
        self.assertEqual(stream_text(events, "stdout"), "admitted\n")
        self.assertEqual(repl.shutdown(), 0)
