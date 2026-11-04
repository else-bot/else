#!/usr/bin/env python3
"""Runner failure-path checks. Run inside omabox as documented in SKILL.md."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("verify_else", Path(__file__).with_name("verify.py"))
verify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify)


class RunnerChecks(unittest.TestCase):
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
               "CODEX_HOME": "/owner/login", "CLAUDE_CONFIG_DIR": "/owner/claude"}
        with patch.dict(os.environ, env, clear=True):
            clean = verify.clean_env()
        self.assertEqual(clean, {k: env[k] for k in ["PATH", "HOME", "WAYLAND_DISPLAY"]})

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
server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), H)
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
