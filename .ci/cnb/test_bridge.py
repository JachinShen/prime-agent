import copy
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock

spec = importlib.util.spec_from_file_location("bridge", Path(__file__).with_name("bridge.py"))
bridge = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge)


def environment():
    return {
        "GITHUB_REPOSITORY": bridge.REPOSITORY, "GITHUB_REF": bridge.REF,
        "GITHUB_EVENT_NAME": "push", "GITHUB_SHA": "a" * 40,
        "GITHUB_SOURCE_SHA": "a" * 40, "CNB_CI_CONFIG_SHA": "b" * 40,
        "CI_CONTROLLER_SHA": "b" * 40, "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "1",
        "CNB_CI_TOKEN": "trigger-secret", "GITHUB_TOKEN": "job-secret",
        "CNB_TOKEN": "worker-secret", "PRIME_AGENT_STATUS_TOKEN": "status-secret",
        "CNB_REPO_SLUG": "jachinshen/github-ci", "CNB_BRANCH": "main",
        "CNB_EVENT": bridge.OBSERVER_EVENT, "CNB_COMMIT": "b" * 40,
    }


def success():
    return {"status": "success", "pipelinesStatus": {
        str(index): {"name": name, "status": "success", "stages": [
            {"id": key, "name": value, "status": "success"}
            for key, value in {**bridge.STAGES, **bridge.SYSTEM_STAGES}.items()]}
        for index, name in enumerate(sorted(bridge.PIPELINES))}}


class ReleaseBridge(unittest.TestCase):
    def test_dispatch_pins_source_and_controller_and_leaves_result_pending(self):
        call = Mock(return_value={"success": True, "sn": "cnb-test-1"})
        status = Mock()
        self.assertEqual(bridge.dispatch(environment(), call=call, status=status), "cnb-test-1")
        body = call.call_args.args[2]
        self.assertEqual(body, {
            "branch": "main", "sha": "b" * 40, "event": bridge.OBSERVER_EVENT,
            "sync": "false", "title": "Prime Agent release " + "a" * 40,
            "env": {"GITHUB_REPOSITORY": bridge.REPOSITORY, "GITHUB_REF": bridge.REF,
                    "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "1",
                    "GITHUB_SOURCE_SHA": "a" * 40, "CI_CONTROLLER_SHA": "b" * 40}})
        self.assertEqual([entry.args[3] for entry in status.call_args_list], ["pending"])

    def test_failed_or_unconfirmed_dispatch_is_not_retried_and_reports_error(self):
        for response in ({"success": False}, {"success": True, "sn": "https://untrusted.invalid"}):
            call, status = Mock(return_value=response), Mock()
            with self.subTest(response=response), self.assertRaises(bridge.BridgeError):
                bridge.dispatch(environment(), call=call, status=status)
            self.assertEqual(call.call_count, 1)
            self.assertEqual([entry.args[3] for entry in status.call_args_list], ["pending", "error"])

    def test_untrusted_events_and_missing_configuration_never_dispatch(self):
        for key, value in (("GITHUB_REF", "refs/heads/main"), ("GITHUB_REPOSITORY", "other/fork"),
                           ("GITHUB_EVENT_NAME", "pull_request"), ("CNB_CI_CONFIG_SHA", "bad"),
                           ("GITHUB_SHA", "0" * 40), ("GITHUB_RUN_ID", "0"), ("CNB_CI_TOKEN", "")):
            env = environment()
            env.pop("GITHUB_SOURCE_SHA")
            env.pop("CI_CONTROLLER_SHA")
            env[key] = value
            call, status = Mock(), Mock()
            with self.subTest(key=key), self.assertRaises(bridge.BridgeError):
                bridge.dispatch(env, call=call, status=status)
            call.assert_not_called()

    def test_both_architectures_and_all_stages_must_pass(self):
        self.assertTrue(bridge.classify(success()))
        self.assertFalse(bridge.classify({"status": "pending"}))
        for mutate in (
            lambda result: result.pop("status"),
            lambda result: result["pipelinesStatus"].pop("1"),
            lambda result: result["pipelinesStatus"]["0"].update(name="unexpected"),
            lambda result: result["pipelinesStatus"]["0"].update(status="pending"),
            lambda result: result["pipelinesStatus"]["0"]["stages"].pop(2),
            lambda result: result["pipelinesStatus"]["0"]["stages"][2].update(status="skipped"),
            lambda result: result["pipelinesStatus"]["0"]["stages"].append({"id": "unknown"}),
            lambda result: result["pipelinesStatus"]["0"]["stages"].append(copy.deepcopy(result["pipelinesStatus"]["0"]["stages"][0])),
            lambda result: result["pipelinesStatus"].update({"0": None}),
            lambda result: result["pipelinesStatus"]["0"]["stages"].append(None),
        ):
            result = success()
            mutate(result)
            with self.subTest(result=result), self.assertRaises(bridge.BridgeError):
                bridge.classify(result)

    def test_observer_checks_status_credential_before_dispatch_and_reports_verified_success(self):
        status = Mock()
        call = Mock(side_effect=[{"success": True, "sn": "cnb-worker-1"}, {"status": "pending"}, success()])
        sleeps = Mock()
        self.assertEqual(bridge.observe(environment(), call=call, status=status, sleep=sleeps), "cnb-worker-1")
        self.assertEqual([entry.args[3] for entry in status.call_args_list], ["pending", "pending", "success"])
        self.assertEqual(sleeps.call_count, 1)
        self.assertEqual(call.call_args_list[0].args[2]["env"], {
            "GITHUB_REPOSITORY": bridge.REPOSITORY, "GITHUB_REF": bridge.REF,
            "GITHUB_SOURCE_SHA": "a" * 40, "CI_CONTROLLER_SHA": "b" * 40})

        call = Mock()
        with self.assertRaises(bridge.BridgeError):
            bridge.observe(environment(), call=call, status=Mock(side_effect=bridge.BridgeError("credential rejected")))
        call.assert_not_called()

    def test_worker_failure_invalid_results_and_timeout_never_report_success(self):
        for response, expected in (({"status": "error"}, "failure"), ({"status": "cancel"}, "failure"),
                                   ({"status": "unknown"}, "error"), (None, "error")):
            call = Mock(side_effect=[{"success": True, "sn": "cnb-worker-1"}, response])
            status = Mock()
            with self.subTest(response=response), self.assertRaises(bridge.BridgeError):
                bridge.observe(environment(), call=call, status=status)
            self.assertEqual(status.call_args.args[3], expected)
        status = Mock()
        with self.assertRaises(bridge.BridgeError):
            bridge.observe(environment(), call=Mock(return_value={"success": True, "sn": "cnb-worker-1"}),
                           status=status, monotonic=Mock(side_effect=[0, 7000]))
        self.assertEqual(status.call_args.args[3], "error")

    def test_observer_rejects_an_unpinned_controller(self):
        env = environment()
        env["CNB_COMMIT"] = "c" * 40
        call = Mock()
        with self.assertRaises(bridge.BridgeError):
            bridge.observe(env, call=call)
        call.assert_not_called()

    def test_conflicting_identity_fields_fail_explicitly(self):
        for key in ("GITHUB_SHA", "CNB_CI_CONFIG_SHA"):
            env = environment()
            env[key] = "c" * 40
            with self.subTest(key=key), self.assertRaises(bridge.BridgeError):
                bridge.identity(env)

    def test_retries_have_separate_status_contexts(self):
        first = environment()
        second = {**first, "GITHUB_RUN_ATTEMPT": "2"}
        self.assertNotEqual(bridge.identity(first)[2], bridge.identity(second)[2])


if __name__ == "__main__":
    unittest.main()
