#!/usr/bin/env python3
"""Behavior checks for scripts/dev.py against fake API, desktop, and omabox executables.

Run inside an owned omabox: omabox run -b BOX -- python3 scripts/test-dev.py
"""

import fcntl
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from unittest.mock import patch

import dev

DEV = Path(__file__).resolve().with_name("dev.py")

FAKE_API = textwrap.dedent('''\
    #!/usr/bin/env python3
    """Implements the API's manifest and lock contract: api.json, api.lock, <db>.api.lock."""
    import fcntl, http.server, json, os, sqlite3, sys, time, uuid
    from pathlib import Path
    data = Path(os.environ["ELSE_DATA_DIR"])
    state = data.parent
    db = data / "else.sqlite3"
    (state / f"env-{sys.argv[1]}.json").write_text(json.dumps({"cwd": os.getcwd(), "env": dict(os.environ)}))
    if sys.argv[1] == "setup":
        data.mkdir(mode=0o700, exist_ok=True)
        token = data / "owner-token"
        if not token.exists():
            token.write_text(uuid.uuid4().hex)
        with sqlite3.connect(db) as conn:
            conn.execute("CREATE TABLE IF NOT EXISTS installation (default_bot_id BLOB)")
            if not conn.execute("SELECT 1 FROM installation").fetchone():
                conn.execute("INSERT INTO installation VALUES (?)", (uuid.uuid4().bytes,))
        sys.exit(0)
    mode = os.environ.get("FAKE_API_MODE", "serve")
    (state / "serve.pid").write_text(str(os.getpid()))
    if mode == "exit":
        sys.exit(3)
    locks = [open(data / "api.lock", "a"), open(f"{os.path.realpath(db)}.api.lock", "a")]
    for lock in locks:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    if mode == "silent":
        time.sleep(600)
    with sqlite3.connect(db) as conn:
        main = str(uuid.UUID(bytes=conn.execute("SELECT default_bot_id FROM installation").fetchone()[0]))
    token = (data / "owner-token").read_text()
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path != "/v1/onboarding" or self.headers.get("Authorization") != f"Bearer {token}":
                self.send_error(403)
                return
            (state / "onboarding.json").write_text(json.dumps(body))
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"{}")
        def do_GET(self):
            if self.path == "/readyz":
                body, code = {"status": "ready"}, 200
            elif self.headers.get("Authorization") != f"Bearer {token}":
                body, code = {"error": {"code": "unauthorized"}}, 401
            else:
                body, code = {"bots": [{"id": main}], "default_bot_id": main, "categories": []}, 200
            payload = json.dumps(body).encode()
            self.send_response(code)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        def log_message(self, *_):
            pass
    host, port = os.environ["ELSE_API_BIND"].rsplit(":", 1)
    server = http.server.ThreadingHTTPServer((host, int(port)), Handler)
    (data / "api.tmp").write_text(json.dumps({"pid": os.getpid(), "url": f"http://127.0.0.1:{server.server_port}"}))
    os.replace(data / "api.tmp", data / "api.json")
    server.serve_forever()
''')

FAKE_DESKTOP = "#!/usr/bin/env python3\nimport time\ntime.sleep(600)\n"

FAKE_OMABOX = textwrap.dedent('''\
    #!/usr/bin/env python3
    import json, os, sys
    from pathlib import Path
    root = Path(os.environ["FAKE_OMABOX_DIR"])
    with (root / "calls.jsonl").open("a") as calls:
        calls.write(json.dumps(sys.argv[1:]) + "\\n")
    home = root / "box-7" / "home"
    command = sys.argv[1]
    if command == "up":
        home.mkdir(parents=True)
        print(json.dumps({"name": "box-7"}))
    elif command == "path":
        print(home)
    elif command == "run":
        argv = sys.argv[sys.argv.index("--") + 1:]
        os.execvpe(argv[0], argv, {**os.environ, "HOME": str(home), "WAYLAND_DISPLAY": "wayland-box-7"})
    elif command == "windows":
        ready = [json.loads(p.read_text()) for p in home.glob("else-dev/*/ready.json")]
        windows = [{"address": "0xother", "class": "foot", "pid": 2}]
        for state in ready:
            windows.append({"address": "0xowned", "class": "else-bot", "pid": state["desktop_pid"]})
            if os.environ.get("FAKE_DUPLICATE"):
                windows.append({"address": "0xcopy", "class": "else-bot", "pid": state["desktop_pid"]})
        print(json.dumps(windows))
    elif command == "down":
        sys.exit(int(os.environ.get("FAKE_DOWN_STATUS", "0")))
''')


def executable(path, text):
    path.write_text(text)
    path.chmod(0o700)
    return path


def dead(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return True
    # A reaped zombie of another parent still answers kill(0); its state says it is gone.
    status = Path(f"/proc/{pid}/stat")
    return status.exists() and status.read_text().split(") ", 1)[1].startswith("Z")


class DevSessions(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="else-test-dev-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.api = executable(self.tmp / "fake-api", FAKE_API)
        self.desktop = executable(self.tmp / "fake-desktop", FAKE_DESKTOP)
        self.env = {k: v for k, v in os.environ.items() if not k.startswith(("ELSE_", "CARGO_"))}
        self.processes = []
        self.addCleanup(self.stop_all)

    def stop_all(self):
        for process in self.processes:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
            process.communicate(timeout=30)

    def checkout(self, name):
        root = self.tmp / name
        (root / "scripts").mkdir(parents=True)
        shutil.copy(DEV, root / "scripts/dev.py")
        return root

    def dev(self, root, *args, env=None):
        return subprocess.run([sys.executable, root / "scripts/dev.py", *args, "--api-binary", self.api],
                              env={**self.env, **(env or {})}, capture_output=True, text=True, timeout=60)

    def start(self, root, *args, env=None):
        process = subprocess.Popen([sys.executable, root / "scripts/dev.py", "api", *args, "--api-binary", self.api],
                                   env={**self.env, **(env or {})}, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)
        self.processes.append(process)
        return process

    def status(self, root, *args):
        deadline = time.monotonic() + 20
        while True:
            result = self.dev(root, "status", *args)
            if result.returncode == 0:
                return json.loads(result.stdout)
            self.assertLess(time.monotonic(), deadline, result.stderr)
            time.sleep(0.1)

    def test_two_checkouts_run_the_same_session_name_privately(self):
        a, b = self.checkout("a"), self.checkout("b")
        first, second = self.start(a, "--session", "work"), self.start(b, "--session", "work")
        one, two = self.status(a, "--session", "work"), self.status(b, "--session", "work")
        self.assertEqual(Path(one["state"]), a / ".else-dev/work")
        self.assertEqual(Path(two["state"]), b / ".else-dev/work")
        self.assertNotEqual(one["url"], two["url"])
        self.assertNotEqual(one["main_bot_id"], two["main_bot_id"])
        served = [json.loads((Path(s["state"]) / "env-serve.json").read_text())["env"] for s in (one, two)]
        self.assertEqual([e["ELSE_BOTS_DIR"] for e in served], [str(a / ".else-dev/work/projects"),
                                                                 str(b / ".else-dev/work/projects")])
        for process, info in ((first, one), (second, two)):
            process.send_signal(signal.SIGTERM)
            self.assertEqual(process.wait(timeout=20), 130)
            self.assertTrue(dead(info["pid"]), "API outlived its launcher")

    def test_desktop_profiles_are_private_and_follow_api_restarts(self):
        a, b = self.checkout("profile-a"), self.checkout("profile-b")
        first = self.start(a)
        self.start(b)
        one, two = self.status(a), self.status(b)
        printer = executable(self.tmp / "profile-desktop", "#!/usr/bin/env python3\nimport os\nprint(os.environ['ELSE_DESKTOP_CONFIG_FILE'])\n")
        foreign = self.tmp / "foreign-profile.json"
        foreign.write_text("untouched")

        def profile(root, info):
            result = self.dev(root, "desktop", "--desktop-binary", printer,
                              env={"ELSE_DESKTOP_CONFIG_FILE": str(foreign)})
            self.assertEqual(result.returncode, 0, result.stderr)
            path = Path(result.stdout.strip())
            self.assertEqual(path, Path(info["state"]) / "desktop/desktop.json")
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(path.parent.stat().st_mode & 0o777, 0o700)
            stored = json.loads(path.read_text())
            self.assertEqual(stored["origin"], info["url"])
            self.assertEqual(stored["owner_token"], (Path(info["state"]) / "data/owner-token").read_text().strip())
            return path

        original = profile(a, one)
        self.assertNotEqual(original, profile(b, two))
        first.send_signal(signal.SIGTERM)
        self.assertEqual(first.wait(timeout=20), 130)
        self.start(a)
        restarted = self.status(a)
        self.assertEqual(original, profile(a, restarted))
        self.assertEqual(foreign.read_text(), "untouched")

    def test_restart_replaces_a_stale_manifest_and_status_proves_ownership(self):
        root = self.checkout("a")
        self.assertEqual(self.dev(root, "reset").returncode, 0)
        manifest = root / ".else-dev/default/data/api.json"
        manifest.write_text(json.dumps({"pid": os.getpid(), "url": "http://127.0.0.1:9"}))
        refused = self.dev(root, "status")
        self.assertEqual(refused.returncode, 1)
        self.assertIn("does not hold", refused.stderr)
        self.start(root)
        info = self.status(root)
        self.assertNotEqual(info["pid"], os.getpid())
        self.assertEqual(json.loads(manifest.read_text())["pid"], info["pid"])
        forwarded = self.dev(root, "curl", "-o", "/dev/null", "http://example.invalid/v1/bots")
        self.assertEqual(forwarded.returncode, 1)
        self.assertIn("/PATH arguments only", forwarded.stderr)
        listed = self.dev(root, "curl", "/v1/bots")
        self.assertEqual(json.loads(listed.stdout)["default_bot_id"], info["main_bot_id"], listed.stderr)
        payload = {"name": "https://example.com"}
        posted = self.dev(root, "curl", "-X", "POST", "-d", json.dumps(payload), "/v1/onboarding")
        self.assertEqual(posted.returncode, 0, posted.stderr)
        self.assertEqual(json.loads((root / ".else-dev/default/onboarding.json").read_text()), payload)

    def test_ambient_owner_configuration_never_reaches_the_api(self):
        root = self.checkout("a")
        (root / ".env").write_text("ELSE_S3_BUCKET=owner-bucket\nDATABASE_URL=sqlite:///owner.sqlite3\n")
        ambient = {"DATABASE_URL": "sqlite:///home/owner/.else/else.sqlite3", "SECRET_KEY": "owner-key",
                   "OWNER_TOKEN_FILE": "/home/owner/.else/owner-token", "ELSE_DATA_DIR": "/home/owner/.else",
                   "ELSE_S3_BUCKET": "owner-bucket", "ELSE_BACKUP_DIR": "/home/owner/backups",
                   "AWS_ACCESS_KEY_ID": "owner", "ELSE_CODEX_BIN": "/opt/codex/bin/codex"}
        result = self.dev(root, "reset", env=ambient)
        self.assertEqual(result.returncode, 0, result.stderr)
        state = root / ".else-dev/default"
        setup = json.loads((state / "env-setup.json").read_text())
        env = setup["env"]
        self.assertEqual(setup["cwd"], "/", "the API must not find the checkout .env")
        self.assertEqual(env["DATABASE_URL"], f"sqlite://{state}/data/else.sqlite3")
        self.assertEqual(env["OWNER_TOKEN_FILE"], str(state / "data/owner-token"))
        self.assertEqual(env["ELSE_BACKUP_DIR"], str(state / "data/backups"))
        self.assertEqual(env["ELSE_CODEX_BIN"], "/opt/codex/bin/codex", "host provider preference is kept")
        for leaked in ("SECRET_KEY", "ELSE_S3_BUCKET", "AWS_ACCESS_KEY_ID"):
            self.assertNotIn(leaked, env)

    def test_explicit_directories_owned_by_others_are_never_deleted(self):
        a, b = self.checkout("a"), self.checkout("b")
        shared = self.tmp / "shared-demo"
        self.assertEqual(self.dev(a, "reset", "--session", "demo", "--dir", shared).returncode, 0)
        for root, session in ((b, "demo"), (a, "other")):
            for command in ("reset", "drop"):
                result = self.dev(root, command, "--session", session, "--dir", shared)
                self.assertEqual(result.returncode, 1)
                self.assertIn("belongs to", result.stderr)
        self.assertTrue((shared / "data/owner-token").exists())

        legacy = self.tmp / "else-demo"
        (legacy / "data").mkdir(parents=True)
        (legacy / ".demo-owned").write_text("else.bot disposable demo\n")
        result = self.dev(a, "drop", "--session", "demo", "--dir", legacy)
        self.assertIn("not created by scripts/dev.py", result.stderr)
        self.assertTrue((legacy / "data").is_dir())

        link = self.tmp / "link"
        link.symlink_to(shared)
        result = self.dev(a, "drop", "--session", "demo", "--dir", link)
        self.assertIn("is not a directory", result.stderr)
        self.assertTrue((shared / "data/owner-token").exists())

    def test_reset_and_drop_refuse_a_running_api(self):
        root = self.checkout("a")
        self.assertEqual(self.dev(root, "reset").returncode, 0)
        data = root / ".else-dev/default/data"
        with open(data / "api.lock", "a") as independent:
            fcntl.flock(independent, fcntl.LOCK_EX | fcntl.LOCK_NB)
            for command in ("reset", "drop"):
                result = self.dev(root, command)
                self.assertEqual(result.returncode, 1)
                self.assertIn("an else.bot API is running", result.stderr)
            self.assertTrue((data / "owner-token").exists())
        self.start(root)
        self.status(root)
        result = self.dev(root, "drop")
        self.assertIn("session default is in use", result.stderr)
        self.assertTrue((data / "owner-token").exists())
        self.stop_all()
        self.assertEqual(self.dev(root, "drop").returncode, 0)
        self.assertFalse((root / ".else-dev/default").exists())
        self.assertTrue((root / ".else-dev/.default.lock").exists(), "the session lock is never unlinked")

    def test_failed_and_interrupted_starts_leave_nothing_running(self):
        root = self.checkout("a")
        failed = self.start(root, env={"FAKE_API_MODE": "exit"})
        self.assertEqual(failed.wait(timeout=30), 1)
        self.assertIn("exited with status 3", failed.communicate()[1])

        hung = self.start(root, env={"FAKE_API_MODE": "silent"})
        pid_file = root / ".else-dev/default/serve.pid"
        deadline = time.monotonic() + 20
        while not pid_file.exists() or not (root / ".else-dev/default/data/api.lock").exists():
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.05)
        time.sleep(0.2)
        hung.send_signal(signal.SIGTERM)
        self.assertEqual(hung.wait(timeout=30), 130)
        self.assertTrue(dead(int(pid_file.read_text())), "interrupted start left its API running")

    def run_box(self, root, env):
        boxes = self.tmp / "omabox"
        boxes.mkdir()
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        executable(bin_dir / "omabox", FAKE_OMABOX)
        env = {"PATH": f"{bin_dir}:{self.env['PATH']}", "FAKE_OMABOX_DIR": str(boxes), **env}
        process = subprocess.Popen([sys.executable, root / "scripts/dev.py", "run", "--api-binary", self.api,
                                    "--desktop-binary", self.desktop], env={**self.env, **env},
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.processes.append(process)
        return process, boxes

    def test_boxed_run_drives_only_its_own_box_and_window(self):
        root = self.checkout("a")
        process, boxes = self.run_box(root, {})
        lines = []
        while not lines or lines[-1] != "}\n":
            line = process.stdout.readline()
            self.assertTrue(line, process.stderr.read() if process.poll() is not None else "no manifest")
            lines.append(line)
        manifest = json.loads("".join(lines))
        self.assertEqual((manifest["box"], manifest["window"]), ("box-7", "0xowned"))
        self.assertIn("omabox shot -b box-7 --window 0xowned", manifest["shot_command"])
        process.send_signal(signal.SIGTERM)
        self.assertEqual(process.wait(timeout=30), 0)
        calls = [json.loads(line) for line in (boxes / "calls.jsonl").read_text().splitlines()]
        self.assertEqual(calls[0][:4], ["up", "--new", "--net", "isolated"])
        self.assertTrue(all(call[1:3] == ["-b", "box-7"] for call in calls[1:]), calls)
        self.assertEqual(calls[-1], ["down", "-b", "box-7"])
        evidence = Path(manifest["evidence"])
        self.assertEqual(json.loads((evidence / "manifest.json").read_text())["status"], "stopped")
        self.assertEqual(json.loads((evidence / "box/doctor.json").read_text())["pid"], manifest["api_pid"])
        self.assertEqual(json.loads((evidence / "box/onboarding.json").read_text())["name"],
                         "Development Assistant")
        self.assertFalse((evidence / "box/data").exists(), "live SQLite must stay in the box")
        self.assertFalse((evidence / "box/desktop").exists(), "the saved owner token must stay in the box")
        state = boxes / "box-7/home/else-dev" / manifest["run"]
        self.assertTrue(dead(manifest["api_pid"]) and dead(manifest["desktop_pid"]))
        env = json.loads((state / "env-serve.json").read_text())["env"]
        self.assertEqual(env["ELSE_CODEX_BIN"], "/nonexistent/else-dev-codex")

    def test_boxed_run_refuses_duplicate_windows_and_still_cleans_up(self):
        process, boxes = self.run_box(self.checkout("a"), {"FAKE_DUPLICATE": "1"})
        self.assertEqual(process.wait(timeout=60), 1)
        self.assertIn("refusing ambiguous windows in box-7", process.communicate()[1])
        calls = [json.loads(line) for line in (boxes / "calls.jsonl").read_text().splitlines()]
        self.assertEqual(calls[-1], ["down", "-b", "box-7"])
        self.assertNotIn(["shot"], [c[:1] for c in calls])

    def test_failed_box_cleanup_fails_the_run(self):
        process, boxes = self.run_box(self.checkout("cleanup"), {"FAKE_DOWN_STATUS": "4"})
        lines = []
        while not lines or lines[-1] != "}\n":
            line = process.stdout.readline()
            self.assertTrue(line, "no manifest")
            lines.append(line)
        manifest = json.loads("".join(lines))
        process.send_signal(signal.SIGTERM)
        self.assertEqual(process.wait(timeout=30), 1)
        report = json.loads((Path(manifest["evidence"]) / "manifest.json").read_text())
        self.assertEqual(report["status"], "failed")
        self.assertIn("cleanup_error", report)

    def test_build_outputs_are_private_even_with_a_shared_cargo_target(self):
        tools = self.tmp / "cargo-bin"
        tools.mkdir()
        executable(tools / "cargo", '''#!/usr/bin/env python3
import json,os,sys
from pathlib import Path
name=sys.argv[sys.argv.index("-p")+1]
path=Path(os.environ["CARGO_TARGET_DIR"])/"debug"/name
print(json.dumps({"reason":"compiler-artifact","target":{"name":name},"executable":str(path)}))
''')
        roots = [self.checkout("build-a"), self.checkout("build-b")]
        outputs = []
        with patch.dict(os.environ, {"PATH": f"{tools}:{os.environ['PATH']}",
                                    "CARGO_TARGET_DIR": str(self.tmp / "shared-target")}):
            for root in roots:
                with patch.object(dev, "ROOT", root):
                    outputs.append(Path(dev.build("else-api", None)))
        self.assertNotEqual(outputs[0], outputs[1])
        for root, output in zip(roots, outputs):
            self.assertTrue(output.is_relative_to(root))

    def test_removal_cannot_delete_a_new_api_at_the_original_path(self):
        root = self.checkout("removal")
        session = dev.Session(root, "default", root / ".else-dev/default")
        fd = session.lock()
        remove = shutil.rmtree
        try:
            session.prepare(str(self.api), host_providers=False)
            def start_new_api_while_deleting_old_state(path, *args, **kwargs):
                if path == session.state:
                    remove(path)
                    return
                self.assertIsNone(dev.try_lock(Path(path) / "data/api.lock", create=False))
                session.prepare(str(self.api), host_providers=False)
                (session.state / "new-api-state").write_text("new installation")
                remove(path, *args, **kwargs)
            with patch.object(dev.shutil, "rmtree", side_effect=start_new_api_while_deleting_old_state):
                session.remove()
            self.assertEqual((session.state / "new-api-state").read_text(), "new installation")
            self.assertTrue(session.database.exists())
        finally:
            os.close(fd)


if __name__ == "__main__":
    unittest.main()
