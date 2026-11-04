#!/usr/bin/env python3
"""Check installed macOS close actions inside an owned verify-else Lume guest."""
import argparse
import importlib.util
import json
from pathlib import Path
import re
import shlex
import time

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("lume", ROOT / ".agents/skills/verify-else/lume.py")
lume = importlib.util.module_from_spec(spec)
spec.loader.exec_module(lume)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", type=Path, required=True)
    parser.add_argument("--app", required=True, help="absolute installed .app path inside the guest")
    parser.add_argument("--service", required=True, help="launchd label of the running fixture API")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    run = lume.Run(args.run)
    manifest = run.read()
    app = Path(args.app)
    lume.require(app.is_absolute() and app.suffix == ".app", "--app must be an absolute .app path")
    executable = str(app / "Contents/MacOS/else-desktop")

    def shell(script):
        guest = lume.Guest(run, lume.live_guest(run, manifest))
        ok, out, err = guest.sh(script, timeout=20)
        lume.require(ok, err or out or "guest shell failed")
        return out.strip()

    def api_pid():
        label = shlex.quote(args.service)
        return shell(f"launchctl print gui/$(id -u)/{label} | awk '$1 == \"pid\" {{print $3}}'")

    original_api = api_pid()
    lume.require(original_api.isdecimal(), "the fixture API must already be running")
    results = []
    for name in ("cmd-w", "close-menu", "cmd-q"):
        evidence = args.output / name
        evidence.mkdir()
        shell(f"open -a {shlex.quote(str(app))}")
        pid = int(shell(f"pgrep -f {shlex.quote('^' + re.escape(executable) + '$')}"))
        mcp = lume.Mcp(run, lume.live_guest(run, manifest))
        window, state = lume.snapshot(mcp, pid, None, evidence, "before")
        if name == "close-menu":
            items = [e for e in state["elements"] if e.get("role") == "AXMenuItem"
                     and e.get("label") == "Close Window"]
            lume.require(len(items) == 1, "expected one Close Window menu item")
            action = mcp.call("click", {"element_token": items[0]["element_token"]})
        else:
            action = mcp.call("press_key", {"target": {"kind": "window", "pid": pid,
                              "window_id": window["window_id"]}, "key": name[-1], "modifiers": ["cmd"]})
        action.pop("images")
        lume.save(evidence / "action.json", action)
        lume.require(not action["is_error"], action["text"])
        deadline = time.monotonic() + 10
        while shell(f"if kill -0 {pid} 2>/dev/null; then echo alive; else echo exited; fi") == "alive":
            if time.monotonic() >= deadline:
                lume.snapshot(mcp, pid, None, evidence, "still-open")
                raise RuntimeError(f"{name} left the installed app running")
            time.sleep(0.2)
        lume.require(api_pid() == original_api, f"{name} changed the API process")
        results.append({"action": name, "desktop_pid": pid, "api_pid": int(original_api), "result": "PASS"})
        lume.save(args.output / "results.json", results)
        print(f"PASS {name}: desktop exited, API {original_api} survived", flush=True)


if __name__ == "__main__":
    main()
