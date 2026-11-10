#!/usr/bin/env python3
"""Compare production native agents-view paths against the read-only TS checkout."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

REPO = Path(__file__).resolve().parents[1]


def run(arguments, **kwargs):
    subprocess.run(arguments, cwd=REPO, check=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ts-reference", type=Path, default=Path.home() / "prime-agent")
    parser.add_argument("--output-dir", type=Path)
    parser.add_argument("--tui-test-binary", type=Path, help="Use an already-built Rust test executable")
    parser.add_argument("--core-test-binary", type=Path, help="Use an already-built Rust test executable")
    args = parser.parse_args()
    output = (args.output_dir or Path(tempfile.mkdtemp(prefix="pa-native-parity-"))).resolve()
    output.mkdir(parents=True, exist_ok=True)
    for filename in ("rust.json", "rust-receipt-target.json", "ts.json", "result.json"):
        (output / filename).unlink(missing_ok=True)
    env = dict(os.environ, DO_NOT_TRACK="1", PA_NATIVE_PARITY_SNAPSHOT=str(output / "rust.json"),
               PA_NATIVE_RECEIPT_SNAPSHOT=str(output / "rust-receipt-target.json"))
    tui = [str(args.tui_test_binary.resolve())] if args.tui_test_binary else ["cargo", "test", "-p", "pa-tui", "--lib", "--"]
    core = [str(args.core_test_binary.resolve())] if args.core_test_binary else ["cargo", "test", "-p", "pa-core", "--lib", "--"]
    run([*tui, "native_agents_view_parity_snapshot", "--ignored"], env=env)
    run([*core, "message_host_handler_round_trip"], env=env)
    run(["bun", "run", "scripts/agents_view_native_ts_snapshot.ts", str(args.ts_reference.resolve()),
         "scripts/fixtures/agents-view-native-parity.json", str(output / "ts.json")], env=env)
    rust = json.loads((output / "rust.json").read_text())
    ts = json.loads((output / "ts.json").read_text())
    receipt = json.loads((output / "rust-receipt-target.json").read_text())
    for key in ("roster", "actions"):
        if rust[key] != ts[key]:
            raise AssertionError(f"Native {key} differs; inspect {output}")
    canonical = lambda value: json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()
    if canonical(receipt) != canonical(ts["nativeReceiptTarget"]):
        raise AssertionError(f"Native receipt target wire differs; inspect {output}")
    fixture = json.loads((REPO / "scripts/fixtures/agents-view-native-parity.json").read_text())
    differences = []
    matched = 0
    for rust_frame, ts_frame in zip(rust["rendered"], ts["rendered"], strict=True):
        if rust_frame["legend"] != ts_frame["legend"]:
            raise AssertionError("Native column header differs")
        for row, rust_line, ts_line in zip(fixture["rows"], rust_frame["rows"], ts_frame["rows"], strict=True):
            rust_line, ts_line = rust_line.rstrip(), ts_line.rstrip()
            if rust_line == ts_line:
                matched += 1
                continue
            # Existing personal-branch indent: Rust four cells/depth, TS two.
            # Accept this precise known difference only; raw frames remain inspectable.
            model_start = rust_line.index(row["model"])
            shifted = rust_line[2:model_start] + "  " + rust_line[model_start:]
            if row["kind"] != "subagent" or row["depth"] != 1 or not rust_line.startswith("    • ") or shifted != ts_line:
                raise AssertionError(f"Unexpected native row rendering difference: {rust_line!r} != {ts_line!r}")
            differences.append({"width": rust_frame["width"], "identity": row["identity"],
                                "difference": "native child indent: Rust4 vs TS2 (existing before feature)"})
    ts_revision = subprocess.check_output(["git", "-C", str(args.ts_reference), "rev-parse", "HEAD"], text=True).strip()
    result = {"ts_revision": ts_revision, "matched_top_level_rows": matched, "matched_native_actions": len(ts["actions"]),
              "matched_roster_entries": len(ts["roster"]), "native_receipt_target_bytes_equal": True,
              "known_rendering_differences": differences,
              "scope": "Text cells at 48/56 columns; native open/reply selection and native receipt target; excludes full-frame styling and Herdr feature"}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    print(f"Evidence: {output}")


if __name__ == "__main__":
    main()
