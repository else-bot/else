"""Real-guest gates for the patched x11vnc, driven through the real API and the
real Rust desktop client (the temporary probe crate).

Usage: PR62_BOT=<bot> python3 gates.py setup | gate1 N | gate2 | gate3 END
Every result is appended to results.jsonl next to this file. Values typed
are fixture text, never secrets.
"""
import base64
import json
import os
import shlex
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
REPO = ROOT.parents[1]
sys.path.insert(0, str(ROOT))
from api import request  # noqa: E402

BOT = os.environ["PR62_BOT"]
PROBE = Path("/private/tmp/else62-keymap-probe-fd6449")
MSB_HOME = "/private/tmp/else62-arm64-fd6449/microsandbox"
SUP = "supervisorctl -c /etc/elsebot/supervisord.conf"
PAGE = "http://127.0.0.1:8421/desktop-login.html"


def guest(command, timeout=60):
    name = f"elsebot-{BOT}"
    return subprocess.check_output(["msb", "exec", name, "--", "sh", "-c", command],
                                   env={**os.environ, "MSB_HOME": MSB_HOME}, text=True, timeout=timeout).strip()


def write(path, data):
    guest("printf %s " + shlex.quote(base64.b64encode(data).decode()) + " | base64 -d >" + shlex.quote(path))


def browser(expr, create=False, page=PAGE):
    select = f"{{create:'{page}'}}" if create else f"{{urlPrefix:'{page}'}}"
    script = ("import {connect} from '/usr/local/lib/elsebot/cdp/cdp.mjs';const b=await connect();"
              f"try{{const p=await b.page({select});console.log(JSON.stringify(await p.read({json.dumps(expr)})));}}"
              "finally{await b.close()}")
    write("/home/elsebot/work/gate-probe.mjs", script.encode())
    return json.loads(guest("runuser -u elsebot -- node /home/elsebot/work/gate-probe.mjs"))


def log(entry):
    entry = {"bot": BOT, "at": time.strftime("%Y-%m-%dT%H:%M:%S"), **entry}
    with (HERE / "results.jsonl").open("a") as f:
        f.write(json.dumps(entry, ensure_ascii=False) + "\n")
    print(json.dumps(entry, ensure_ascii=False), flush=True)


def probe(steps):
    env = {**os.environ, "PROBE_BOT": BOT, "PROBE_SCRIPT": json.dumps(steps),
           "CARGO_TARGET_DIR": str(REPO / ".else-dev/.build")}
    out = subprocess.run(["cargo", "test", "-q", "else62_keymap_probe", "--", "--ignored", "--nocapture"],
                         cwd=PROBE, env=env, capture_output=True, text=True, timeout=600)
    line = next((l for l in out.stdout.splitlines() if l.startswith("PROBE ")), None)
    if out.returncode or not line:
        raise SystemExit(f"probe failed: {out.stdout[-2000:]} {out.stderr[-2000:]}")
    return json.loads(line[6:])


def fresh_display():
    guest(f"{SUP} stop chromium x11vnc x11vnc-watch openbox xvfb >/dev/null; "
          f"{SUP} start xvfb openbox x11vnc x11vnc-watch chromium >/dev/null; sleep 4; {SUP} status | grep -v RUNNING || true")
    guest("curl -sf " + PAGE + " >/dev/null || setsid nohup runuser -u elsebot -- python3 -m http.server 8421 "
          "--bind 127.0.0.1 --directory /home/elsebot/work/viewer-fixture >/tmp/viewer-http.log 2>&1 </dev/null & "
          "for a in $(seq 1 100); do curl -sf " + PAGE + " >/dev/null && break; sleep 0.1; done")
    browser("document.title", create=True)


def arm_page():
    browser("user.value='';pass.value='';window.inTimes=[];window.keyCodes=[];"
            "user.oninput=pass.oninput=()=>inTimes.push(Date.now());"
            "user.onkeydown=e=>keyCodes.push(e.code);window.scrollTo(0,0);user.focus();true")


def page_state():
    return browser("({user:user.value,pass:pass.value,inputs:inTimes.length,"
                   "first:inTimes[0]||null,last:inTimes[inTimes.length-1]||null,focus:document.activeElement.id})")


HELD = r'''
import ctypes, json
x = ctypes.CDLL("libX11.so.6"); x.XOpenDisplay.restype = ctypes.c_void_p
x.XDefaultRootWindow.restype = ctypes.c_ulong
d = ctypes.c_void_p(x.XOpenDisplay(b":99")); k = ctypes.create_string_buffer(32); x.XQueryKeymap(d, k)
w = ctypes.c_ulong(); i = ctypes.c_int(); m = ctypes.c_uint()
x.XQueryPointer(d, ctypes.c_ulong(x.XDefaultRootWindow(d)), ctypes.byref(w), ctypes.byref(w), ctypes.byref(i),
                ctypes.byref(i), ctypes.byref(i), ctypes.byref(i), ctypes.byref(m))
print(json.dumps({"keys_down": [c for c in range(256) if k.raw[c // 8] >> (c % 8) & 1], "buttons": m.value & 0x1F00}))
'''

# Waits for the controlling connection to take the browser lock, then
# blocks on it the way a CDP helper would and records when it got it.
WATCH = r'''
import fcntl, json, os, sys, time
fd = os.open("/var/lib/elsebot/browser.lock", os.O_RDWR | os.O_CREAT, 0o600)
deadline = time.time() + 120
while time.time() < deadline:
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB); fcntl.flock(fd, fcntl.LOCK_UN); time.sleep(0.002)
    except BlockingIOError:
        break
held = time.time()
fcntl.flock(fd, fcntl.LOCK_EX)
free = time.time()
fcntl.flock(fd, fcntl.LOCK_UN)
print(json.dumps({"held_at": int(held * 1000), "free_at": int(free * 1000)}), flush=True)
'''

HELPER = ("import {connect} from '/usr/local/lib/elsebot/cdp/cdp.mjs';"
          "await new Promise(r=>setTimeout(r,1500));const t0=Date.now();"
          "const b=await connect({lockTimeoutMs:120000});console.log(JSON.stringify({helper_waited_from:t0,helper_got:Date.now()}));await b.close()")


def held():
    write("/tmp/held.py", HELD.encode())
    return json.loads(guest("runuser -u elsebot -- env DISPLAY=:99 python3 /tmp/held.py"))


def cjk(start, n):
    return "".join(chr(0x4E00 + start + i) for i in range(n))


def setup():
    request("POST", f"/v1/bots/{BOT}/computer/resume")
    probe([])
    guest("mkdir -p /home/elsebot/work/viewer-fixture && chown -R elsebot:elsebot /home/elsebot/work")
    write("/home/elsebot/work/viewer-fixture/desktop-login.html", (REPO / "scripts/fixtures/desktop-login.html").read_bytes())
    guest("chown -R elsebot:elsebot /home/elsebot/work")
    log({"setup": guest("dpkg-query -W -f='${Version}' x11vnc; echo; head -c 400 /etc/elsebot/image.json")})


def gate1(runs):
    texts = ["native-é漢字かな", cjk(3000, 100)]
    for i in range(runs):
        for text in texts:
            fresh_display()
            arm_page()
            probe([{"text": text, "per_char": True, "gap_ms": 30}])
            state = page_state()
            log({"gate": 1, "run": i + 1, "chars": len(text), "ok": state["user"] == text,
                 "got_len": len(state["user"]), "held": held()})


STALL = ("p=$(supervisorctl -c /etc/elsebot/supervisord.conf pid x11vnc); "
         "for i in $(seq 1 %d); do sleep 0.3; kill -STOP $p; sleep 0.1; kill -CONT $p; done")


def gate2():
    fresh_display()
    arm_page()
    text = cjk(5000, 60)
    stall = subprocess.Popen([sys.executable, __file__, "stall", "20"])
    probe([{"text": text, "per_char": True, "gap_ms": 30}])
    stall.wait(60)
    state = page_state()
    log({"gate": 2, "case": "x11vnc stalled 100 ms every 300 ms during 30 ms commits", "chars": len(text),
         "ok": state["user"] == text, "got_len": len(state["user"]), "held": held()})

    arm_page()
    tag = "é漢字" + cjk(7000 + int(time.time()) % 500, 3)
    probe([{"text": "ab"}, {"chord": [0xFFE3, 0x61]}, {"text": "かな"}, {"chord": [0xFF09]}, {"text": "пароль"},
           {"chord": [0xFFE3, 0x6C]}, {"sleep_ms": 300},
           {"text": PAGE + "#" + tag, "per_char": True, "gap_ms": 30}, {"chord": [0xFF0D]}, {"sleep_ms": 1500}])
    state = page_state()
    url = browser("decodeURIComponent(location.hash.slice(1))")
    log({"gate": 2, "case": "Ctrl+A replaces, Tab moves focus, Ctrl+L then text into the address bar",
         "user_ok": state["user"] == "かな", "pass_ok": state["pass"] == "пароль", "hash_ok": url == tag,
         "held": held()})


def gate3(end):
    fresh_display()
    arm_page()
    write("/tmp/lockwatch.py", WATCH.encode())
    write("/home/elsebot/work/helper-wait.mjs", HELPER.encode())
    guest("rm -f /tmp/lockwatch.json /tmp/helper.json; "
          "setsid nohup runuser -u elsebot -- python3 /tmp/lockwatch.py >/tmp/lockwatch.json 2>&1 </dev/null & "
          "setsid nohup runuser -u elsebot -- node /home/elsebot/work/helper-wait.mjs >/tmp/helper.json 2>&1 </dev/null & ")
    text = cjk(9000, 4096)
    report = probe([{"down": 0xFFE1}, {"text": text}, {"end": end}]) if end != "stop" else None
    time.sleep(8)
    watch = json.loads(guest("cat /tmp/lockwatch.json"))
    helper = guest("cat /tmp/helper.json")
    state = page_state()
    entry = {"gate": 3, "end": end, "sent": len(text), "typed": len(state["user"]),
             "prefix_ok": text.startswith(state["user"]), "lock": watch, "helper": helper,
             "inputs_after_lock": None if not state["last"] else int(state["last"] > watch["free_at"]),
             "last_input_to_lock_free_ms": None if not state["last"] else watch["free_at"] - state["last"],
             "held": held(), "probe": report}
    if report and "end_at" in report:
        entry["lock_free_minus_end_ms_crossclock"] = watch["free_at"] - report["end_at"]
    log(entry)


if __name__ == "__main__":
    cmd = sys.argv[1]
    if cmd == "setup":
        setup()
    elif cmd == "gate1":
        gate1(int(sys.argv[2]))
    elif cmd == "gate2":
        gate2()
    elif cmd == "gate3":
        gate3(sys.argv[2])
    elif cmd == "stall":
        guest(STALL % int(sys.argv[2]), timeout=120)
    elif cmd == "held":
        print(held())


def gate3_stop(api_pid):
    fresh_display()
    arm_page()
    write("/tmp/lockwatch.py", WATCH.encode())
    guest("rm -f /tmp/lockwatch.json; setsid nohup runuser -u elsebot -- python3 /tmp/lockwatch.py "
          ">/tmp/lockwatch.json 2>&1 </dev/null & ")
    text = cjk(9000, 4096)
    env = {**os.environ, "PROBE_BOT": BOT, "CARGO_TARGET_DIR": str(REPO / ".else-dev/.build"),
           "PROBE_SCRIPT": json.dumps([{"down": 0xFFE1}, {"text": text}, {"sleep_ms": 60000}])}
    client = subprocess.Popen(["cargo", "test", "-q", "else62_keymap_probe", "--", "--ignored", "--nocapture"],
                              cwd=PROBE, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(5)
    deadline = time.time()
    stop_at = int(time.time() * 1000)
    os.kill(api_pid, 15)
    while time.time() < deadline + 30:
        try:
            os.kill(api_pid, 0)
            time.sleep(0.05)
        except ProcessLookupError:
            break
    api_exit_at = int(time.time() * 1000)
    client.kill()
    return {"text": text, "stop_at": stop_at, "api_exit_at": api_exit_at}


def gate3_stop_report(started):
    watch = json.loads(guest("cat /tmp/lockwatch.json"))
    state = page_state()
    text = started.pop("text")
    log({"gate": 3, "end": "api-stop", "sent": len(text), "typed": len(state["user"]),
         "prefix_ok": text.startswith(state["user"]), "lock": watch,
         "inputs_after_lock": int(bool(state["last"]) and state["last"] > watch["free_at"]),
         "last_input_to_lock_free_ms": None if not state["last"] else watch["free_at"] - state["last"],
         "held": held(), **started})
