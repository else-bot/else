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


if __name__ == "__main__":
    unittest.main()
