#!/usr/bin/env python3
"""Repeat compiled API tests concurrently, retaining every result without retries."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import time


TARGETS = {
    "tool_permissions": [],
    "ws": ["ended_channels_do_not_use_up_the_subscription_limit", "--exact"],
    "owner_questions": [],
    "email_api": [],
    "reset": [],
    "turns": [],
    "else_api": ["mcp::transport::tests::stdio_shutdown_and_drop_terminate_launcher_descendants", "--exact"],
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build", type=Path, required=True, help="cargo test --no-run --message-format=json output")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--iterations", type=int, default=20)
    parser.add_argument("--jobs", type=int, default=6)
    parser.add_argument("--target", action="append", choices=TARGETS)
    args = parser.parse_args()
    targets = args.target or list(TARGETS)
    binaries = {}
    for line in args.build.read_text().splitlines():
        entry = json.loads(line)
        if entry.get("reason") == "compiler-artifact" and entry.get("profile", {}).get("test") and entry.get("executable"):
            binaries[entry["target"]["name"]] = entry["executable"]
    missing = set(targets) - binaries.keys()
    if missing:
        parser.error(f"missing test binaries: {sorted(missing)}")
    args.output.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ, TOKIO_WORKER_THREADS="2")

    def run(work):
        target, iteration = work
        command = [binaries[target], *TARGETS[target], "--test-threads=4", "--nocapture"]
        start = time.monotonic()
        try:
            result = subprocess.run(command, capture_output=True, env=env, timeout=180)
            code, output = result.returncode, result.stdout + result.stderr
        except subprocess.TimeoutExpired as error:
            code, output = 124, (error.stdout or b"") + (error.stderr or b"")
        log = args.output / f"{target}-{iteration:03}.log"
        log.write_bytes(output)
        if b"0 passed; 0 failed" in output or b"test result:" not in output:
            code = code or 125
        record = {"target": target, "iteration": iteration, "exit": code,
                  "seconds": round(time.monotonic() - start, 3), "log": str(log), "command": command}
        print(json.dumps(record), flush=True)
        return record

    work = [(target, iteration) for iteration in range(1, args.iterations + 1) for target in targets]
    with ThreadPoolExecutor(max_workers=args.jobs) as executor:
        records = list(executor.map(run, work))
    (args.output / "results.json").write_text(json.dumps(records, indent=2) + "\n")
    summary = {target: {"runs": sum(r["target"] == target for r in records),
                        "failures": sum(r["target"] == target and r["exit"] != 0 for r in records)}
               for target in targets}
    print(json.dumps({"summary": summary}), flush=True)
    return int(any(r["exit"] != 0 for r in records))


if __name__ == "__main__":
    raise SystemExit(main())
