#!/usr/bin/env python3
"""Read model metadata through the installed app-server. Never reads auth files."""
import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import time

ROOT = Path(__file__).resolve().parent


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--infer", action="store_true", help="Run one subscription-backed tier verification turn")
    opts = parser.parse_args()
    cmd = ["codex", "app-server", "--stdio"]
    for feature in ["apps", "plugins", "hooks", "multi_agent", "shell_tool", "browser_use", "computer_use"]:
        cmd += ["-c", "features." + feature + "=false"]
    result = {"version": subprocess.check_output(["codex", "--version"], text=True).strip(), "command": cmd}
    inbox = queue.Queue()
    env = {k:v for k,v in os.environ.items() if k not in {"OPENAI_API_KEY", "CODEX_THREAD_ID"}}
    with (ROOT / "probe.stderr.txt").open("w") as stderr:
        process = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True, env=env)
        def reader():
            for line in process.stdout:
                inbox.put(json.loads(line))
        threading.Thread(target=reader, daemon=True).start()
        def send(value):
            process.stdin.write(json.dumps(value) + "\n")
            process.stdin.flush()
        def response(request_id):
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                item = inbox.get(timeout=max(0.01, deadline - time.monotonic()))
                if item.get("id") == request_id:
                    return item
            raise TimeoutError("app-server response")
        try:
            send({"id":1,"method":"initialize","params":{"clientInfo":{"name":"elsebot_prompts_research","version":"1"}}})
            result["initialize"] = response(1)
            send({"method":"initialized","params":{}})
            send({"id":2,"method":"model/list","params":{"includeHidden":True,"limit":100}})
            models = response(2)
            result["model_list_request"] = {"includeHidden":True,"limit":100}
            result["requested_model_entries"] = [m for m in models.get("result",{}).get("data",[]) if m.get("model") == "gpt-6.1-sol"]
            result["next_cursor"] = models.get("result",{}).get("nextCursor")
            if "error" in models:
                result["error"] = models["error"]
            if opts.infer:
                start = {"model":"gpt-6.1-sol", "serviceTier":"priority", "cwd":"/tmp/elsebot-research-prompts",
                         "approvalPolicy":"never", "sandbox":"read-only", "ephemeral":True,
                         "developerInstructions":"Synthetic tier verification. Return exactly SYNTHETIC_TIER_READY. Do not call tools or read files.",
                         "config":{"model_reasoning_effort":"high", "project_doc_max_bytes":0, "mcp_servers":{}}}
                result["thread_start_request"] = start
                send({"id":3,"method":"thread/start","params":start})
                result["thread_start_response"] = response(3)
                thread_id = result["thread_start_response"]["result"]["thread"]["id"]
                turn = {"threadId":thread_id,"model":"gpt-6.1-sol","effort":"high","serviceTier":"priority",
                        "input":[{"type":"text","text":"Return exactly SYNTHETIC_TIER_READY. Use no tools."}]}
                result["turn_start_request"] = turn
                send({"id":4,"method":"turn/start","params":turn})
                result["turn_start_response"] = response(4)
                deadline = time.monotonic() + 40
                result["turn_events"] = []
                while time.monotonic() < deadline:
                    item = inbox.get(timeout=max(0.01, deadline - time.monotonic()))
                    # Deny any unexpected server request. No tool dispatch is implemented.
                    if "id" in item and "method" in item:
                        send({"id":item["id"],"error":{"code":-32000,"message":"Tools disabled for synthetic tier verification"}})
                        result["turn_events"].append({"unexpected_request_method":item["method"]})
                    elif item.get("method") in {"turn/started","turn/completed"}:
                        turn_value = item.get("params", {}).get("turn", {})
                        if "items" in turn_value:
                            turn_value["items"] = [entry for entry in turn_value["items"] if entry.get("type") in {"userMessage","agentMessage"}]
                        result["turn_events"].append(item)
                    elif item.get("method") == "item/completed" and item.get("params", {}).get("item", {}).get("type") in {"userMessage","agentMessage"}:
                        result["turn_events"].append(item)
                    if item.get("method") == "turn/completed":
                        break
        except Exception as error:
            result["error"] = type(error).__name__ + ": " + str(error)
        finally:
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
    (ROOT / ("tier-probe.json" if opts.infer else "model-probe.json")).write_text(json.dumps(result,indent=2) + "\n")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
