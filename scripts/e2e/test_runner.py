#!/usr/bin/env python3
"""Offline regression checks for E2E isolation, fake releases and cancellation."""
import importlib.util
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("e2e_runner", Path(__file__).parents[1] / "test-e2e.py")
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class RunnerTests(unittest.TestCase):
    def source(self, root):
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        (root / ".gitignore").write_text(".env\ntarget/\n")
        (root / "Cargo.toml").write_text('[workspace.package]\nversion = "0.0.5"\n')
        (root / "Cargo.lock").write_text('version = 4\n\n[[package]]\nname = "else-api"\nversion = "0.0.5"\n\n[[package]]\nname = "else-desktop"\nversion = "0.0.5"\n')
        migrations = root / "apps/api/migrations"
        migrations.mkdir(parents=True)
        (migrations / "0009_base.sql").write_text("SELECT 1;\n")
        (root / "deleted").touch()
        subprocess.run(["git", "-C", str(root), "add", "."], check=True)
        (root / "deleted").unlink()
        (root / ".env").write_text("SECRET=private\n")
        (root / "target").mkdir()
        (root / "target/cache").write_text("do not copy")
        (root / "working-edit").write_text("uncommitted\n")
        (root / "source-link").symlink_to("working-edit")

    def test_snapshot_preserves_edits_and_links_but_excludes_secrets_and_caches(self):
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            root = work / "source"
            root.mkdir()
            self.source(root)
            with patch.object(runner, "ROOT", root):
                runner.snapshot(work / "snapshot")
            copied = work / "snapshot"
            self.assertEqual((copied / "working-edit").read_text(), "uncommitted\n")
            self.assertTrue((copied / "source-link").is_symlink())
            for excluded in [".env", "target", ".git", "deleted"]:
                self.assertFalse((copied / excluded).exists(), excluded)

    def test_both_fake_tags_have_matching_manifest_lock_and_update_migration(self):
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            root = work / "source"
            root.mkdir()
            self.source(root)
            with patch.object(runner, "ROOT", root):
                repo = runner.releases(work)
            for version in ["0.0.1", "0.0.2"]:
                def show(path):
                    return runner.capture(["git", "--git-dir", str(repo), "show", f"v{version}:{path}"])
                self.assertIn(f'version = "{version}"', show("Cargo.toml"))
                self.assertEqual(show("Cargo.lock").count(f'version = "{version}"'), 2)
            self.assertIn("SELECT 1", show("apps/api/migrations/0010_test_release.sql"))
            self.assertIn('version = "0.0.5"', (root / "Cargo.toml").read_text())

    def test_environment_removes_live_gates_and_private_configuration(self):
        original = {"HOME": "/owner", "PATH": "/usr/bin", "ELSE_LIVE_CODEX": "1",
                    "ELSE_HOME": "/owner/install", "DATABASE_URL": "private",
                    "CARGO_PROFILE_DEV_DEBUG": "2", "PGPASSWORD": "private"}
        with patch.dict(os.environ, original, clear=True):
            self.assertEqual(runner.clean_environment(), {"HOME": "/owner", "PATH": "/usr/bin"})
            self.assertEqual(dict(os.environ), original)

    def test_cancellation_terminates_a_running_child(self):
        with tempfile.TemporaryDirectory() as directory:
            pid_file = Path(directory) / "pid"
            cancel = queue.Queue(maxsize=1)
            outcomes = queue.Queue(maxsize=1)
            def execute():
                try:
                    runner.run([sys.executable, "-c", "import os,time,pathlib; pathlib.Path(" + repr(str(pid_file)) + ").write_text(str(os.getpid())); time.sleep(30)"], cancel=cancel)
                except InterruptedError:
                    outcomes.put("cancelled")
            thread = threading.Thread(target=execute)
            thread.start()
            deadline = time.monotonic() + 5
            while not pid_file.exists() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertTrue(pid_file.exists())
            cancel.put_nowait(True)
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive())
            self.assertEqual(outcomes.get_nowait(), "cancelled")
            with self.assertRaises(ProcessLookupError):
                os.kill(int(pid_file.read_text()), 0)

    def test_application_lane_runs_optional_s3_tests_without_ignoring_sqlite_tests(self):
        commands = queue.Queue(maxsize=3)
        def record(command, **kwargs):
            commands.put_nowait(command)
        with tempfile.TemporaryDirectory() as directory:
            result = {}
            with patch.object(runner, "run", side_effect=record):
                runner.application_tests(["cargo", "test"], ["replication", "sqlite"],
                                         SimpleNamespace(test_threads=4, test_jobs=2), {},
                                         Path(directory), result)
            captured = [commands.get_nowait() for _ in range(3)]
            for command in captured:
                if "replication" in command:
                    self.assertIn("--ignored", command)
                else:
                    self.assertNotIn("--ignored", command)
            self.assertEqual(set(result["test_targets"]), {"unit", "replication", "sqlite"})

    def test_each_cache_directory_has_one_owner_and_its_own_ubuntu_volume(self):
        with tempfile.TemporaryDirectory() as directory:
            first, second = Path(directory) / "first", Path(directory) / "second"
            self.assertNotEqual(runner.ubuntu_cache_volume(first, "sdk"), runner.ubuntu_cache_volume(second, "sdk"))
            self.assertEqual(runner.ubuntu_cache_volume(first, "sdk"), runner.ubuntu_cache_volume(first, "sdk"))
            owner = runner.own_cache(first)
            with self.assertRaisesRegex(RuntimeError, "another E2E run owns"):
                runner.own_cache(first)
            runner.own_cache(second).close()
            owner.close()
            runner.own_cache(first).close()

    def test_installer_rejects_unscoped_fast_profile_and_relative_cache(self):
        for extra, message in [({"ELSE_INSTALL_TEST_PROFILE": "dev"}, "requires ELSE_INSTALL_TEST_CACHE"),
                               ({"ELSE_INSTALL_TEST_CACHE": "relative"}, "must be an absolute path")]:
            env = runner.clean_environment()
            env.update(extra)
            result = subprocess.run(["bash", str(runner.ROOT / "install.sh"), "--help"], env=env, text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(message, result.stderr)


if __name__ == "__main__":
    unittest.main()
