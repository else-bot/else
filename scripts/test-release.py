#!/usr/bin/env python3
"""Check release archives and publication against disposable Git repositories."""
import contextlib
import hashlib
import importlib
import io
import json
import os
from pathlib import Path
import plistlib
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import release_binaries as binaries

release = importlib.import_module("guest-release")
VERSION = "0.0.2"

FAKE_GH = '''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

state = Path(os.environ["ELSE_TEST_RELEASE_STATE"])
data = json.loads(state.read_text()) if state.exists() else None
command = sys.argv[2]
if command == "view":
    if data is None:
        sys.exit("release not found")
    print(json.dumps(data))
elif command in ("create", "upload"):
    if command == "create":
        assert "--draft" in sys.argv and "--verify-tag" in sys.argv
        data = {"isDraft": True, "assets": []}
    names = {asset["name"] for asset in data["assets"]}
    for argument in sys.argv[4:]:
        if argument.startswith("--"):
            break
        assert Path(argument).is_file(), argument
        names.add(Path(argument).name)
        if os.environ.get("ELSE_TEST_UPLOAD_FAIL") == "1":
            break
    data["assets"] = [{"name": name} for name in sorted(names)]
    state.write_text(json.dumps(data))
    if os.environ.get("ELSE_TEST_UPLOAD_FAIL") == "1":
        sys.exit("simulated interrupted upload")
elif command == "edit":
    assert len(data["assets"]) == 7, data
    assert "--draft=false" in sys.argv
    if os.environ.get("ELSE_TEST_PUBLISH_FAIL") == "1":
        sys.exit("simulated publication failure")
    data["isDraft"] = False
    state.write_text(json.dumps(data))
else:
    sys.exit("unexpected gh call: " + str(sys.argv))
'''


def make_binaries(directory):
    directory.mkdir()
    for binary in binaries.BINARIES:
        path = directory / binary
        path.write_bytes(f"fixture for {binary}".encode())
        path.chmod(0o755)


def package_all(source, output):
    with contextlib.redirect_stdout(io.StringIO()):
        for platform in binaries.PLATFORMS:
            binaries.package(VERSION, platform, source, output)


class Archives(unittest.TestCase):
    def test_archives_and_checksums(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, output = root / "bin", root / "dist"
            make_binaries(source)
            package_all(source, output)
            assets = binaries.release_assets(VERSION, output)
            self.assertEqual(len(assets), 7)
            for archive in assets[:-1]:
                binary = next(binary for binary in binaries.BINARIES if archive.name.startswith(binary))
                bundled = binary == "else-desktop" and "macos-arm64" in archive.name
                executable = "else.bot.app/Contents/MacOS/else-desktop" if bundled else binary
                with tarfile.open(archive) as tar:
                    self.assertEqual(tar.extractfile(executable).read(), (source / binary).read_bytes())
                    self.assertEqual(tar.getmember(executable).mode, 0o755)
                    if bundled:
                        info = plistlib.loads(tar.extractfile("else.bot.app/Contents/Info.plist").read())
                        self.assertEqual(info["CFBundleShortVersionString"], VERSION)
                        self.assertEqual(info["CFBundleExecutable"], binary)
                        self.assertNotIn("else.bot.app/Contents/Resources/installation.json", tar.getnames())
            for line in assets[-1].read_text().splitlines():
                digest, name = line.split("  ")
                self.assertEqual(digest, hashlib.sha256((output / name).read_bytes()).hexdigest())
            package_all(source, output)
            self.assertEqual(len(list(output.glob("*.tar.gz"))), 6)

    def test_missing_binary_does_not_produce_a_partial_platform(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, output = root / "bin", root / "dist"
            make_binaries(source)
            (source / "else-desktop").unlink()
            with self.assertRaisesRegex(ValueError, "missing executable"):
                binaries.package(VERSION, "linux-amd64", source, output)
            self.assertEqual(list(output.iterdir()), [])

    def test_interrupted_packaging_preserves_the_previous_archive(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source, output = root / "bin", root / "dist"
            make_binaries(source)
            package_all(source, output)
            archive = output / binaries.archive_name("else-api", VERSION, "linux-amd64")
            previous = archive.read_bytes()
            with patch.object(tarfile.TarFile, "addfile", side_effect=OSError("disk full")):
                with self.assertRaisesRegex(OSError, "disk full"):
                    binaries.package(VERSION, "linux-amd64", source, output)
            self.assertEqual(archive.read_bytes(), previous)
            self.assertEqual(len(list(output.iterdir())), 6)


class Publication(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.remote = self.root / "origin.git"
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "--bare", str(self.remote))
        self.git("init", "-b", "main")
        self.git("config", "user.email", "release-test@example.invalid")
        self.git("config", "user.name", "Release test")
        self.git("remote", "add", "origin", str(self.remote))
        self.commit_version("0.0.1")
        self.git("tag", "-a", "v0.0.1", "-m", "previous release")
        self.commit_version(VERSION)
        self.git("push", "origin", "main", "--tags")
        self.source, self.output = self.root / "bin", self.root / "dist"
        make_binaries(self.source)
        package_all(self.source, self.output)
        fake_bin = self.root / "fake-bin"
        fake_bin.mkdir()
        (fake_bin / "gh").write_text(FAKE_GH)
        (fake_bin / "gh").chmod(0o755)
        self.state = self.root / "release.json"
        environment = patch.dict(os.environ, {
            "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
            "ELSE_TEST_RELEASE_STATE": str(self.state),
        })
        environment.start()
        self.addCleanup(environment.stop)
        previous = Path.cwd()
        os.chdir(self.repo)
        self.addCleanup(os.chdir, previous)
        image = "ghcr.io/else-bot/else-guest@sha256:" + "a" * 64
        gate = patch.object(release, "cmd_check", return_value={"image": image})
        self.gate = gate.start()
        self.addCleanup(gate.stop)

    def git(self, *arguments):
        return subprocess.run(["git", *arguments], cwd=self.repo, check=True, capture_output=True, text=True).stdout.strip()

    def commit_version(self, version):
        (self.repo / "Cargo.toml").write_text(f'[workspace.package]\nversion = "{version}"\n')
        self.git("add", "Cargo.toml")
        self.git("commit", "-m", f"version {version}")

    def plan(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            release.cmd_release_plan("origin", "main")
        return output.getvalue().strip()

    def finalize(self):
        with contextlib.redirect_stdout(io.StringIO()):
            release.cmd_finalize("origin", "main", self.output)

    def test_new_release_and_ordinary_push(self):
        self.assertEqual(self.plan(), "state=build")
        self.finalize()
        self.assertEqual(self.git("rev-parse", "v0.0.2^{}"), self.git("rev-parse", "HEAD"))
        self.assertFalse(json.loads(self.state.read_text())["isDraft"])
        self.gate.assert_called_once()
        self.assertEqual(self.plan(), "state=skip")
        self.finalize()
        self.assertEqual(self.plan(), "state=skip")
        self.git("commit", "--allow-empty", "-m", "ordinary push")
        self.git("push", "origin", "main")
        self.assertEqual(self.plan(), "state=skip")
        self.finalize()
        self.assertEqual(self.gate.call_count, 1)

    def test_missing_platform_prevents_tagging(self):
        (self.output / binaries.archive_name("else-api", VERSION, "linux-arm64")).unlink()
        with self.assertRaisesRegex(ValueError, "missing release archive"):
            self.finalize()
        self.assertNotIn("refs/tags/v0.0.2", self.git("ls-remote", "--tags", "origin"))
        self.assertFalse(self.state.exists())
        self.gate.assert_not_called()

    def test_interrupted_upload_resumes_the_same_tag(self):
        with patch.dict(os.environ, {"ELSE_TEST_UPLOAD_FAIL": "1"}):
            with self.assertRaisesRegex(release.Fail, "uploading its release binaries failed"):
                self.finalize()
        tagged = self.git("rev-parse", "v0.0.2^{}")
        self.assertTrue(json.loads(self.state.read_text())["isDraft"])
        self.assertEqual(self.plan(), "state=build")
        self.finalize()
        self.assertEqual(self.git("rev-parse", "v0.0.2^{}"), tagged)
        self.assertEqual(self.plan(), "state=skip")
        self.gate.assert_called_once()

    def test_tag_without_a_release_can_resume(self):
        self.git("tag", "-a", "v0.0.2", "-m", "tag created before interruption")
        self.git("push", "origin", "v0.0.2")
        self.assertEqual(self.plan(), "state=build")
        self.finalize()
        self.assertEqual(self.plan(), "state=skip")

    def test_guest_image_failure_prevents_tagging(self):
        self.gate.side_effect = release.Fail("guest image failed verification")
        with self.assertRaisesRegex(release.Fail, "guest image failed verification"):
            self.finalize()
        self.assertNotIn("refs/tags/v0.0.2", self.git("ls-remote", "--tags", "origin"))
        self.assertFalse(self.state.exists())

    def test_publication_failure_keeps_a_resumable_draft(self):
        with patch.dict(os.environ, {"ELSE_TEST_PUBLISH_FAIL": "1"}):
            with self.assertRaisesRegex(release.Fail, "publishing its GitHub release failed"):
                self.finalize()
        self.assertTrue(json.loads(self.state.read_text())["isDraft"])
        self.assertEqual(self.plan(), "state=build")
        self.finalize()
        self.assertEqual(self.plan(), "state=skip")

    def test_unmerged_commit_cannot_release(self):
        self.git("commit", "--allow-empty", "-m", "unmerged work")
        with self.assertRaisesRegex(release.Fail, "is not on origin/main"):
            self.plan()
        with self.assertRaisesRegex(release.Fail, "is not on origin/main"):
            self.finalize()
        self.assertNotIn("refs/tags/v0.0.2", self.git("ls-remote", "--tags", "origin"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
