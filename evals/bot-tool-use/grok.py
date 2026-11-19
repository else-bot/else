#!/usr/bin/env python3
"""Replay bot-tool-use cases through Grok CLI's headless print mode.

The launch mirrors providers/grok.rs without host tools: Grok's own
search_tool and use_tool are its only tools, and else.bot's MCP tools are
reached through them. Grok refuses a tool name its catalog lacks before the
mock server sees the call, so those refusals are graded as rejected calls.
These runs spend Grok model calls on the host login (`~/.grok/auth.json`).
"""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import uuid

HERE = Path(__file__).resolve().parent


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


sys.path.insert(0, str(HERE))
run = load("run", HERE / "run.py")

CONFIG = """[cli]
auto_update = false
[features]
managed_config = false
campaigns = false
[subagents]
enabled = false
[workflows]
enabled = false
[memory]
enabled = false
[memory_v2]
enabled = false
[permission]
allow = ["MCPTool(elsebot__*)", "MCPTool(mcp__elsebot__*)"]
{deny}
[mcp_servers.elsebot]
command = {command}
args = {args}
startup_timeout_sec = 30
"""

# grok_home::HOST_TOOLS, the --tools names a task with host tools adds.
HOST_TOOLS = "run_terminal_cmd,read_file,write,search_replace,list_dir,grep,get_task_output,kill_task"

ENV = {"GROK_DISABLE_AUTOUPDATER": "1", "GROK_SUBAGENTS": "0", "GROK_WORKFLOWS": "0",
       "GROK_MEMORY": "0", "GROK_BACKEND_SEARCH": "0", "GROK_CAMPAIGNS": "0",
       "GROK_DISABLE_EXPERIMENTS": "1"}


def transcript(stdout):
    """Grok's tool calls, their refusals, and its final text, from stream JSON."""
    uses, refused, final, completed = {}, [], "", False
    for line in stdout.splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        content = (event.get("message") or {}).get("content")
        if event.get("type") == "assistant" and isinstance(content, list):
            for block in content:
                if block.get("type") == "tool_use":
                    uses[block["id"]] = block
        if event.get("type") == "user" and isinstance(content, list):
            for block in content:
                text = json.dumps(block.get("content"))
                if block.get("is_error") and ("Tool not found" in text or "not a valid MCP tool" in text):
                    use = uses.get(block.get("tool_use_id"), {})
                    refused.append(use.get("input", {}).get("tool_name") or use.get("name"))
        if event.get("type") == "result":
            # Production completes a turn only on this terminal (grok/protocol.rs).
            completed = (event.get("subtype") == "success" and event.get("is_error") is False
                         and event.get("stop_reason") == "end_turn")
            final = event.get("result") if isinstance(event.get("result"), str) else ""
    return list(uses.values()), refused, final, completed


def sample(case, context, out, model, effort, host_tools):
    out.mkdir(parents=True)
    run.write_json(out / "context.json", context)
    run.write_json(out / "case.json", case)
    home = out / "home"
    home.mkdir()
    os.symlink(Path.home() / ".grok/auth.json", home / "auth.json")
    args = ["-I", str(HERE / "mock_mcp.py"), "--context", str(out / "context.json"),
            "--case", str(out / "case.json"), "--log", str(out / "calls.jsonl")]
    (home / "config.toml").write_text(CONFIG.format(command=json.dumps(sys.executable),
                                                    args=json.dumps(args),
                                                    deny="" if host_tools else 'deny = ["Read(**)"]'))
    (home / "prompt.txt").write_text(run.packet(case["setup"], context))
    system = f"{context['instructions']}\n{context['grok_tool_note']}"
    work = Path(tempfile.mkdtemp(prefix="grok-eval-"))
    try:
        proc = subprocess.run(
            ["grok", "--output-format", "streaming-messages-json", "--model", model,
             "--reasoning-effort", effort, "--tools",
             "search_tool,use_tool" + ("," + HOST_TOOLS if host_tools else ""), "--no-subagents",
             "--no-plan", "--disable-web-search", "--system-prompt-override", system,
             "--prompt-file", str(home / "prompt.txt"), "--session-id", str(uuid.uuid4()),
             *(["--always-approve"] if host_tools else ["--permission-mode", "dontAsk"])],
            cwd=work, capture_output=True, text=True, timeout=600,
            env={**os.environ, "GROK_HOME": str(home), **ENV})
    finally:
        shutil.rmtree(work, ignore_errors=True)
    (out / "stdout.jsonl").write_text(proc.stdout)
    (out / "stderr.txt").write_text(proc.stderr)
    if proc.returncode != 0:
        raise RuntimeError(f"grok exited {proc.returncode}; see {out / 'stderr.txt'}")
    uses, refused, final, completed = transcript(proc.stdout)
    calls = [json.loads(l) for l in (out / "calls.jsonl").read_text().splitlines()] \
        if (out / "calls.jsonl").exists() else []
    checks = run.grade(case["expect"], calls, final, refused, context.get("parking_tools"))
    checks.append({"check": "turn completed with end_turn", "pass": completed})
    # Production's tracker ends the turn on a use_tool call whose target it cannot read.
    checks.append({"check": "every use_tool names its tool", "pass": all(
        isinstance(u.get("input", {}).get("tool_name"), str) for u in uses if u.get("name") == "use_tool")})
    row = {"case": case["id"], "pass": all(c["pass"] for c in checks),
           "calls": [c["name"] for c in calls], "refused": refused,
           "grok_calls": [u.get("input", {}).get("tool_name") or u.get("name") for u in uses],
           "failed": [c["check"] for c in checks if not c["pass"]], "final": final[:400]}
    run.write_json(out / "row.json", row)
    return row


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--api-bin", help="A built else-api; default runs it through Cargo.")
    parser.add_argument("--case", action="append", required=True)
    parser.add_argument("--samples", type=int, default=2)
    parser.add_argument("--model", default="grok-4.7")
    parser.add_argument("--effort", default="medium")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--host-tools", action="store_true",
                        help="Launch as a task with host tools does: Grok's file tools and --always-approve.")
    args = parser.parse_args()
    args.out = args.out.resolve()
    cases = {c["id"]: c for c in run.load_cases()}
    contexts, rows = {}, []
    for cid in [c for arg in args.case for c in arg.split(",")]:
        case = cases[cid]
        turn = case.get("turn", "owner")
        if turn not in contexts:
            contexts[turn] = run.export_context(args.api_bin, turn)
        for n in range(args.samples):
            row = sample(case, contexts[turn], args.out / f"{cid}-{n}", args.model, args.effort, args.host_tools)
            rows.append(row)
            print(json.dumps(row), flush=True)
            with (args.out / "results.jsonl").open("a") as f:
                f.write(json.dumps(row) + "\n")
    passed = sum(r["pass"] for r in rows)
    print("TOTAL", json.dumps({"samples": len(rows), "pass": passed}))
    return 0 if passed == len(rows) else 1


if __name__ == "__main__":
    sys.exit(main())
