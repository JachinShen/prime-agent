#!/usr/bin/env python3
"""Dispatch GitHub release builds; verify and report their results from CNB."""
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

REPOSITORY = "JachinShen/prime-agent"
REF = "refs/heads/personal-release"
API = "https://api.cnb.cool/jachinshen/github-ci/-/build"
WEB = "https://cnb.cool/jachinshen/github-ci/-/build/logs/"
OBSERVER_EVENT = "api_trigger_prime_agent_release"
WORKER_EVENT = "api_trigger_prime_agent_release_build"
STAGES = {"stage-0": "Fetch exact GitHub source",
          "stage-1": "Build and verify release archive",
          "stage-2": "Upload verified build artifacts"}
SYSTEM_STAGES = {"prepare": "Prepare", "beforeEnd": "BeforeEnd",
                 "debugDetection": "DebugDetection", "end": "End"}
PIPELINES = {"Prime Agent Linux x64", "Prime Agent Linux arm64"}


class BridgeError(Exception):
    pass


class BuildFailed(BridgeError):
    pass


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise BridgeError("API redirect refused")


def sha(value):
    if not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{40}", value) or value == "0" * 40:
        raise BridgeError("Expected a full nonzero commit SHA")
    return value


def request(url, token, body=None):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, headers={
        "Authorization": "Bearer " + token, "Content-Type": "application/json",
        "Accept": "application/json", "User-Agent": "prime-agent-cnb-release"})
    try:
        with urllib.request.build_opener(NoRedirect).open(req, timeout=20) as response:
            result = json.load(response)
    except urllib.error.HTTPError as error:
        raise BridgeError(f"API HTTP {error.code}; request was not retried") from None
    except (urllib.error.URLError, TimeoutError, ValueError):
        raise BridgeError("API request failed or returned invalid JSON; inspect before retrying") from None
    if not isinstance(result, dict):
        raise BridgeError("Expected an API response object")
    return result


def identity(env):
    if env.get("GITHUB_REPOSITORY") != REPOSITORY or env.get("GITHUB_REF") != REF:
        raise BridgeError("Expected the Prime Agent personal-release branch")
    source = sha(env.get("GITHUB_SOURCE_SHA") or env.get("GITHUB_SHA"))
    controller = sha(env.get("CI_CONTROLLER_SHA") or env.get("CNB_CI_CONFIG_SHA"))
    if (env.get("GITHUB_SHA") and env.get("GITHUB_SOURCE_SHA") and env["GITHUB_SHA"] != source
            or env.get("CNB_CI_CONFIG_SHA") and env.get("CI_CONTROLLER_SHA") and env["CNB_CI_CONFIG_SHA"] != controller):
        raise BridgeError("Conflicting source or controller identities")
    ids = [env.get("GITHUB_RUN_ID"), env.get("GITHUB_RUN_ATTEMPT")]
    if any(not isinstance(value, str) or not re.fullmatch(r"[1-9][0-9]{0,19}", value) for value in ids):
        raise BridgeError("Expected a positive GitHub run ID and attempt")
    return source, controller, f"cnb/release/{ids[0]}/{ids[1]}"


def write_status(token, source, context, state, target, call=request):
    if state not in ("pending", "success", "failure", "error"):
        raise BridgeError("Invalid GitHub commit status")
    descriptions = {"pending": "Waiting for verified CNB release builds",
                    "success": "Both Linux release archives built, verified and uploaded",
                    "failure": "CNB release build failed or was cancelled",
                    "error": "CNB release result could not be verified"}
    result = call(f"https://api.github.com/repos/{REPOSITORY}/statuses/{sha(source)}", token,
                  {"context": context, "state": state,
                   "description": descriptions[state], "target_url": target})
    if result.get("state") != state or result.get("context") != context:
        raise BridgeError("GitHub did not confirm the requested status")


def start(token, controller, event, env, call=request):
    result = call(API + "/start", token, {
        "branch": "main", "sha": sha(controller), "event": event, "sync": "false",
        "title": "Prime Agent release " + sha(env["GITHUB_SOURCE_SHA"]), "env": env})
    sn = result.get("sn")
    if result.get("success") is not True or not isinstance(sn, str) or not re.fullmatch(r"cnb-[A-Za-z0-9-]+", sn):
        raise BridgeError("CNB did not confirm dispatch acceptance")
    return sn


def dispatch(env, call=request, status=write_status):
    if env.get("GITHUB_EVENT_NAME") not in ("push", "workflow_dispatch"):
        raise BridgeError("Only trusted push/manual events may dispatch builds")
    source, controller, context = identity(env)
    cnb_token, github_token = env.get("CNB_CI_TOKEN"), env.get("GITHUB_TOKEN")
    if not cnb_token or not github_token:
        raise BridgeError("CNB_CI_TOKEN and GITHUB_TOKEN must be configured")
    target = f"https://github.com/{REPOSITORY}/actions/runs/{env['GITHUB_RUN_ID']}"
    status(github_token, source, context, "pending", target)
    worker_env = {key: env[key] for key in ("GITHUB_REPOSITORY", "GITHUB_REF", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT")}
    worker_env.update(GITHUB_SOURCE_SHA=source, CI_CONTROLLER_SHA=controller)
    try:
        sn = start(cnb_token, controller, OBSERVER_EVENT, worker_env, call=call)
    except BridgeError:
        status(github_token, source, context, "error", target)
        raise
    print(f"DISPATCHED: {WEB}{sn}; result status: {context}")
    return sn


def classify(result):
    if not isinstance(result, dict):
        raise BridgeError("Invalid CNB status response")
    state = result.get("status")
    if state in ("error", "cancel"):
        raise BuildFailed("CNB release builds " + state)
    if state == "pending":
        return False
    if state != "success":
        raise BridgeError("Unknown CNB result; refusing to report success")
    pipelines = result.get("pipelinesStatus")
    if not isinstance(pipelines, dict) or len(pipelines) != len(PIPELINES):
        raise BridgeError("Expected both native release build pipelines")
    seen = set()
    for pipeline in pipelines.values():
        if not isinstance(pipeline, dict):
            raise BridgeError("Invalid release pipeline")
        name = pipeline.get("name")
        if not isinstance(name, str) or name not in PIPELINES or name in seen or pipeline.get("status") != "success":
            raise BridgeError("Missing, duplicate or unsuccessful release pipeline")
        seen.add(name)
        stages = pipeline.get("stages")
        if not isinstance(stages, list):
            raise BridgeError("Missing release stages")
        found = set()
        expected = {**STAGES, **SYSTEM_STAGES}
        for stage in stages:
            if not isinstance(stage, dict):
                raise BridgeError("Invalid release stage")
            stage_id = stage.get("id")
            if not isinstance(stage_id, str) or stage_id not in expected or stage_id in found or stage.get("name") != expected[stage_id]:
                raise BridgeError("Unknown, duplicate or mismatched release stage")
            allowed = ("success", "skipped") if stage_id == "debugDetection" else ("success",)
            if stage.get("status") not in allowed:
                raise BridgeError("Unsuccessful release stage: " + stage_id)
            found.add(stage_id)
        if not STAGES.keys() <= found:
            raise BridgeError("Build, verification and upload must all succeed")
    return True


def observe(env, call=request, status=write_status, sleep=time.sleep, monotonic=time.monotonic):
    source, controller, context = identity(env)
    if (env.get("CNB_REPO_SLUG") != "jachinshen/github-ci" or env.get("CNB_BRANCH") != "main"
            or env.get("CNB_EVENT") != OBSERVER_EVENT or sha(env.get("CNB_COMMIT")) != controller):
        raise BridgeError("Expected the pinned CNB release observer")
    cnb_token, github_token = env.get("CNB_TOKEN"), env.get("PRIME_AGENT_STATUS_TOKEN")
    if not cnb_token or not github_token:
        raise BridgeError("Observer requires CNB_TOKEN and PRIME_AGENT_STATUS_TOKEN")
    target = f"https://github.com/{REPOSITORY}/actions/runs/{env['GITHUB_RUN_ID']}"
    worker_env = {key: env[key] for key in ("GITHUB_REPOSITORY", "GITHUB_REF", "GITHUB_SOURCE_SHA", "CI_CONTROLLER_SHA")}
    # Verify the persistent status credential before starting expensive builds.
    status(github_token, source, context, "pending", target)
    try:
        sn = start(cnb_token, controller, WORKER_EVENT, worker_env, call=call)
        target = WEB + sn
        print("CNB release builds: " + target, flush=True)
        status(github_token, source, context, "pending", target)
        deadline = monotonic() + 110 * 60
        while monotonic() < deadline:
            if classify(call(API + "/status/" + sn, cnb_token)):
                status(github_token, source, context, "success", target)
                return sn
            sleep(10)
        raise BridgeError("CNB release builds timed out; inspect the worker before retrying")
    except BridgeError as error:
        status(github_token, source, context, "failure" if isinstance(error, BuildFailed) else "error", target)
        raise


if __name__ == "__main__":
    try:
        if sys.argv[1:] == ["dispatch"]:
            dispatch(os.environ)
        elif sys.argv[1:] == ["observe"]:
            observe(os.environ)
        else:
            raise BridgeError("Usage: bridge.py dispatch|observe")
    except BridgeError as error:
        print("CNB release bridge failed: " + str(error), file=sys.stderr)
        sys.exit(1)
