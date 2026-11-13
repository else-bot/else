#!/usr/bin/env python3
"""Replay bot-tool-use cases through Codex code mode and count ALL_TOOLS scans.

The launch mirrors an owner turn's catalog, so else.bot's tools stay deferred
behind ALL_TOOLS, but with a read-only sandbox and no shell. These runs spend
Codex model calls on the host login.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import importlib.util
import re

HERE = Path(__file__).resolve().parent


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


sys.path.insert(0, str(HERE))
run = load("run", HERE / "run.py")

FEATURES_OFF = ["multi_agent", "multi_agent_v2", "shell_snapshot", "apps", "plugins", "remote_plugin",
                "plugin_sharing", "hooks", "browser_use", "browser_use_external", "in_app_browser",
                "in_app_local_automation", "computer_use", "image_generation", "goals", "memories",
                "sleep_tool", "view_image", "tool_suggest", "skill_search",
                "skill_mcp_dependency_install", "workspace_dependencies"]


# An exact-name scan names its tools: `t.name === "..."` or `[...].includes(t.name)`.
# Anything that matches names or descriptions by pattern is broad.
EXACT = re.compile(r"\.name\s*===|\]\s*\.includes\(\s*\w+\.name\s*\)")
BROAD = re.compile(r"\.(test|match|search|startsWith|endsWith|indexOf)\(|\.includes\(\s*['\"`]|description")
# A cell that throws reports "Script error:" first; V8 names JSON in every
# JSON.parse SyntaxError. Outputs may be JSON-encoded, so match either newline.
PARSE_ERROR = re.compile(r"Script error:(?:\n|\\n)SyntaxError: [^\n]{0,200}?JSON")


def toml_str(s):
    return json.dumps(s)


def sample(case, context, out, model, effort):
    out.mkdir(parents=True)
    run.write_json(out / "context.json", context)
    run.write_json(out / "case.json", case)
    home = out / "home"
    home.mkdir()
    os.symlink(Path.home() / ".codex/auth.json", home / "auth.json")
    instructions = context["instructions"] + "\n\n" + context["codex_deferred_tools"]
    tools = ", ".join(toml_str(t["name"]) for t in context["listed_tools"])
    lines = [
        'approval_policy = "never"', 'sandbox_mode = "read-only"', 'web_search = "disabled"',
        "project_doc_max_bytes = 0", "check_for_update_on_startup = false",
        f"model_reasoning_effort = {toml_str(effort)}",
        f"developer_instructions = {toml_str(instructions)}",
        "", "[agents]", "enabled = false", "", "[features]", "shell_tool = false",
        *[f"{f} = false" for f in FEATURES_OFF], "code_mode_host = true", "",
        "[tools]", "update_plan = { enabled = false }",
        "experimental_request_user_input = { enabled = false }", "",
        "[mcp_servers.elsebot]", f"command = {toml_str(sys.executable)}",
        "args = [" + ", ".join(toml_str(a) for a in [
            "-I", str(HERE / "mock_mcp.py"), "--context", str(out / "context.json"),
            "--case", str(out / "case.json"), "--log", str(out / "calls.jsonl")]) + "]",
        f"enabled_tools = [{tools}]", "required = true",
        'default_tools_approval_mode = "approve"', "startup_timeout_sec = 15",
    ]
    (home / "config.toml").write_text("\n".join(lines) + "\n")
    work = Path(tempfile.mkdtemp(prefix="codex-eval-"))
    message = run.packet(case["setup"], context)
    proc = subprocess.run(
        ["codex", "exec", "--json", "--skip-git-repo-check", "--sandbox", "read-only", "-m", model, "-"],
        input=message, cwd=work, capture_output=True, text=True, timeout=600,
        env={**os.environ, "CODEX_HOME": str(home)})
    (out / "stdout.jsonl").write_text(proc.stdout)
    (out / "stderr.txt").write_text(proc.stderr)
    rollout = next(home.glob("sessions/**/rollout-*.jsonl"))
    items = [json.loads(l) for l in rollout.read_text().splitlines()]
    execs, outputs, usage = {}, {}, None
    final = ""
    for it in items:
        p = it.get("payload", {})
        if it.get("type") == "response_item" and p.get("type") == "custom_tool_call":
            execs[p["call_id"]] = p.get("input", "")
        if it.get("type") == "response_item" and p.get("type") == "custom_tool_call_output":
            o = p.get("output")
            outputs[p["call_id"]] = o if isinstance(o, str) else json.dumps(o)
        if it.get("type") == "response_item" and p.get("type") == "message" and p.get("role") == "assistant":
            final = " ".join(c.get("text", "") for c in p.get("content", []))
        if it.get("type") == "event_msg" and p.get("type") == "token_count" and p.get("info"):
            usage = p["info"].get("total_token_usage")
    scans = [cid for cid, code in execs.items() if "ALL_TOOLS" in code]
    exact = [cid for cid in scans if EXACT.search(execs[cid]) and not BROAD.search(execs[cid])]
    scan_bytes = sum(len(outputs.get(cid, "")) for cid in scans)
    parse_errors = sum(bool(PARSE_ERROR.search(o)) for o in outputs.values())
    calls = [json.loads(l) for l in (out / "calls.jsonl").read_text().splitlines()] \
        if (out / "calls.jsonl").exists() else []
    if proc.returncode != 0:
        raise RuntimeError(f"codex exec exited {proc.returncode}; see {out / 'stderr.txt'}")
    checks = run.grade(case["expect"], calls, final, (), context.get("parking_tools"))
    checks.append({"check": "no JSON parse errors in exec cells", "pass": parse_errors == 0})
    row = {"case": case["id"], "pass": all(c["pass"] for c in checks), "parse_errors": parse_errors,
           "execs": len(execs), "scans": len(scans), "exact_scans": len(exact), "scan_bytes": scan_bytes,
           "calls": [c["name"] for c in calls], "errors": sum(bool(c["result"].get("isError")) for c in calls),
           "input": (usage or {}).get("input_tokens"), "cached": (usage or {}).get("cached_input_tokens"),
           "output": (usage or {}).get("output_tokens"),
           "failed": [c["check"] for c in checks if not c["pass"]],
           "scan_code": [execs[c][:200] for c in scans]}
    run.write_json(out / "row.json", row)
    return row


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--api-bin", help="A built else-api; default runs it through Cargo.")
    parser.add_argument("--case", action="append", required=True)
    parser.add_argument("--samples", type=int, default=2)
    parser.add_argument("--model", default="gpt-6.1-sol")
    parser.add_argument("--effort", default="medium")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    # Codex runs in a temporary directory, so every generated path must be absolute.
    args.out = args.out.resolve()
    cases = {c["id"]: c for c in run.load_cases()}
    contexts = {}
    rows = []
    for cid in [c for arg in args.case for c in arg.split(",")]:
        case = cases[cid]
        turn = case.get("turn", "owner")
        if turn not in contexts:
            contexts[turn] = run.export_context(args.api_bin, turn)
        for n in range(args.samples):
            row = sample(case, contexts[turn], args.out / f"{cid}-{n}", args.model, args.effort)
            rows.append(row)
            print(json.dumps(row), flush=True)
            with (args.out / "results.jsonl").open("a") as f:
                f.write(json.dumps(row) + "\n")
    keys = ["execs", "scans", "exact_scans", "scan_bytes", "input", "output"]
    total = {k: sum((r[k] or 0) for r in rows) for k in keys}
    passed = sum(r["pass"] for r in rows)
    print("TOTAL", json.dumps({"samples": len(rows), "pass": passed, **total}))
    return 0 if passed == len(rows) else 1


if __name__ == "__main__":
    sys.exit(main())
