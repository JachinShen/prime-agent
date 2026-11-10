"""RFC-0001 external subagent contracts, without starting terminals or agents."""

from __future__ import annotations

import asyncio
import copy
import importlib
import inspect
import re
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch

rlm = importlib.import_module("rlm")


def external_payload() -> dict:
    return {
        "agent": "codex",
        "workspace_id": "w1",
        "pane_id": "w1:p2",
        "terminal_id": "term-2",
        "agent_name": "pa-review",
        "turn": 1,
        "state": "working",
        "answer_path": "/tmp/parent/sub-1/turns/001/answer.md",
        "answer_truncated": False,
        "output": {"text": "working", "source": "recent-unwrapped", "revision": 4, "truncated": False},
    }


class ExternalSubagentTest(unittest.TestCase):
    def test_documented_subagent_signatures_match_both_runtime_facades(self) -> None:
        core_prompt = Path(__file__).resolve().parents[2] / "crates/pa-core/src/prompts/layers/core.md"
        methods = {"spawn", "collect", "subagent_read", "subagent_keys", "subagent_focus", "subagent_stop", "subagent_close"}
        documented = set()
        for declaration in re.findall(r"`rlm\.([^`]+)`", core_prompt.read_text()):
            name = declaration.partition("(")[0]
            if name not in methods or "(" not in declaration:
                continue
            namespace = {}
            exec(f"from __future__ import annotations\nasync def {declaration}:\n    pass\n", namespace)
            expected = tuple((p.name, p.kind, p.default) for p in inspect.signature(namespace[name]).parameters.values())
            for facade in (rlm, rlm.rlm):
                with self.subTest(method=name, facade=type(facade).__name__):
                    function = getattr(facade, name)
                    self.assertTrue(inspect.iscoroutinefunction(function))
                    actual = tuple((p.name, p.kind, p.default) for p in inspect.signature(function).parameters.values())
                    self.assertEqual(actual, expected, declaration)
            documented.add(name)
        self.assertEqual(documented, methods)

    def test_spawn_forwards_kind_and_literal_argv_through_both_facades(self) -> None:
        for spawn in (rlm.spawn, rlm.rlm.spawn):
            payload = {
                "rlm_child_id": "sub-1", "name": "review", "session_dir": "/tmp/parent/sub-1",
                "model": None, "external": external_payload(),
            }
            host = AsyncMock(return_value=payload)
            with patch.object(rlm, "host_request", host):
                handle = asyncio.run(spawn("review code", name="review", agent="codex", agent_args=["-m", "$(literal)"]))
            host.assert_awaited_once_with("rlm.run", {"prompt": "review code", "kwargs": {
                "name": "review", "agent": "codex", "agent_args": ["-m", "$(literal)"],
            }})
            self.assertIsNone(handle.model)
            self.assertEqual(handle.external, rlm.ExternalSubagentInfo(
                agent="codex", workspace_id="w1", pane_id="w1:p2", terminal_id="term-2",
                agent_name="pa-review", turn=1, state="working",
                answer_path=Path("/tmp/parent/sub-1/turns/001/answer.md"), answer_truncated=False,
                output=rlm.ExternalSubagentOutput(text="working", source="recent-unwrapped", revision=4, truncated=False),
            ))

    def test_native_spawn_wire_is_unchanged(self) -> None:
        host = AsyncMock(return_value={"rlm_child_id": "sub-1", "name": "review", "session_dir": "/tmp/sub-1", "model": "p/m"})
        with patch.object(rlm, "host_request", host):
            handle = asyncio.run(rlm.spawn("review", name="review", agent="prime"))
        host.assert_awaited_once_with("rlm.run", {"prompt": "review", "kwargs": {"name": "review"}})
        self.assertIsNone(handle.external)

    def test_worktree_spawn_and_collect_preserve_checkout_through_both_facades(self) -> None:
        external = external_payload()
        external["worktree"] = {"path": "/tmp/checkouts/review", "branch": "task/review", "repo_root": "/tmp/repo"}
        for spawn in (rlm.spawn, rlm.rlm.spawn):
            for worktree in (True, "task/review"):
                payload = {"rlm_child_id": "sub-1", "name": "review", "session_dir": "/tmp/sub-1",
                           "model": None, "external": external}
                host = AsyncMock(return_value=payload)
                with patch.object(rlm, "host_request", host):
                    handle = asyncio.run(spawn("review", name="review", agent="codex", worktree=worktree, base="HEAD~1"))
                host.assert_awaited_once_with("rlm.run", {"prompt": "review", "kwargs": {
                    "name": "review", "agent": "codex", "worktree": worktree, "base": "HEAD~1",
                }})
                self.assertEqual(handle.external.worktree, rlm.ExternalSubagentWorktree(
                    path=Path("/tmp/checkouts/review"), branch="task/review", repo_root=Path("/tmp/repo")))
        host = AsyncMock(return_value={"results": [{"rlm_child_id": "sub-1", "status": "running", "settled": False, "external": external}]})
        with patch.object(rlm, "host_request", host):
            result = asyncio.run(rlm.collect("sub-1"))[0]
        self.assertEqual(result.external.worktree, handle.external.worktree)

    def test_invalid_or_ignored_worktree_requests_fail_explicitly(self) -> None:
        host = AsyncMock()
        for options in (
            {"worktree": True}, {"agent": "prime", "worktree": "task/review"},
            {"agent": "codex", "worktree": 1}, {"agent": "codex", "worktree": " "},
            {"agent": "codex", "worktree": "bad\0branch"},
            {"agent": "codex", "base": "HEAD"},
            {"agent": "codex", "worktree": True, "base": ""},
            {"agent": "codex", "worktree": True, "base": 42},
        ):
            with self.subTest(options=options), patch.object(rlm, "host_request", host):
                with self.assertRaises((ValueError, TypeError)):
                    asyncio.run(rlm.spawn("review", name="review", **options))
        host.assert_not_awaited()
        host.return_value = {"rlm_child_id": "sub-1", "name": "review", "session_dir": "/tmp/sub-1",
                             "model": None, "external": external_payload()}
        with patch.object(rlm, "host_request", host):
            with self.assertRaisesRegex(RuntimeError, "did not create the requested worktree"):
                asyncio.run(rlm.spawn("review", name="review", agent="codex", worktree=True))
        for worktree in ({}, {"path": "relative", "branch": "task/review", "repo_root": "/tmp/repo"},
                         {"path": "/tmp/worktree", "branch": "", "repo_root": "/tmp/repo"}):
            external = external_payload()
            external["worktree"] = worktree
            with self.subTest(worktree=worktree), self.assertRaisesRegex(RuntimeError, "worktree"):
                rlm._external_from_payload({"external": external}, "probe")

    def test_rejects_ignored_external_request(self) -> None:
        host = AsyncMock(return_value={"rlm_child_id": "sub-1", "name": "review", "session_dir": "/tmp/sub-1", "model": "p/m"})
        with patch.object(rlm, "host_request", host):
            with self.assertRaisesRegex(RuntimeError, "did not start the requested external agent"):
                asyncio.run(rlm.spawn("review", name="review", agent="codex"))

    def test_rejects_mixed_native_options_and_invalid_argv_before_dispatch(self) -> None:
        host = AsyncMock()
        for options in (
            {"agent": "codex", "model": "p/m"},
            {"agent": "codex", "thinking": "high"},
            {"agent_args": ["--arg"]},
            {"agent": "codex", "agent_args": "--arg"},
            {"agent": "codex", "agent_args": [42]},
            {"agent": "codex", "agent_args": ["\0"]},
        ):
            with self.subTest(options=options), patch.object(rlm, "host_request", host):
                with self.assertRaises((ValueError, TypeError)):
                    asyncio.run(rlm.spawn("review", name="review", **options))
        host.assert_not_awaited()

    def test_collect_retains_answer_file_truncation_and_notification_failure(self) -> None:
        external = external_payload()
        external.update(answer_truncated=True, notification_error="host queue unavailable")
        host = AsyncMock(return_value={"results": [{
            "rlm_child_id": "sub-1", "status": "done", "settled": True,
            "answer_text": "bounded answer", "external": external,
        }]})
        with patch.object(rlm, "host_request", host):
            result = asyncio.run(rlm.collect("sub-1"))[0]
        self.assertEqual((result.answer_text, result.external.answer_truncated, result.external.notification_error),
                         ("bounded answer", True, "host queue unavailable"))

    def test_invalid_external_metadata_fails_instead_of_returning_native_result(self) -> None:
        for field, value in (("turn", True), ("answer_truncated", 1), ("pane_id", ""), ("output", "screen")):
            external = external_payload()
            external[field] = value
            host = AsyncMock(return_value={"results": [{"rlm_child_id": "sub-1", "status": "done", "settled": True, "external": external}]})
            with self.subTest(field=field), patch.object(rlm, "host_request", host):
                with self.assertRaisesRegex(RuntimeError, "external"):
                    asyncio.run(rlm.collect("sub-1"))
        external = copy.deepcopy(external_payload())
        external["output"]["revision"] = False
        with self.assertRaisesRegex(RuntimeError, "terminal output"):
            rlm._external_from_payload({"external": external}, "probe")

    def test_controls_preserve_target_and_reject_invalid_input(self) -> None:
        host = AsyncMock(return_value={"focused": True})
        handle = rlm.RLMSpawnHandle("sub-1", "review", Path("/tmp/sub-1"), None, rlm._external_from_payload({"external": external_payload()}, "test"))
        with patch.object(rlm, "host_request", host):
            asyncio.run(rlm.rlm.subagent_focus(handle))
        host.assert_awaited_once_with("rlm.subagent.control", {"target": "sub-1", "action": "focus", "keys": [], "lines": 80})
        host.reset_mock()
        with patch.object(rlm, "host_request", host):
            for lines in (False, 0, 2001):
                with self.assertRaises(ValueError):
                    asyncio.run(rlm.subagent_read("review", lines=lines))
            with self.assertRaises(ValueError):
                asyncio.run(rlm.subagent_keys("review"))
            with self.assertRaises(TypeError):
                asyncio.run(rlm.subagent_keys("review", "\0"))
        host.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()
