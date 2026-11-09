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
releases = json.loads(state.read_text()) if state.exists() else {}
command = sys.argv[2]
tag = sys.argv[3] if len(sys.argv) > 3 and not sys.argv[3].startswith("--") else None
if command == "view":
    if tag is None:
        if not latest.exists():
            sys.exit("release not found")
        print(json.dumps({"isDraft": False, "assets": [], "tagName": latest.read_text()}))
        sys.exit(0)
    if tag not in releases:
        sys.exit("release not found")
    print(json.dumps(releases[tag]))
elif command in ("create", "upload"):
    if command == "create":
        assert "--draft" in sys.argv and "--verify-tag" in sys.argv
        releases[tag] = {"isDraft": True, "assets": [], "tagName": tag}
    data = releases[tag]
    names = {asset["name"] for asset in data["assets"]}
    for argument in sys.argv[4:]:
        if argument.startswith("--"):
            break
        assert Path(argument).is_file(), argument
        names.add(Path(argument).name)
        if os.environ.get("ELSE_TEST_UPLOAD_FAIL") == "1":
            break
    data["assets"] = [{"name": name} for name in sorted(names)]
    state.write_text(json.dumps(releases))
    if os.environ.get("ELSE_TEST_UPLOAD_FAIL") == "1":
        sys.exit("simulated interrupted upload")
elif command == "edit":
    data = releases[tag]
    assert len(data["assets"]) == 7, data
    assert "--draft=false" in sys.argv
    if os.environ.get("ELSE_TEST_PUBLISH_FAIL") == "1":
        sys.exit("simulated publication failure")
    if "--latest" in sys.argv:
        latest.write_text(tag)
    data["isDraft"] = False
    state.write_text(json.dumps(releases))
else:
    sys.exit("unexpected gh call: " + str(sys.argv))
'''


FAKE_DISPATCH_GH = '''#!/usr/bin/env python3
import json
import os
from pathlib import Path
import subprocess
import sys

root = Path(os.environ["ELSE_TEST_GH_ROOT"])
args = sys.argv[1:]
with (root / "gh-calls").open("a") as log:
    log.write(json.dumps(args) + "\\n")
if args[:2] == ["workflow", "run"]:
    if os.environ.get("ELSE_TEST_DISPATCH_FAIL"):
        sys.exit("HTTP 403")
    request = next(a.split("=", 1)[1] for a in args if a.startswith("request="))
    bump = next(a.split("=", 1)[1] for a in args if a.startswith("bump="))
    (root / "runs.json").write_text(json.dumps(
        [{"databaseId": 7, "displayTitle": f"release {bump} [{request}]"},
         {"databaseId": 6, "displayTitle": "release patch [older]"}]))
elif args[:2] == ["run", "list"]:
    runs = root / "runs.json"
    subprocess.run(["jq", "-r", args[args.index("--jq") + 1]],
                   input=runs.read_text() if runs.exists() else "[]", text=True, check=True)
elif args[:2] == ["run", "view"]:
    print("https://example.invalid/actions/runs/" + args[2])
elif args[:2] == ["run", "watch"]:
    sys.exit(1 if os.environ.get("ELSE_TEST_RUN_FAIL") else 0)
else:
    sys.exit("unexpected gh call: " + str(args))
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
        workflow = Path(__file__).resolve().parents[1] / ".github/workflows/release.yml"
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


class GitHubOAuthBuild(unittest.TestCase):
    def script(self, name):
        workflow = (ROOT / ".github/workflows/release.yml").read_text()
        step = workflow.split(f"      - name: {name}\n", 1)[1].split("      - name:", 1)[0]
        return textwrap.dedent(step.split("        run: ", 1)[1].removeprefix("|\n"))

    def prepare(self, root, overrides=None):
        env = {**os.environ, "RUNNER_TEMP": str(root), "GITHUB_ENV": str(root / "environment"),
               "ELSE_GITHUB_CLIENT_ID": "fixture-id", "ELSE_GITHUB_CLIENT_SECRET": "private-fixture-secret"}
        env.update(overrides or {})
        return subprocess.run(["bash", "-e", "-o", "pipefail", "-c",
                               self.script("Prepare GitHub MCP sign-in configuration")],
                              env=env, capture_output=True, text=True, timeout=10)

    def test_release_prepares_private_registration_and_removes_it(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = self.prepare(root)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            registration = root / "else-github-oauth.json"
            self.assertEqual(json.loads(registration.read_text()), {
                "client_id": "fixture-id", "client_secret": "private-fixture-secret"})
            self.assertEqual(registration.stat().st_mode & 0o777, 0o600)
            environment = (root / "environment").read_text()
            self.assertEqual(environment, f"ELSE_GITHUB_OAUTH_CONFIG={registration}\n")
            self.assertNotIn("private-fixture-secret", result.stdout + result.stderr + environment)
            cleanup = subprocess.run(["bash", "-e", "-c", self.script("Remove OAuth registrations")],
                                     env={**os.environ, "RUNNER_TEMP": str(root)},
                                     capture_output=True, text=True, timeout=10)
            self.assertEqual(cleanup.returncode, 0, cleanup.stderr)
            self.assertFalse(registration.exists())

    def test_release_rejects_missing_or_invalid_credentials_without_echoing_them(self):
        for override in [{"ELSE_GITHUB_CLIENT_ID": ""}, {"ELSE_GITHUB_CLIENT_SECRET": ""},
                         {"ELSE_GITHUB_CLIENT_SECRET": "private\ninvalid"}]:
            with self.subTest(override=list(override)), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                result = self.prepare(root, override)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((root / "else-github-oauth.json").exists())
                self.assertNotIn("private", result.stdout + result.stderr)


class ReleaseDispatch(unittest.TestCase):
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
        self.git("remote", "add", "origin", str(self.remote))
        (self.repo / "scripts").mkdir()
        (self.repo / "scripts/release.sh").write_text(Path(__file__).with_name("release.sh").read_text())
        (self.repo / "Cargo.toml").write_text('[workspace.package]\nversion = "0.0.1"\n')
        self.git("add", ".")
        self.git("commit", "-m", "initial")
        self.git("push", "-q", "origin", "main")
        fake_bin = self.root / "fake-bin"
        fake_bin.mkdir()
        (fake_bin / "gh").write_text(FAKE_DISPATCH_GH)
        (fake_bin / "sleep").write_text("#!/bin/sh\nexit 0\n")
        for binary in fake_bin.iterdir():
            binary.chmod(0o755)
        self.environment = dict(os.environ, PATH=f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
                                ELSE_TEST_GH_ROOT=str(self.root))

    def git(self, *arguments):
        return subprocess.run(["git", *arguments], cwd=self.repo, check=True,
                              capture_output=True, text=True).stdout.strip()

    def release(self, *arguments, **overrides):
        return subprocess.run(["bash", str(self.repo / "scripts/release.sh"), *arguments], input="",
                              env={**self.environment, **overrides}, capture_output=True, text=True, timeout=15)

    def gh_calls(self, *prefix):
        log = self.root / "gh-calls"
        calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        return [call for call in calls if call[:len(prefix)] == list(prefix)]

    def remote_refs(self):
        return self.git("--git-dir", str(self.remote), "for-each-ref", "--format=%(refname) %(objectname)")

    def test_dispatches_the_head_of_main_without_pushing(self):
        self.git("commit", "--allow-empty", "-m", "unpushed local work")
        refs = self.remote_refs()
        main = self.git("rev-parse", "origin/main")
        result = self.release("-y", "minor")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        dispatch, = self.gh_calls("workflow", "run")
        self.assertEqual(dispatch[:5], ["workflow", "run", "release.yml", "--ref", "main"])
        self.assertIn("bump=minor", dispatch)
        self.assertIn(f"sha={main}", dispatch)
        self.assertIn("Release run: https://example.invalid/actions/runs/7", result.stdout)
        self.assertEqual(self.gh_calls("run", "watch"), [])
        self.assertEqual(self.remote_refs(), refs)

    def test_releases_a_chosen_commit_on_main(self):
        first = self.git("rev-parse", "HEAD")
        self.git("commit", "--allow-empty", "-m", "newer work")
        self.git("push", "-q", "origin", "main")
        result = self.release("-y", "--sha", first, "patch")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"sha={first}", self.gh_calls("workflow", "run")[0])

    def test_a_commit_outside_main_never_dispatches(self):
        self.git("commit", "--allow-empty", "-m", "unmerged work")
        result = self.release("-y", "--sha", "HEAD", "patch")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("is not on origin/main", result.stderr)
        self.assertEqual(self.gh_calls(), [])

    def test_confirmation_needs_a_terminal_or_yes(self):
        result = self.release("patch")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("as the next patch version", result.stdout)
        self.assertIn("no terminal to confirm; pass -y", result.stderr)
        self.assertEqual(self.gh_calls(), [])

    def test_wait_reports_the_run_result(self):
        result = self.release("-y", "--wait", "patch")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.gh_calls("run", "watch")[0][:3], ["run", "watch", "7"])
        self.assertIn("Released.", result.stdout)
        failed = self.release("-y", "--wait", "patch", ELSE_TEST_RUN_FAIL="1")
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("the release failed; see https://example.invalid/actions/runs/7", failed.stderr)

    def test_dispatch_failure_is_reported(self):
        result = self.release("-y", "patch", ELSE_TEST_DISPATCH_FAIL="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot start the release workflow", result.stderr)
        self.assertEqual(self.gh_calls("run", "list"), [])


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


IMAGE = "ghcr.io/else-bot/else-guest@sha256:" + "a" * 64
CRATES = ("else-api", "else-desktop", "else-tailnet", "else-visual")
# A crates.io package that happens to share the initial workspace version.
REGISTRY_CRATE = ('\n[[package]]\nname = "registry-crate"\nversion = "0.0.1"\n'
                  'source = "registry+https://github.com/rust-lang/crates.io-index"\n')


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
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "tag.gpgsign", "false")
        self.git("remote", "add", "origin", str(self.remote))
        for path, text in (("infra/guest/Dockerfile", "FROM scratch\n"), ("infra/guest-base.json", "{}\n"),
                           ("infra/guest-image.json", "{}\n")):
            (self.repo / path).parent.mkdir(parents=True, exist_ok=True)
            (self.repo / path).write_text(text)
        self.commit_version("0.0.1")
        self.git("tag", "-a", "v0.0.1", "-m", "previous release")
        self.git("push", "-q", "origin", "main", "--tags")
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
        self.record_release("v0.0.1", "0.0.1")
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
        for name, value in (("cmd_check", {"image": IMAGE}), ("verify", {}),
                            ("guest_plan", ("reuse", "b" * 64, IMAGE))):
            patcher = patch.object(release, name, return_value=value)
            setattr(self, name, patcher.start())
            self.addCleanup(patcher.stop)
        self.gate = self.cmd_check

    def git(self, *arguments):
        return subprocess.run(["git", *arguments], cwd=self.repo, check=True, capture_output=True, text=True).stdout.strip()

    def remote_ref(self, ref):
        listed = self.git("ls-remote", "origin", ref).split()
        return listed[0] if listed else None

    def releases(self):
        return json.loads(self.state.read_text()) if self.state.exists() else {}

    def record_release(self, tag, version, draft=False):
        releases = self.releases()
        names = [binaries.archive_name(b, version, p) for p in binaries.PLATFORMS for b in binaries.BINARIES]
        releases[tag] = {"isDraft": draft, "tagName": tag,
                         "assets": [{"name": name} for name in names + ["SHA256SUMS"]]}
        self.state.write_text(json.dumps(releases))

    def commit_version(self, version):
        (self.repo / "Cargo.toml").write_text(f'[workspace.package]\nversion = "{version}"\n')
        (self.repo / "Cargo.lock").write_text("version = 4\n" + "".join(
            f'\n[[package]]\nname = "{name}"\nversion = "{version}"\n' for name in CRATES) + REGISTRY_CRATE)
        self.git("add", ".")
        self.git("commit", "-m", f"version {version}")

    def plan(self, bump="patch", sha=None):
        with contextlib.redirect_stderr(io.StringIO()):
            return release.cmd_release_plan("origin", "main", bump, sha)

    def prepare(self, version=VERSION):
        self.git("fetch", "-q", "origin")
        self.git("checkout", "-q", "--detach", "origin/main")
        return release.cmd_prepare(version, IMAGE, "origin")

    def finalize(self, rev, version=VERSION):
        with contextlib.redirect_stdout(io.StringIO()):
            release.cmd_finalize("origin", "main", version, rev, self.output)

    def test_versions_come_from_the_newest_tag_and_main_is_never_changed(self):
        main = self.remote_ref("refs/heads/main")
        planned = self.plan()
        self.assertEqual(planned, {"state": "release", "version": "0.0.2", "sha": main, "candidate": "",
                                   "guest_state": "reuse", "guest_hash": "b" * 64, "guest_image": IMAGE})
        self.assertEqual(self.plan("minor")["version"], "0.1.0")
        self.assertEqual(self.plan("major")["version"], "1.0.0")
        candidate = self.prepare()
        self.finalize(candidate)
        self.assertEqual(self.remote_ref("refs/heads/main"), main)
        self.assertEqual(self.plan()["version"], "0.0.3")

    def test_prepare_commits_only_the_version_and_pin_on_top_of_main(self):
        main = self.remote_ref("refs/heads/main")
        candidate = self.prepare()
        self.assertEqual(self.git("rev-parse", f"{candidate}^"), main)
        self.assertEqual(self.git("diff-tree", "--no-commit-id", "--name-only", "-r", candidate).splitlines(),
                         ["Cargo.lock", "Cargo.toml", "infra/guest-image.json"])
        self.assertEqual(tomllib.loads(self.git("show", f"{candidate}:Cargo.toml"))["workspace"]["package"]["version"],
                         VERSION)
        lock = tomllib.loads(self.git("show", f"{candidate}:Cargo.lock"))
        self.assertEqual({entry["name"]: entry["version"] for entry in lock["package"]},
                         {**dict.fromkeys(CRATES, VERSION), "registry-crate": "0.0.1"},
                         "every workspace crate is bumped and registry crates are left alone")
        self.assertEqual(json.loads(self.git("show", f"{candidate}:infra/guest-image.json"))["image"], IMAGE)
        self.assertEqual(self.git("show", "-s", "--format=%s", candidate), f"chore(release): v{VERSION}")
        self.assertEqual(self.remote_ref(f"refs/heads/release/v{VERSION}"), candidate)
        self.verify.assert_called_once()
        again = self.prepare()
        self.assertEqual(self.git("rev-parse", f"{again}^{{tree}}", f"{again}^"),
                         self.git("rev-parse", f"{candidate}^{{tree}}", f"{candidate}^"))
        self.assertEqual(self.remote_ref(f"refs/heads/release/v{VERSION}"), again, "a rerun replaces the candidate")

    def test_prepare_refuses_a_dirty_checkout_or_an_older_version(self):
        self.git("checkout", "-q", "--detach", "origin/main")
        with self.assertRaisesRegex(release.Fail, "not newer than 0.0.1"):
            release.cmd_prepare("0.0.1", IMAGE)
        (self.repo / "Cargo.toml").write_text("changed\n")
        with self.assertRaisesRegex(release.Fail, "uncommitted changes"):
            release.cmd_prepare(VERSION, IMAGE)

    def test_release_publishes_the_candidate_and_removes_its_branch(self):
        candidate = self.prepare()
        self.finalize(candidate)
        self.assertEqual(self.git("rev-parse", "v0.0.2^{}"), candidate)
        self.assertFalse(self.releases()["v0.0.2"]["isDraft"])
        self.assertEqual(self.latest.read_text(), "v0.0.2")
        self.assertIsNone(self.remote_ref("refs/heads/release/v0.0.2"))
        self.assertEqual(self.remote_ref("refs/tags/main/v0.0.2"), self.git("rev-parse", f"{candidate}^"))
        self.finalize(candidate)
        self.gate.assert_called_once()

    def test_only_reviewed_code_can_be_released(self):
        self.git("checkout", "-q", "--detach", "origin/main")
        (self.repo / "unreviewed.rs").write_text("fn main() {}\n")
        self.commit_version(VERSION)
        with self.assertRaisesRegex(release.Fail, "changes more than the version and guest pin: unreviewed.rs"):
            self.finalize("HEAD")
        self.git("checkout", "-q", "--detach", "origin/main")
        self.git("commit", "-q", "--allow-empty", "-m", "unmerged work")
        self.commit_version(VERSION)
        with self.assertRaisesRegex(release.Fail, "neither on origin/main nor a release commit"):
            self.finalize("HEAD")
        with self.assertRaisesRegex(release.Fail, "is not on origin/main"):
            self.plan(sha="HEAD~1")
        self.assertIsNone(self.remote_ref("refs/tags/v0.0.2"))

    def tamper(self, candidate, path, extra):
        self.git("checkout", "-q", "--detach", candidate)
        with (self.repo / path).open("a") as file:
            file.write(extra)
        self.git("commit", "-q", "-a", "--amend", "--no-edit")
        return self.git("rev-parse", "HEAD")

    def test_dependency_changes_hidden_in_the_release_files_are_rejected(self):
        candidate = self.prepare()
        for path, extra in (("Cargo.lock", '\n[[package]]\nname = "gpui-base"\nversion = "9.9.9"\n'),
                            ("Cargo.toml", '\n[patch.crates-io]\ngpui-base = { git = "https://example.invalid" }\n')):
            with self.subTest(path=path):
                tampered = self.tamper(candidate, path, extra)
                with self.assertRaisesRegex(release.Fail, f"changes {path} beyond the version bump"):
                    self.finalize(tampered)
        self.assertIsNone(self.remote_ref("refs/tags/v0.0.2"))

    def test_a_tampered_unpublished_tag_is_never_resumed(self):
        tampered = self.tamper(self.prepare(), "Cargo.lock", '\n[[package]]\nname = "evil"\nversion = "1.0.0"\n')
        self.git("tag", "-a", "v0.0.2", "-m", "tampered", tampered)
        self.git("push", "-q", "origin", "v0.0.2")
        with self.assertRaisesRegex(release.Fail, "beyond the version bump"):
            self.plan()

    def test_a_tag_at_another_commit_is_never_moved(self):
        candidate = self.prepare()
        self.git("tag", "-a", "v0.0.2", "-m", "elsewhere", "origin/main")
        self.git("push", "-q", "origin", "v0.0.2")
        with self.assertRaisesRegex(release.Fail, "already exists at"):
            self.finalize(candidate)

    def test_pull_requests_keep_the_workspace_version(self):
        self.git("checkout", "-q", "-b", "feature", "origin/main")
        self.git("commit", "-q", "--allow-empty", "-m", "feature")
        with contextlib.redirect_stdout(io.StringIO()):
            guest_release.cmd_gate("HEAD", "origin/main")
        self.commit_version(VERSION)
        with self.assertRaisesRegex(guest_release.Fail, "pull requests keep the workspace version"):
            guest_release.cmd_gate("HEAD", "origin/main")

    def test_missing_platform_prevents_tagging(self):
        candidate = self.prepare()
        (self.output / binaries.archive_name("else-api", VERSION, "linux-arm64")).unlink()
        with self.assertRaisesRegex(ValueError, "missing release archive"):
            self.finalize(candidate)
        self.assertIsNone(self.remote_ref("refs/tags/v0.0.2"))
        self.assertNotIn("v0.0.2", self.releases())
        self.gate.assert_not_called()

    def test_guest_image_failure_prevents_tagging(self):
        candidate = self.prepare()
        self.gate.side_effect = release.Fail("guest image failed verification")
        with self.assertRaisesRegex(release.Fail, "guest image failed verification"):
            self.finalize(candidate)
        self.assertIsNone(self.remote_ref("refs/tags/v0.0.2"))
        self.assertNotIn("v0.0.2", self.releases())

    def assert_resumes(self, candidate):
        self.assertTrue(self.releases()["v0.0.2"]["isDraft"])
        planned = self.plan("minor")
        self.assertEqual((planned["state"], planned["version"], planned["candidate"]), ("resume", VERSION, candidate))
        self.finalize(planned["candidate"])
        self.assertEqual(self.git("rev-parse", "v0.0.2^{}"), candidate)
        self.assertEqual(self.plan()["state"], "release")
        self.gate.assert_called_once()

    def test_interrupted_upload_resumes_the_same_tag(self):
        candidate = self.prepare()
        with patch.dict(os.environ, {"ELSE_TEST_UPLOAD_FAIL": "1"}):
            with self.assertRaisesRegex(release.Fail, "uploading its release binaries failed"):
                self.finalize(candidate)
        self.assert_resumes(candidate)

    def test_publication_failure_keeps_a_resumable_draft(self):
        candidate = self.prepare()
        with patch.dict(os.environ, {"ELSE_TEST_PUBLISH_FAIL": "1"}):
            with self.assertRaisesRegex(release.Fail, "publishing its GitHub release failed"):
                self.finalize(candidate)
        self.assert_resumes(candidate)

    def test_a_tag_without_a_release_resumes(self):
        candidate = self.prepare()
        self.git("tag", "-a", "v0.0.2", "-m", "tag created before interruption", candidate)
        self.git("push", "-q", "origin", "v0.0.2")
        planned = self.plan()
        self.assertEqual((planned["state"], planned["candidate"]), ("resume", candidate))
        self.finalize(candidate)
        self.assertFalse(self.releases()["v0.0.2"]["isDraft"])
        self.assertEqual(self.remote_ref("refs/tags/main/v0.0.2"), self.git("rev-parse", f"{candidate}^"))

    def test_a_release_base_tag_is_never_moved(self):
        candidate = self.prepare()
        self.git("push", "-q", "origin", f"{candidate}:refs/tags/main/v0.0.2")
        with self.assertRaisesRegex(release.Fail, "main/v0.0.2 already exists at"):
            self.finalize(candidate)

    def older_draft_with_newer_tag(self):
        candidate = self.prepare()
        with patch.dict(os.environ, {"ELSE_TEST_PUBLISH_FAIL": "1"}):
            with self.assertRaises(release.Fail):
                self.finalize(candidate)
        newer = self.prepare("0.0.3")
        self.git("tag", "-a", "v0.0.3", "-m", "newer version", newer)
        self.git("push", "-q", "origin", "v0.0.3")
        return candidate

    def test_resuming_an_older_draft_cannot_downgrade_latest(self):
        candidate = self.older_draft_with_newer_tag()
        self.latest.write_text("v0.0.3")
        self.finalize(candidate)
        self.assertFalse(self.releases()["v0.0.2"]["isDraft"])
        self.assertEqual(self.latest.read_text(), "v0.0.3")

    def test_an_unpublished_newer_tag_does_not_block_latest_promotion(self):
        candidate = self.older_draft_with_newer_tag()
        self.finalize(candidate)
        self.assertEqual(self.latest.read_text(), "v0.0.2")

    def test_no_previous_github_release(self):
        self.latest.unlink()
        self.finalize(self.prepare())
        self.assertEqual(self.latest.read_text(), "v0.0.2")

    def test_guest_build_fingerprint_ignores_release_orchestration(self):
        paths = ("scripts/release.py", "scripts/guest_release.py", "scripts/guest-release.py",
                 ".github/workflows/guest-image.yml")
        guest = self.repo / "infra/guest/Dockerfile"
        guest.write_text("FROM scratch\n")
        for path in paths:
            file = self.repo / path
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text("original orchestration\n")
        self.git("add", ".")
        self.git("commit", "-m", "guest build inputs")
        before = guest_release.source_sha256(guest_release.GUEST)
        self.commit_version("0.0.3")
        self.assertEqual(guest_release.source_sha256(guest_release.GUEST), before)
        for path in paths:
            with self.subTest(path=path):
                (self.repo / path).write_text("updated orchestration\n")
                self.git("add", path)
                self.git("commit", "-m", "release orchestration change")
                self.assertEqual(guest_release.source_sha256(guest_release.GUEST), before)

    def test_guest_build_fingerprint_tracks_image_inputs(self):
        paths = ("infra/guest/Dockerfile", "infra/guest/else-init", "infra/guest-base.json")
        for path in paths:
            file = self.repo / path
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text("original build input\n")
        self.git("add", "infra")
        self.git("commit", "-m", "guest build inputs")
        for path in paths:
            with self.subTest(path=path):
                before = guest_release.source_sha256(guest_release.GUEST)
                (self.repo / path).write_text("updated build input\n")
                self.git("add", path)
                self.git("commit", "-m", "guest input change")
                self.assertNotEqual(guest_release.source_sha256(guest_release.GUEST), before)
        before = guest_release.source_sha256(guest_release.GUEST)
        with patch.dict(guest_release.GUEST.build_args, {"IMAGE_TAG": "else-guest:4"}):
            self.assertNotEqual(guest_release.source_sha256(guest_release.GUEST), before)


if __name__ == "__main__":
    unittest.main(verbosity=2)
