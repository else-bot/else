#!/usr/bin/env python3
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

CHECK = Path(__file__).resolve().with_name("check_migrations.py")


class MigrationNumberingTests(unittest.TestCase):
    def run_check(self, names):
        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            for name in names:
                (directory / name).write_text("SELECT 1;\n")
            return subprocess.run(
                [sys.executable, str(CHECK), str(directory)],
                cwd=directory, capture_output=True, text=True,
            )

    def test_accepts_contiguous_versions_in_any_directory_order(self):
        result = self.run_check(["0003_last.sql", "0001_first.sql", "0002_middle.sql", "README.md"])
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_duplicate_0017_with_both_filenames(self):
        names = [f"{version:04d}_migration.sql" for version in range(1, 18)]
        names.append("0017_collision.sql")
        result = self.run_check(names)
        self.assertEqual(result.returncode, 1)
        for message in ["duplicate migration prefix 0017", "0017_migration.sql", "0017_collision.sql"]:
            self.assertIn(message, result.stderr)

    def test_rejects_missing_middle_version(self):
        result = self.run_check(["0001_first.sql", "0003_third.sql"])
        self.assertEqual(result.returncode, 1)
        self.assertIn("missing migration prefixes: 0002", result.stderr)

    def test_rejects_missing_first_version(self):
        result = self.run_check(["0002_second.sql"])
        self.assertEqual(result.returncode, 1)
        self.assertIn("missing migration prefixes: 0001", result.stderr)

    def test_duplicate_cannot_hide_a_gap(self):
        result = self.run_check(["0001_first.sql", "0003_third.sql", "0003_collision.sql"])
        self.assertEqual(result.returncode, 1)
        self.assertIn("duplicate migration prefix 0003", result.stderr)
        self.assertIn("missing migration prefixes: 0002", result.stderr)

    def test_rejects_empty_directory(self):
        result = self.run_check([])
        self.assertEqual(result.returncode, 1)
        self.assertIn("no SQL migrations", result.stderr)

    def test_rejects_malformed_sql_filenames(self):
        for name in ["0000_zero.sql", "1_unpadded.sql", "0002.sql", "0002_.sql", "next_change.sql", "20261107000000_timestamp.sql"]:
            with self.subTest(name=name):
                result = self.run_check(["0001_first.sql", name])
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"invalid migration filename {name}", result.stderr)

    def test_rejects_missing_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = subprocess.run(
                [sys.executable, str(CHECK), str(Path(tmp) / "missing")],
                capture_output=True, text=True,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No such file or directory", result.stderr)

    def test_checks_repository_from_another_working_directory(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = subprocess.run(
                [sys.executable, str(CHECK.resolve())], cwd=tmp,
                capture_output=True, text=True,
            )
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
