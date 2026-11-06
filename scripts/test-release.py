#!/usr/bin/env python3
"""Check release archives and publication against disposable Git repositories."""
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tarfile
import tempfile
import textwrap
import tomllib
import unittest
from unittest.mock import patch

import release_binaries as binaries

import release
import guest_release
ROOT = Path(__file__).resolve().parents[1]
VERSION = "0.0.2"

FAKE_GH = '''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

state = Path(os.environ["ELSE_TEST_RELEASE_STATE"])
latest = Path(os.environ["ELSE_TEST_LATEST_RELEASE"])
data = json.loads(state.read_text()) if state.exists() else None
command = sys.argv[2]
if command == "view":
    if sys.argv[3] == "--json":
        if not latest.exists():
            sys.exit("release not found")
        print(json.dumps({"isDraft": False, "assets": [], "tagName": latest.read_text()}))
        sys.exit(0)
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
    if "--latest" in sys.argv:
        latest.write_text(sys.argv[3])
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


class RustSetup(unittest.TestCase):
    def test_preinstalled_rust_without_shell_environment_file(self):
        workflow = Path(__file__).resolve().parents[1] / ".github/workflows/release-tag.yml"
        step = workflow.read_text().split("      - name: Install stable Rust\n", 1)[1]
        script = textwrap.dedent(step.split("        run: |\n", 1)[1].split("      - name:", 1)[0])
        script = script.replace("${{ matrix.target }}", "aarch64-apple-darwin")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bin_dir = root / ".cargo/bin"
            bin_dir.mkdir(parents=True)
            (bin_dir / "rustup").write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$RUSTUP_LOG"\n')
            (bin_dir / "rustc").write_text('#!/bin/sh\necho "rustc 1.91.0 (fixture)"\n')
            for binary in bin_dir.iterdir():
                binary.chmod(0o755)
            (bin_dir / "cut").symlink_to(shutil.which("cut"))
            log = root / "rustup.log"
            output = root / "output"
            github_path = root / "path"
            result = subprocess.run(
                [shutil.which("bash"), "--noprofile", "--norc", "-e", "-o", "pipefail", "-c", script],
                env={"HOME": str(root), "PATH": str(bin_dir), "RUSTUP_LOG": str(log),
                     "GITHUB_OUTPUT": str(output), "GITHUB_PATH": str(github_path)},
                capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("toolchain install stable", log.read_text())
            self.assertIn("default stable", log.read_text())
            self.assertIn("target add aarch64-apple-darwin", log.read_text())
            self.assertEqual(output.read_text(), "version=1.91.0\n")
            self.assertEqual(github_path.read_text(), f"{bin_dir}\n")


class ReleasePreflight(unittest.TestCase):
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
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "tag.gpgsign", "false")
        self.git("remote", "add", "origin", str(self.remote))
        scripts = self.repo / "scripts"
        scripts.mkdir()
        (scripts / "release.sh").write_text(Path(__file__).with_name("release.sh").read_text())
        (self.repo / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["apps/api", "apps/desktop"]\n'
            '[workspace.package]\nversion = "0.0.1"\n')
        self.lock = 'version = 4\n'
        for name in ("api", "desktop"):
            member = self.repo / "apps" / name
            (member / "src").mkdir(parents=True)
            (member / "Cargo.toml").write_text(
                f'[package]\nname = "else-{name}"\nversion.workspace = true\n')
            (member / "src/main.rs").write_text("fn main() {}\n")
            self.lock += f'\n[[package]]\nname = "else-{name}"\nversion = "0.0.1"\n'
        self.lock += ('\n[[package]]\nname = "pinned-dependency"\nversion = "0.0.1"\n'
                      'source = "git+https://example.invalid/dependency#' + 'a' * 40 + '"\n')
        (self.repo / "Cargo.lock").write_text(self.lock)
        self.git("add", ".")
        self.git("commit", "-m", "initial release")
        self.git("tag", "v0.0.1")
        self.git("push", "origin", "main", "--tags")

    def git(self, *arguments):
        return subprocess.run(["git", *arguments], cwd=self.repo, check=True,
                              capture_output=True, text=True).stdout.strip()

    def preflight(self):
        return subprocess.run(["bash", str(self.repo / "scripts/release.sh"), "patch"],
                              input="", capture_output=True, text=True)

    def assert_ready_to_confirm(self, result):
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("Cut release/v0.0.2", result.stdout, result.stdout + result.stderr)
        self.assertIn("no terminal to confirm; pass -y", result.stderr)
        self.assertEqual(self.git("ls-remote", "--heads", "origin", "release/*"), "")

    def test_conflicting_local_tag_does_not_block_release_or_get_overwritten(self):
        self.git("commit", "--allow-empty", "-m", "local work")
        self.git("tag", "-f", "v0.0.1")
        local_tag = self.git("rev-parse", "v0.0.1")
        self.git("reset", "--hard", "origin/main")
        self.assert_ready_to_confirm(self.preflight())
        self.assertEqual(self.git("rev-parse", "v0.0.1"), local_tag)

    def test_remote_tag_blocks_release_even_when_missing_locally(self):
        self.git("tag", "v0.0.2")
        self.git("push", "origin", "v0.0.2")
        self.git("tag", "-d", "v0.0.2")
        result = self.preflight()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("tag v0.0.2 already exists", result.stderr)
        self.assertNotIn("Cut release/", result.stdout)
        self.assertNotIn("v0.0.2", self.git("tag", "--list"))

    def test_local_only_tag_does_not_block_release_or_get_overwritten(self):
        self.git("tag", "v0.0.2")
        local_tag = self.git("rev-parse", "v0.0.2")
        self.assert_ready_to_confirm(self.preflight())
        self.assertEqual(self.git("rev-parse", "v0.0.2"), local_tag)

    def test_preparation_needs_no_cargo_or_dependency_cache(self):
        fake_bin = self.root / "fake-bin"
        fake_bin.mkdir()
        (fake_bin / "cargo").write_text(
            '#!/bin/sh\necho "release preparation unexpectedly invoked Cargo" >&2\nexit 1\n')
        (fake_bin / "cargo").chmod(0o755)
        pr = self.root / "pr-created"
        (fake_bin / "gh").write_text(f'''#!/bin/sh
case "$1 $2" in
"pr list") [ ! -f "{pr}" ] || echo https://example.invalid/pull/1 ;;
"pr create") touch "{pr}"; echo https://example.invalid/pull/1 ;;
*) exit 1 ;;
esac
''')
        (fake_bin / "gh").chmod(0o755)
        environment = dict(os.environ, PATH=f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
                           CARGO_HOME=str(self.root / "empty-cargo-home"))
        for name in ("Cargo.toml", "Cargo.lock"):
            with (self.repo / name).open("a") as file:
                file.write("\n# local uncommitted work\n")
        checkout = {name: (self.repo / name).read_bytes() for name in ("Cargo.toml", "Cargo.lock")}
        worktrees = self.git("worktree", "list", "--porcelain")
        command = ["bash", str(self.repo / "scripts/release.sh"), "-y", "patch"]
        result = subprocess.run(command, env=environment, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("Release pull request: https://example.invalid/pull/1", result.stdout)
        ref = "refs/heads/release/v0.0.2"
        head = self.git("--git-dir", str(self.remote), "rev-parse", ref)
        self.assertEqual(self.git("rev-parse", f"{head}^"), self.git("rev-parse", "origin/main"))
        self.assertEqual(self.git("diff-tree", "--no-commit-id", "--name-only", "-r", head).splitlines(),
                         ["Cargo.lock", "Cargo.toml"])
        manifest = tomllib.loads(self.git("show", f"{head}:Cargo.toml"))
        self.assertEqual(manifest["workspace"]["package"]["version"], "0.0.2")
        expected_lock = self.lock.replace('name = "else-api"\nversion = "0.0.1"',
                                          'name = "else-api"\nversion = "0.0.2"').replace(
                                              'name = "else-desktop"\nversion = "0.0.1"',
                                              'name = "else-desktop"\nversion = "0.0.2"')
        self.assertEqual(self.git("show", f"{head}:Cargo.lock"), expected_lock.strip())
        self.assertEqual({name: (self.repo / name).read_bytes() for name in checkout}, checkout)
        self.assertEqual(self.git("worktree", "list", "--porcelain"), worktrees)
        repeated = subprocess.run(command, env=environment, capture_output=True, text=True, timeout=15)
        self.assertEqual(repeated.returncode, 0, repeated.stdout + repeated.stderr)
        self.assertIn("already exists on origin", repeated.stdout)
        self.assertEqual(self.git("--git-dir", str(self.remote), "rev-parse", ref), head)


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
                        self.assertEqual(info["CFBundleIconFile"], "else-bot.icns")
                        self.assertEqual(tar.extractfile("else.bot.app/Contents/Resources/else-bot.icns").read(),
                                         (ROOT / "media/branding/icons/else-bot.icns").read_bytes())
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
        self.latest = self.root / "latest-release"
        self.latest.write_text("v0.0.1")
        environment = patch.dict(os.environ, {
            "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
            "ELSE_TEST_RELEASE_STATE": str(self.state),
            "ELSE_TEST_LATEST_RELEASE": str(self.latest),
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

    def older_draft_with_newer_tag(self):
        with patch.dict(os.environ, {"ELSE_TEST_PUBLISH_FAIL": "1"}):
            with self.assertRaises(release.Fail):
                self.finalize()
        self.commit_version("0.0.3")
        self.git("tag", "-a", "v0.0.3", "-m", "newer version")
        self.git("push", "origin", "main", "--tags")
        self.git("checkout", "--detach", "v0.0.2")

    def test_resuming_an_older_draft_cannot_downgrade_latest(self):
        self.older_draft_with_newer_tag()
        self.latest.write_text("v0.0.3")
        self.finalize()
        self.assertFalse(json.loads(self.state.read_text())["isDraft"])
        self.assertEqual(self.latest.read_text(), "v0.0.3")

    def test_an_unpublished_newer_tag_does_not_block_latest_promotion(self):
        self.older_draft_with_newer_tag()
        self.finalize()
        self.assertEqual(self.latest.read_text(), "v0.0.2")

    def test_no_previous_github_release(self):
        self.latest.unlink()
        self.finalize()
        self.assertEqual(self.latest.read_text(), "v0.0.2")

    def test_guest_build_fingerprint_ignores_native_publisher_changes(self):
        helper = self.repo / "scripts/guest_release.py"
        helper.parent.mkdir()
        helper.write_text(Path(guest_release.__file__).read_text())
        self.git("add", "scripts")
        self.git("commit", "-m", "guest build helper")
        before = guest_release.source_sha256(guest_release.GUEST)
        publisher = helper.parent / "release.py"
        publisher.write_text(Path(release.__file__).read_text())
        self.git("add", "scripts")
        self.git("commit", "-m", "native publisher change")
        self.assertEqual(guest_release.source_sha256(guest_release.GUEST), before)
        helper.write_text(helper.read_text() + "\n# changed guest build inputs\n")
        self.git("add", "scripts")
        self.git("commit", "-m", "guest helper change")
        self.assertNotEqual(guest_release.source_sha256(guest_release.GUEST), before)


if __name__ == "__main__":
    unittest.main(verbosity=2)
