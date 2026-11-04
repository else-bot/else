#!/usr/bin/env python3
"""Offline checks for installer database configuration and release switching."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
DEFINITIONS = (ROOT / "install.sh").read_text().rsplit('\nmain "$@"', 1)[0]


class InstallerConfigTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.home = Path(directory.name)
        (self.home / "config").mkdir()
        (self.home / "data").mkdir()
        (self.home / "versions/0.0.3").mkdir(parents=True)
        (self.home / "versions/0.0.6").mkdir(parents=True)
        (self.home / "current").symlink_to("versions/0.0.3")
        self.config = self.home / "config/.env"

    def update(self, url, **overrides):
        if url is not None:
            self.config.write_text(f"DATABASE_URL={url}\nELSE_API_BIND=127.0.0.1:7878\nCUSTOM_SETTING=kept\n")
        (self.home / "data/owner-token").write_text("existing-token")
        env = {
            "PATH": os.environ["PATH"], "HOME": str(self.home), "USER": "tester",
            "ELSE_HOME": str(self.home), "ELSE_VERSION": "0.0.6", **overrides,
        }
        # Exercise stage2 and the real config/file operations without package
        # installation, building binaries, starting services, or provider login.
        script = DEFINITIONS + r'''
DISTRO=arch
NO_MSB=1
SKIP_GUEST=1
SERVER_ONLY=1
source_version() { echo 0.0.6; }
installed_version() { basename "$(readlink "$CURRENT")"; }
linux_packages() { :; }
linux_access() { :; }
ensure_rust() { :; }
build_release() { :; }
refresh_provider_paths() { :; }
elsebot() {
    printf '%s %s %s\n' "$1" "$(readlink "$CURRENT")" "$(env_value DATABASE_URL)" >>"$ELSE_HOME/events"
    [ "${FAIL_STOP:-0}" != 1 ]
}
api_running() { return 1; }
backup_if_migrating() { echo backup >>"$ELSE_HOME/events"; }
install_commands() { :; }
setup_service() { :; }
run_setup() {
    printf 'setup %s %s\n' "$(readlink "$CURRENT")" "$(env_value DATABASE_URL)" >>"$ELSE_HOME/events"
    case "$(env_value DATABASE_URL)" in sqlite:*) ;; *) return 1 ;; esac
}
start_api() { echo start >>"$ELSE_HOME/events"; }
check_providers() { :; }
if [ "${FAIL_CONFIG_WRITE:-0}" = 1 ]; then config_set() { return 1; }; fi
stage2
'''
        return subprocess.run(["bash", "-c", script], env=env, capture_output=True, text=True)

    def test_non_sqlite_configuration_switches_to_sqlite_without_old_backup(self):
        result = self.update("postgres://localhost/else")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        url = f"sqlite://{self.home}/data/else.sqlite3"
        self.assertIn(f"DATABASE_URL={url}\n", self.config.read_text())
        self.assertIn("CUSTOM_SETTING=kept\n", self.config.read_text())
        self.assertEqual(self.config.stat().st_mode & 0o777, 0o600)
        self.assertEqual((self.home / "data/owner-token").read_text(), "existing-token")
        self.assertEqual((self.home / "events").read_text().splitlines(), [
            "stop versions/0.0.3 postgres://localhost/else",
            f"setup versions/0.0.6 {url}", "start",
        ])

    def test_sqlite_update_keeps_custom_database_and_takes_backup(self):
        database = self.home / "custom.sqlite3"
        database.write_bytes(b"existing database")
        result = self.update(f"sqlite://{database}")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"DATABASE_URL=sqlite://{database}\n", self.config.read_text())
        self.assertEqual(database.read_bytes(), b"existing database")
        self.assertIn("backup", (self.home / "events").read_text().splitlines())

    def test_stop_failure_leaves_release_and_configuration_unchanged(self):
        result = self.update("legacy://localhost/else", FAIL_STOP="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("DATABASE_URL=legacy://localhost/else\n", self.config.read_text())
        self.assertEqual(os.readlink(self.home / "current"), "versions/0.0.3")

    def test_invalid_override_leaves_release_and_configuration_unchanged(self):
        result = self.update("legacy://localhost/else", ELSE_DATABASE_URL="invalid://else")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ELSE_DATABASE_URL must be a SQLite URL", result.stderr)
        self.assertIn("DATABASE_URL=legacy://localhost/else\n", self.config.read_text())
        self.assertEqual(os.readlink(self.home / "current"), "versions/0.0.3")

    def test_non_sqlite_configuration_uses_explicit_sqlite_override(self):
        url = f"sqlite://{self.home}/custom.sqlite3"
        result = self.update("legacy://localhost/else", ELSE_DATABASE_URL=url)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"DATABASE_URL={url}\n", self.config.read_text())

    def test_retry_after_failed_configuration_write_skips_old_backup(self):
        result = self.update("legacy://localhost/else", FAIL_CONFIG_WRITE="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(os.readlink(self.home / "current"), "versions/0.0.6")
        self.assertIn("DATABASE_URL=legacy://localhost/else\n", self.config.read_text())
        result = self.update(None)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"DATABASE_URL=sqlite://{self.home}/data/else.sqlite3\n", self.config.read_text())
        self.assertNotIn("backup", (self.home / "events").read_text().splitlines())

    def test_repeating_sqlite_setup_keeps_configuration_and_skips_old_backup(self):
        result = self.update("legacy://localhost/else")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        config = self.config.read_text()
        result = self.update(None)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.config.read_text(), config)
        self.assertNotIn("backup", (self.home / "events").read_text().splitlines())


if __name__ == "__main__":
    unittest.main()
