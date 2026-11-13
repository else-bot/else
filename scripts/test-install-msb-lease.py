#!/usr/bin/env python3
"""Check that the installer releases Microsandbox leases a clock step left in the future."""
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
# The table as msb 0.7.13 creates it (m20260621_000002_create_maintenance_lease).
SCHEMA = """CREATE TABLE "maintenance_lease" ( "name" text NOT NULL PRIMARY KEY, "holder_pid" integer,
"lease_expires_at" datetime_text NOT NULL, "last_completed_at" datetime_text )"""
FUTURE = "2099-01-01 00:00:00.000000001"
PAST = "2020-01-01 00:00:00.000000001"


class Lease(unittest.TestCase):
    def release(self, rows):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            (home / "db").mkdir()
            db = home / "db" / "msb.db"
            with sqlite3.connect(db) as conn:
                conn.execute(SCHEMA)
                conn.executemany("INSERT INTO maintenance_lease VALUES (?, ?, ?, ?)", rows)
            output = subprocess.check_output(
                ["bash", "-c", 'source "$1"; release_stale_msb_leases "$2"', "test",
                 str(ROOT / "install.sh"), str(home)],
                text=True,
            )
            with sqlite3.connect(db) as conn:
                now = conn.execute("SELECT strftime('%Y-%m-%d %H:%M:%f', 'now')").fetchone()[0]
                leases = {name: (pid, expires, completed) for name, pid, expires, completed
                          in conn.execute("SELECT * FROM maintenance_lease")}
            return output, now, leases

    def test_releases_a_released_lease_dated_in_the_future(self):
        output, now, leases = self.release([("install_exclusive", None, FUTURE, FUTURE)])
        pid, expires, completed = leases["install_exclusive"]
        self.assertIsNone(pid)
        self.assertLessEqual(expires, now)
        self.assertEqual(expires, completed)
        self.assertIn("install_exclusive", output)

    def test_keeps_a_lease_a_process_holds(self):
        output, _, leases = self.release([("install_exclusive", 4242, FUTURE, PAST)])
        self.assertEqual(leases["install_exclusive"], (4242, FUTURE, PAST))
        self.assertEqual(output, "")

    def test_keeps_a_lease_released_in_the_past(self):
        output, _, leases = self.release([("install_exclusive", None, PAST, PAST)])
        self.assertEqual(leases["install_exclusive"], (None, PAST, PAST))
        self.assertEqual(output, "")

    def test_skips_a_home_without_a_database(self):
        with tempfile.TemporaryDirectory() as home:
            subprocess.check_call(["bash", "-c", 'source "$1"; release_stale_msb_leases "$2"', "test",
                                   str(ROOT / "install.sh"), home])


if __name__ == "__main__":
    unittest.main()
