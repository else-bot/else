#!/usr/bin/env python3
"""Bounded subscription inference. Explicit --infer is required. No global config edits."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import threading
import time
from fixture import context

ROOT = Path(__file__).resolve().parent
SCRATCH = Path("/tmp/elsebot-research-prompts")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--infer", action="store_true", help="Use the signed-in Codex subscription")
    parser.add_argument("--case", action="append")
    parser.add_argument("--cases", type=Path, default=ROOT / "cases.json")
    parser.add_argument("--prompt", type=Path, default=ROOT / "prompt.txt")
    parser.add_argument("--output", type=Path, default=ROOT / "results")
    args = parser.parse_args()
    if not args.infer:
        parser.error("Pass --infer to authorize subscription inference")
    cases = json.loads(args.cases.read_text())
    selected = [case for case in cases if not args.case or case["id"] in args.case]
    if not selected:
        parser.error("No matching cases")
    if any((args.output / case["id"]).exists() for case in selected):
        parser.error("Case output already exists. Choose a fresh --output directory to preserve evidence.")
    args.output.mkdir(parents=True, exist_ok=True)
    SCRATCH.mkdir(parents=True, exist_ok=True)
    results = []
    batch_start = time.monotonic()
    for case in selected:
        if time.monotonic() - batch_start >= 720:
            results.append({"case": case["id"], "pass": False, "error": "720 second batch limit"})
            break
        out = args.output / case["id"]
        out.mkdir(exist_ok=True)
        ctx = context(case["bot"], **case.get("overrides", {}))
        (out / "context.json").write_text(json.dumps(ctx, indent=2) + "\n")
        visible = {k: v for k, v in ctx.items() if k not in case.get("hidden_context_keys", [])}
        role = next(bot["role"] for bot in ctx["registry"] if bot["id"] == case["bot"])
        prompt = args.prompt.read_text().format(bot_id=case["bot"], role=role, context=json.dumps(visible))
        (out / "developer.txt").write_text(prompt)
        (out / "user.txt").write_text(case["request"] + "\n")
        cmd = ["codex", "--no-daemon", "-a", "never", "exec", "--ignore-user-config", "--ignore-rules", "--ephemeral",
               "--strict-config", "--skip-git-repo-check", "-C", str(SCRATCH), "-s", "read-only", "-m", "gpt-6.1-sol", "--json"]
        config = {"model_reasoning_effort": "high", "service_tier": "priority", "web_search": "disabled",
                  "project_doc_max_bytes": 0, "developer_instructions": prompt,
                  "features.shell_tool": False, "features.apps": False, "features.plugins": False,
                  "features.hooks": False, "features.multi_agent": False, "features.browser_use": False,
                  "features.computer_use": False, "features.image_generation": False,
                  "mcp_servers.elsebot.command": "python3",
                  "mcp_servers.elsebot.default_tools_approval_mode": "approve",
                  "mcp_servers.elsebot.args": [str(ROOT / "fixture.py"), str((out / "context.json").resolve()), str((out / "tools.jsonl").resolve())],
                  "mcp_servers.elsebot.startup_timeout_sec": 10,
                  "mcp_servers.elsebot.tool_timeout_sec": 10}
        for key, value in config.items():
            cmd += ["-c", key + "=" + json.dumps(value)]
        cmd += ["-o", str((out / "final.txt").resolve()), "-"]
        (out / "invocation.json").write_text(json.dumps(cmd, indent=2) + "\n")
        # Force supported subscription auth, and avoid identifying as the parent session.
        env = {k: v for k, v in os.environ.items() if k not in {"OPENAI_API_KEY", "CODEX_THREAD_ID"}}
        started = time.monotonic()
        filtered = {"reasoning_events":0,"unparsed_events":0}
        with (out / "events.jsonl").open("w") as stdout, (out / "stderr.txt").open("w") as stderr:
            process = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, env=env, start_new_session=True)
            def record_events():
                for line in process.stdout:
                    try:
                        event = json.loads(line)
                    except (json.JSONDecodeError, UnicodeDecodeError):
                        filtered["unparsed_events"] += 1
                        continue
                    kind = str(event.get("type", "")) + " " + str(event.get("item", {}).get("type", ""))
                    if "reasoning" in kind.lower():
                        filtered["reasoning_events"] += 1
                        continue
                    stdout.write(json.dumps(event) + "\n")
                    stdout.flush()
            recorder = threading.Thread(target=record_events, daemon=True)
            recorder.start()
            timed_out = False
            process.stdin.write(case["request"].encode())
            process.stdin.close()
            try:
                process.wait(timeout=55)
            except subprocess.TimeoutExpired:
                timed_out = True
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
            recorder.join(timeout=3)
        calls = [json.loads(line) for line in (out / "tools.jsonl").read_text().splitlines()] if (out / "tools.jsonl").exists() else []
        events = [json.loads(line) for line in (out / "events.jsonl").read_text().splitlines() if line.startswith("{")]
        reply = (out / "final.txt").read_text() if (out / "final.txt").exists() else ""
        names = [call["request"]["name"] for call in calls]
        checks = {"process_ok": process.returncode == 0 and not timed_out,
                  "expected_tools": names == case["expect_tools"],
                  "has_reply": bool(reply.strip()),
                  "reply_terms": all(term.lower() in reply.lower() for term in case.get("reply_contains", [])),
                  "reply_exclusions": all(term.lower() not in reply.lower() for term in case.get("reply_not_contains", [])),
                  "target": not case.get("target") or bool(calls) and calls[0]["request"]["arguments"].get("target_bot_id") == case["target"],
                  "rejection": not case.get("expect_code") or bool(calls) and calls[-1]["response"].get("code") == case["expect_code"],
                  "status": not case.get("expect_status") or bool(calls) and calls[-1]["response"].get("status") == case["expect_status"],
                  "no_native_command": not any(event.get("item", {}).get("type") == "command_execution" for event in events),
                  "turn_completed": any(event.get("type") == "turn.completed" for event in events)}
        result = {"case": case["id"], "pass": all(checks.values()), "checks": checks, "actual_tools": names,
                  "seconds": round(time.monotonic() - started, 3), "returncode": process.returncode, "timeout": timed_out,
                  "filtered_output":filtered,
                  "usage": [e.get("usage") for e in events if e.get("type") == "turn.completed"]}
        (out / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        results.append(result)
        print(json.dumps(result), flush=True)
    summary = {"model_requested": "gpt-6.1-sol", "reasoning_requested": "high", "tier_requested": "priority",
               "per_case_timeout_seconds": 55, "batch_limit_seconds": 720,
               "elapsed_seconds": round(time.monotonic() - batch_start, 3), "results": results}
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")


if __name__ == "__main__":
    main()
