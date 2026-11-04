#!/usr/bin/env python3
"""Runner failure-path checks for native macOS and owned Linux environments."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import signal
import selectors
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("verify_else", Path(__file__).with_name("verify.py"))
verify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify)


class RunnerChecks(unittest.TestCase):
    def test_fixture_supports_the_skill_catalog_handshake(self):
        request = {"id": 1, "method": "skills/list", "params": {"cwds": ["/tmp/project"], "forceReload": True}}
        result = subprocess.run(
            [sys.executable, str(verify.SCRIPT.with_name("fake-codex.py")), "app-server"],
            input=json.dumps(request) + "\n", capture_output=True, text=True, check=True, timeout=5,
        )
        self.assertEqual(json.loads(result.stdout), {"jsonrpc": "2.0", "id": 1, "result": {"data": []}})

    def test_fixture_can_resume_after_a_historical_hang_marker(self):
        child = subprocess.Popen([sys.executable, str(verify.SCRIPT.with_name("fake-codex.py")), "app-server"],
                                 stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        selector = selectors.DefaultSelector()
        selector.register(child.stdout, selectors.EVENT_READ)
        pending = b""

        def send(request):
            child.stdin.write(json.dumps(request).encode() + b"\n")
            child.stdin.flush()

        def receive():
            nonlocal pending
            while b"\n" not in pending:
                self.assertTrue(selector.select(timeout=10), "fixture protocol stalled")
                chunk = os.read(child.stdout.fileno(), 65536)
                self.assertTrue(chunk, "fixture exited before replying")
                pending += chunk
            line, pending = pending.split(b"\n", 1)
            return json.loads(line)

        try:
            send({"id": 1, "method": "turn/start", "params": {"input": [{"text": "[ui:hang]"}]}})
            stopped = receive()["result"]["turn"]["id"]
            send({"id": 2, "method": "turn/interrupt", "params": {"turnId": stopped}})
            self.assertEqual(receive()["id"], 2)
            self.assertEqual(receive()["params"]["turn"]["status"], "interrupted")
            send({"id": 3, "method": "turn/start", "params": {"input": [{
                "text": "<history>[ui:hang]</history>\n\nOwner's latest message:\nresume"}]}})
            resumed = receive()["result"]["turn"]["id"]
            self.assertNotEqual(stopped, resumed)
            text = ""
            while True:
                message = receive()
                if message["method"] == "turn/completed":
                    self.assertEqual(message["params"]["turn"], {"id": resumed, "status": "completed"})
                    break
                self.assertEqual(message["params"]["turnId"], resumed)
                text += message["params"]["delta"]
            self.assertEqual(text, "Fixture reply ✓ from the desktop composer.")
        finally:
            selector.close()
            child.stdin.close()
            child.wait(timeout=5)
            child.stdout.close()

    def test_interrupt_reaps_running_lanes_and_removes_private_homes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state = root / "child.json"
            test = root / "blocking-test"
            test.write_text("#!/usr/bin/env python3\nimport json,os,pathlib,time\npathlib.Path(" + repr(str(state)) +
                            ").write_text(json.dumps({'pid':os.getpid(),'home':os.environ['HOME']}))\ntime.sleep(60)\n")
            test.chmod(0o700)
            spec_file = root / "spec.json"
            spec_file.write_text(json.dumps({"run_id": "interrupt", "output": str(root / "evidence"),
                "api": "/bin/false", "jobs": 4, "test_threads": 1, "timeout": 60,
                "excluded_targets": [], "tests": [{"name": "blocking", "executable": str(test)}]}))
            with (root / "worker.log").open("w") as log:
                child = subprocess.Popen([sys.executable, str(verify.SCRIPT), "--backend", "process", "--worker", str(spec_file)],
                                         stdout=log, stderr=subprocess.STDOUT)
                try:
                    deadline = time.monotonic() + 5
                    while not state.exists():
                        self.assertIsNone(child.poll())
                        self.assertLess(time.monotonic(), deadline)
                        time.sleep(0.01)
                    owned = json.loads(state.read_text())
                    child.send_signal(signal.SIGTERM)
                    self.assertNotEqual(child.wait(timeout=5), 0)
                    with self.assertRaises(ProcessLookupError):
                        os.kill(owned["pid"], 0)
                    self.assertFalse(Path(owned["home"]).exists())
                finally:
                    if child.poll() is None:
                        child.kill()
                        child.wait()

    def test_timeout_reaps_the_owned_child(self):
        with tempfile.TemporaryDirectory() as directory:
            pid = Path(directory) / "pid"
            with self.assertRaises(subprocess.TimeoutExpired):
                verify.command([sys.executable, "-c",
                                "import os,time,pathlib; pathlib.Path(" + repr(str(pid)) +
                                ").write_text(str(os.getpid())); time.sleep(30)"], timeout=0.5)
            self.assertTrue(pid.exists())
            with self.assertRaises(ProcessLookupError):
                os.kill(int(pid.read_text()), 0)

    def test_failed_lanes_retain_evidence_and_fail_the_run(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            fake = home / "failed-test"
            fake.write_text('#!/bin/sh\necho "test result: ok. 1 passed; 0 failed; 0 ignored"\nexit 7\n')
            fake.chmod(0o700)
            with patch.object(Path, "home", return_value=home):
                status = verify.worker({"run_id": "failure-proof", "api": "/bin/false", "jobs": 2,
                                        "test_threads": 1, "timeout": 2, "excluded_targets": [],
                                        "tests": [{"name": "exit-failure", "executable": str(fake)}]})
            self.assertEqual(status, 1)
            evidence = home / "else-verification/failure-proof"
            report = json.loads((evidence / "results.json").read_text())
            self.assertEqual(report["status"], "failed")
            self.assertTrue(all(lane["status"] == "failed" for lane in report["lanes"].values()))
            self.assertIn("1 passed", (evidence / "test-exit-failure/test.log").read_text())
            self.assertTrue((evidence / "bots/setup.log").exists())

    def test_runtime_environment_drops_inherited_configuration(self):
        env = {"PATH": "/usr/bin", "HOME": "/private-box", "WAYLAND_DISPLAY": "wayland-0",
               "ELSE_LIVE_CODEX": "1", "DATABASE_URL": "owner.sqlite3", "SECRET_KEY": "private",
               "CODEX_HOME": "/owner/login", "CLAUDE_CONFIG_DIR": "/owner/claude", "AWS_PROFILE": "owner"}
        with patch.dict(os.environ, env, clear=True):
            clean = verify.clean_env()
        self.assertEqual(clean, {k: env[k] for k in ["PATH", "HOME", "WAYLAND_DISPLAY"]})

    def test_instances_own_private_runtime_homes_and_remove_them(self):
        with tempfile.TemporaryDirectory() as directory:
            api = verify.Instance("/bin/false", Path(directory))
            home = Path(api.env["HOME"])
            self.assertTrue(home.is_dir())
            self.assertTrue(home.is_relative_to(api.work))
            self.assertEqual(api.env["XDG_CONFIG_HOME"], str(home / ".config"))
            api.__exit__()
            self.assertFalse(home.exists())

    def start_fake(self, directory, serve):
        fake = Path(directory) / "fake-api"
        fake.write_text("#!/usr/bin/env python3\nimport os, sys\n" + serve)
        fake.chmod(0o700)
        api = verify.Instance(str(fake), Path(directory))
        self.addCleanup(api.__exit__)
        api.data.mkdir()
        (api.data / "api.json").write_text(json.dumps({"pid": os.getpid(), "url": "http://127.0.0.1:9"}))
        return api

    def test_start_uses_the_port_its_own_child_publishes(self):
        serve = """import http.server, json, pathlib
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200); self.send_header("Content-Length", "19"); self.end_headers()
        self.wfile.write(b'{"status": "ready"}')
    def log_message(self, *_): pass
assert os.environ["ELSE_API_BIND"] == "127.0.0.1:0"
class Server(http.server.ThreadingHTTPServer):
    def server_bind(self):
        http.server.socketserver.TCPServer.server_bind(self)
        self.server_name = "localhost"
        self.server_port = self.server_address[1]
server = Server(("127.0.0.1", 0), H)
data = pathlib.Path(os.environ["ELSE_DATA_DIR"])
(data / "api.json").write_text(json.dumps({"pid": os.getpid(), "url": f"http://127.0.0.1:{server.server_port}"}))
server.serve_forever()
"""
        with tempfile.TemporaryDirectory() as directory:
            api = self.start_fake(directory, serve)
            with patch.object(verify.Instance, "doctor"):
                api.start()
            published = json.loads((api.data / "api.json").read_text())
            self.assertEqual(published["pid"], api.child.pid)
            self.assertEqual(api.url, published["url"])
            self.assertNotEqual(api.url, "http://127.0.0.1:9")

    def test_start_fails_when_its_child_exits_without_publishing(self):
        with tempfile.TemporaryDirectory() as directory:
            api = self.start_fake(directory, "import time\ntime.sleep(0.3)\nsys.exit(3)\n")
            with self.assertRaisesRegex(RuntimeError, "exited before publishing"):
                api.start()
            self.assertFalse((api.data / "api.json").exists(), "a stale manifest must not be trusted")


if __name__ == "__main__":
    unittest.main()
