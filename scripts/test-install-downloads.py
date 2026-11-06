#!/usr/bin/env python3
"""Exercise prebuilt installation with local Git tags and HTTP release assets."""
import contextlib
from functools import partial
import http.server
import io
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import threading
import unittest

import release_binaries

ROOT = Path(__file__).resolve().parents[1]
VERSIONS = ("0.0.1", "0.0.2")

SETUP = r'''
set -euo pipefail
source "$TEST_INSTALLER"
detect_platform() {
    DISTRO="$TEST_DISTRO"
    DISTRO_NAME=fixture
    OS=Linux
    ARCH="$TEST_ARCH"
}
linux_packages() { apt_packages > "$TEST_PACKAGES"; }
ensure_bootstrap_tools() { command -v git curl >/dev/null; }
if [ "${TEST_SERVER:-0}" = 1 ]; then
    linux_access() { :; }
    ensure_msb() { :; }
    decide_guest_image() { GUEST_IMAGE_STATE=skip; }
    refresh_provider_paths() { :; }
    check_providers() { :; }
    api_running() { [ "${TEST_RUNNING:-1}" = 1 ]; }
    prepare_running_api() { echo gate >> "$ELSE_HOME/server-actions"; }
    elsebot() { echo "$*" >> "$ELSE_HOME/server-actions"; }
    run_setup() { [ "${TEST_FAIL_SETUP:-0}" != 1 ]; }
    start_api() { echo ready >> "$ELSE_HOME/server-actions"; }
fi
'''

ACTION = r'''
if [ "$TEST_ACTION" = main ]; then
    main "$@"
else
    detect_platform
    COMPONENTS="$TEST_COMPONENTS"
    install_release "$ELSE_VERSION"
    switch_to "$ELSE_VERSION"
fi
'''

HARNESS = SETUP + ACTION
TAGGED_INSTALLER = 'if [ "${1:-}" = --help ]; then echo --from-source app-current; exit 0; fi\n' + HARNESS


def executable(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(0o755)


class Downloads(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Installer test")
        self.git("config", "user.email", "installer@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "tag.gpgsign", "false")
        (self.repo / "scripts").mkdir()
        for name in ("elsebot", "macos_app.py"):
            shutil.copy2(ROOT / "scripts" / name, self.repo / "scripts" / name)
        shutil.copytree(ROOT / "media", self.repo / "media")
        (self.repo / "install.sh").write_text(TAGGED_INSTALLER)
        for version in VERSIONS:
            (self.repo / "Cargo.toml").write_text(f'[workspace.package]\nversion = "{version}"\n')
            self.git("add", "-A")
            self.git("commit", "-qm", version)
            self.git("tag", f"v{version}")
            self.assets(version)
        latest = self.root / "releases/latest/download"
        latest.mkdir(parents=True)
        shutil.copy2(self.root / "releases/download/v0.0.2/SHA256SUMS", latest / "SHA256SUMS")
        self.requests = []
        requests = self.requests

        class Handler(http.server.SimpleHTTPRequestHandler):
            def do_GET(self):
                requests.append(self.path)
                super().do_GET()

            def log_message(self, *_):
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), partial(Handler, directory=str(self.root)))
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(thread.join)
        self.addCleanup(server.shutdown)
        self.home = self.root / "home"
        self.home.mkdir()
        self.installation = self.home / "installation"
        self.environment = dict(os.environ, HOME=str(self.home), XDG_CONFIG_HOME=str(self.home / ".config"),
                                XDG_DATA_HOME=str(self.home / ".local/share"), ELSE_HOME=str(self.installation),
                                ELSE_REPO=str(self.repo), ELSE_RELEASES_URL=f"http://127.0.0.1:{server.server_port}/releases",
                                ELSE_INSTALL_NO_SYSTEMD="1", ELSE_AUTOSTART="0",
                                TEST_INSTALLER=str(ROOT / "install.sh"), TEST_DISTRO="debian", TEST_ARCH="x86_64",
                                TEST_ACTION="main", TEST_COMPONENTS="both", TEST_PACKAGES=str(self.root / "packages"))
        for key in ("ELSE_VERSION", "ELSE_INSTALL_STAGE", "ELSE_INSTALL_TEST_CACHE", "ELSE_INSTALL_TEST_PROFILE"):
            self.environment.pop(key, None)

    def git(self, *args):
        return subprocess.run(["git", *args], cwd=self.repo, capture_output=True, check=True, text=True)

    def assets(self, version, binary_version=None):
        source = self.root / f"bin-{version}"
        for name in release_binaries.BINARIES:
            executable(source / name, f"#!/bin/sh\nprintf '%s\\n' '{name} {binary_version or version}'\n")
        output = self.root / f"releases/download/v{version}"
        with contextlib.redirect_stdout(io.StringIO()):
            for platform in release_binaries.PLATFORMS:
                release_binaries.package(version, platform, source, output)
        release_binaries.release_assets(version, output)

    def run_installer(self, *args, success=True, before_action="", **environment):
        result = subprocess.run(["bash", "-c", SETUP + before_action + ACTION, "installer-test", *args],
                                env=dict(self.environment, **environment), capture_output=True, text=True)
        if success:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def test_latest_published_app_install_and_repeat(self):
        (self.repo / "Cargo.toml").write_text('[workspace.package]\nversion = "0.0.3"\n')
        self.git("add", "Cargo.toml")
        self.git("commit", "-qm", "unpublished release")
        self.git("tag", "v0.0.3")
        self.run_installer("--yes", "--app-only")
        self.assertEqual((self.installation / "current").readlink(), Path("versions/0.0.2"))
        self.assertTrue((self.installation / "current/bin/else-desktop").is_file())
        self.assertFalse((self.installation / "current/bin/else-api").exists())
        self.assertFalse((self.installation / "toolchain").exists())
        packages = (self.root / "packages").read_text().split()
        self.assertNotIn("build-essential", packages)
        self.assertFalse(any(package.endswith("-dev") for package in packages))
        self.assertEqual(self.requests, ["/releases/latest/download/SHA256SUMS",
                                       "/releases/download/v0.0.2/SHA256SUMS",
                                       "/releases/download/v0.0.2/else-desktop-v0.0.2-linux-amd64.tar.gz"])
        self.requests.clear()
        self.run_installer("--yes", "--app-only", "--version", "0.0.2")
        self.assertEqual(self.requests, [])

    def test_all_platforms_and_components(self):
        for distro, arch, platform in (("debian", "x86_64", "linux-amd64"),
                                      ("ubuntu", "aarch64", "linux-arm64"),
                                      ("macos", "aarch64", "macos-arm64")):
            for components in ("server", "app", "both"):
                with self.subTest(platform=platform, components=components):
                    destination = self.home / f"{platform}-{components}"
                    self.requests.clear()
                    self.run_installer(TEST_ACTION="release", TEST_COMPONENTS=components,
                                       TEST_DISTRO=distro, TEST_ARCH=arch, ELSE_HOME=str(destination), ELSE_VERSION="0.0.2")
                    expected = {"else-api", "else-desktop"} if components == "both" else {
                        "else-api" if components == "server" else "else-desktop"}
                    self.assertEqual({path.name for path in (destination / "current/bin").iterdir()}, expected)
                    self.assertEqual(set(self.requests), {"/releases/download/v0.0.2/SHA256SUMS", *[
                        f"/releases/download/v0.0.2/{name}-v0.0.2-{platform}.tar.gz" for name in expected]})

    def test_macos_app_bundle_has_installation_owner(self):
        self.run_installer("--yes", "--app-only", "--version", "0.0.2", TEST_DISTRO="macos", TEST_ARCH="aarch64")
        app = self.installation / "Applications/else.bot.app/Contents"
        self.assertTrue((app / "MacOS/else-desktop").is_file())
        info = plistlib.loads((app / "Info.plist").read_bytes())
        self.assertEqual(info["CFBundleIconFile"], "else-bot.icns")
        self.assertEqual((app / "Resources/else-bot.icns").read_bytes(),
                         (ROOT / "media/branding/icons/else-bot.icns").read_bytes())
        self.assertEqual(json.loads((app / "Resources/installation.json").read_text())["home"], str(self.installation))
        self.assertFalse((self.installation / "toolchain").exists())

    def test_linux_launcher_has_icon(self):
        self.run_installer("--yes", "--app-only", "--version", "0.0.2")
        share = self.home / ".local/share"
        self.assertIn("Icon=else-bot\n", (share / "applications/else-bot.desktop").read_text())
        self.assertEqual((share / "icons/hicolor/scalable/apps/else-bot.svg").read_bytes(),
                         (ROOT / "media/branding/else-bot.svg").read_bytes())

    def test_update_keeps_previous_version(self):
        self.run_installer("--yes", "--app-only", "--version", "0.0.1")
        self.run_installer("--yes", "--app-only", "--version", "0.0.2")
        self.assertEqual((self.installation / "current").readlink(), Path("versions/0.0.2"))
        self.assertTrue((self.installation / "versions/0.0.1/bin/else-desktop").is_file())

    def pointer(self, name):
        return (self.installation / name).readlink()

    def install_both(self, version="0.0.1", **environment):
        self.run_installer("--yes", "--both", "--version", version, TEST_SERVER="1", **environment)

    def test_app_update_preserves_server_and_installed_inventory(self):
        self.install_both()
        actions = self.installation / "server-actions"
        actions.unlink()
        settings = self.installation / "config/elsebot.env"
        settings.write_text("ELSEBOT_SERVICE=custom-api\nELSEBOT_MANAGER=systemd\nELSEBOT_AUTOSTART=1\n")
        self.run_installer("--yes", "--app-only", "--no-autostart", "--version", "0.0.2")
        self.assertEqual(self.pointer("app-current"), Path("versions/0.0.2"))
        self.assertEqual(self.pointer("server-current"), Path("versions/0.0.1"))
        self.assertEqual(self.pointer("current"), Path("versions/0.0.1"))
        self.assertEqual((self.installation / "config/components").read_text(), "both\n")
        self.assertEqual(settings.read_text(), "ELSEBOT_SERVICE=custom-api\nELSEBOT_MANAGER=systemd\nELSEBOT_AUTOSTART=1\n")
        self.assertFalse(actions.exists(), "app updates must never call the server")
        self.assertEqual((self.installation / "bin/else-api").readlink(), Path("../server-current/bin/else-api"))
        self.assertEqual((self.installation / "bin/else-desktop").readlink(), Path("../app-current/bin/else-desktop"))

    def test_server_update_keeps_app_and_macos_publication(self):
        self.install_both(TEST_DISTRO="macos", TEST_ARCH="aarch64")
        app = self.installation / "Applications/else.bot.app/Contents/MacOS/else-desktop"
        original = app.read_bytes()
        app_pointer_time = (self.installation / "app-current").lstat().st_mtime_ns
        desktop_link_time = (self.installation / "bin/else-desktop").lstat().st_mtime_ns
        metadata = self.installation / "config/elsebot.env"
        app_setting = next(line for line in metadata.read_text().splitlines() if line.startswith("ELSEBOT_APP="))
        self.run_installer("--yes", "--server-only", "--version", "0.0.2", TEST_SERVER="1",
                           TEST_DISTRO="macos", TEST_ARCH="aarch64")
        self.assertEqual(self.pointer("app-current"), Path("versions/0.0.1"))
        self.assertEqual(self.pointer("server-current"), Path("versions/0.0.2"))
        self.assertEqual(self.pointer("current"), Path("versions/0.0.2"))
        self.assertEqual(app.read_bytes(), original)
        self.assertEqual((self.installation / "app-current").lstat().st_mtime_ns, app_pointer_time)
        self.assertEqual((self.installation / "bin/else-desktop").lstat().st_mtime_ns, desktop_link_time)
        self.assertIn(app_setting, metadata.read_text().splitlines())
        self.assertEqual((self.installation / "config/components").read_text(), "both\n")

    def test_legacy_combined_migration_pins_both_before_app_activation(self):
        self.install_both()
        for name in ("app-current", "server-current"):
            (self.installation / name).unlink()
        for binary in ("else-api", "else-desktop"):
            link = self.installation / f"bin/{binary}"
            link.unlink()
            link.symlink_to(f"../current/bin/{binary}")
        target = self.installation / "versions/0.0.1"
        (self.installation / "current").unlink()
        (self.installation / "current").symlink_to(target)
        self.run_installer("--yes", "--app-only", "--version", "0.0.2")
        self.assertEqual(self.pointer("server-current"), target)
        self.assertEqual(self.pointer("app-previous"), target)
        self.assertEqual(self.pointer("current"), target)
        self.assertEqual(self.pointer("app-current"), Path("versions/0.0.2"))
        self.assertEqual((self.installation / "bin/else-api").readlink(), Path("../server-current/bin/else-api"))
        self.assertEqual((self.installation / "bin/else-desktop").readlink(), Path("../app-current/bin/else-desktop"))
        self.run_installer("--yes", "--app-only", "--version", "0.0.2")
        self.assertEqual(self.pointer("server-current"), target, "migration retry must keep the server pinned")

    def test_adding_components_keeps_the_existing_components_version(self):
        self.run_installer("--yes", "--server-only", "--version", "0.0.1", TEST_SERVER="1")
        self.run_installer("--yes", "--app-only", "--version", "0.0.2")
        self.assertEqual(self.pointer("server-current"), Path("versions/0.0.1"))
        self.assertEqual(self.pointer("app-current"), Path("versions/0.0.2"))
        self.assertEqual((self.installation / "config/components").read_text(), "both\n")

    def test_unsafe_old_component_installer_is_refused(self):
        self.install_both()
        (self.repo / "install.sh").write_text('if [ "${1:-}" = --help ]; then echo --from-source; exit 0; fi\nexit 73\n')
        self.git("add", "install.sh")
        self.git("commit", "-qm", "old installer")
        self.git("tag", "-f", "v0.0.2")
        result = self.run_installer("--yes", "--app-only", "--version", "0.0.2", success=False)
        self.assertIn("predates component pointers", result.stderr)
        self.assertEqual(self.pointer("server-current"), Path("versions/0.0.1"))
        self.assertEqual(self.pointer("app-current"), Path("versions/0.0.1"))
        self.assertFalse((self.installation / "run/install.lock").exists())

    def test_command_consumers_use_their_components_release(self):
        self.install_both()
        self.run_installer("--yes", "--app-only", "--version", "0.0.2")
        for arguments, expected in ((["version"], "0.0.1"), (["api", "--version"], "else-api 0.0.1"),
                                    (["desktop", "--version"], "else-desktop 0.0.2")):
            result = subprocess.run([str(self.installation / "bin/elsebot"), *arguments],
                                    env=self.environment, capture_output=True, text=True, check=True)
            self.assertEqual(result.stdout.strip(), expected)
        stubs = self.root / "stubs"
        executable(stubs / "systemctl", "#!/bin/sh\nexit 0\n")
        metadata = self.installation / "config/elsebot.env"
        metadata.write_text("ELSEBOT_SERVICE=fixture-api\nELSEBOT_MANAGER=systemd\nELSEBOT_AUTOSTART=0\n")
        subprocess.run([str(self.installation / "bin/elsebot"), "service-install"], check=True,
                       env=dict(self.environment, PATH=f"{stubs}:{os.environ['PATH']}"), capture_output=True)
        unit = self.home / ".config/systemd/user/fixture-api.service"
        self.assertIn(f"ExecStart={self.installation}/server-current/bin/else-api serve", unit.read_text())
        executable(stubs / "plutil", "#!/bin/sh\nexit 0\n")
        metadata.write_text("ELSEBOT_SERVICE=fixture-api\nELSEBOT_MANAGER=launchd\nELSEBOT_AUTOSTART=0\n")
        subprocess.run([str(self.installation / "bin/elsebot"), "service-install"], check=True,
                       env=dict(self.environment, PATH=f"{stubs}:{os.environ['PATH']}"), capture_output=True)
        plist = self.installation / "config/bot.else.fixture-api.plist"
        self.assertEqual(plistlib.loads(plist.read_bytes())["ProgramArguments"],
                         [f"{self.installation}/server-current/bin/else-api", "serve"])

    def test_default_update_still_updates_both_installed_components(self):
        self.install_both()
        self.run_installer("--yes", "--app-only", "--version", "0.0.2")
        self.run_installer("--yes", "--version", "0.0.2", TEST_SERVER="1")
        self.assertEqual(self.pointer("app-current"), Path("versions/0.0.2"))
        self.assertEqual(self.pointer("server-current"), Path("versions/0.0.2"))

    def test_failed_server_update_restores_each_selected_component_and_rollback(self):
        self.install_both()
        self.run_installer("--yes", "--app-only", "--version", "0.0.2")
        pointers = {name: self.pointer(name) for name in ("app-current", "server-current", "app-previous", "current")}
        for component in ("--server-only", "--both"):
            with self.subTest(component=component):
                self.run_installer("--yes", component, "--version", "0.0.2", success=False,
                                   TEST_SERVER="1", TEST_FAIL_SETUP="1")
                self.assertEqual({name: self.pointer(name) for name in pointers}, pointers)
                self.assertFalse((self.installation / "server-previous").exists())
        self.assertTrue((self.installation / "versions/0.0.1/bin/else-api").is_file())
        self.assertTrue((self.installation / "versions/0.0.2/bin/else-desktop").is_file())

    def test_prune_retains_all_active_and_previous_component_releases(self):
        self.install_both()
        for version in ("0.0.3", "0.0.4", "0.0.5"):
            (self.repo / "Cargo.toml").write_text(f'[workspace.package]\nversion = "{version}"\n')
            self.git("add", "Cargo.toml")
            self.git("commit", "-qm", version)
            self.git("tag", f"v{version}")
            self.assets(version)
        self.run_installer("--yes", "--server-only", "--version", "0.0.2", TEST_SERVER="1")
        for version in ("0.0.3", "0.0.4", "0.0.5"):
            self.run_installer("--yes", "--app-only", "--version", version)
        kept = {path.name for path in (self.installation / "versions").iterdir()}
        self.assertEqual(kept, {"0.0.1", "0.0.2", "0.0.4", "0.0.5"})
        self.assertEqual(self.pointer("server-previous"), Path("versions/0.0.1"))
        self.assertEqual(self.pointer("app-previous"), Path("versions/0.0.4"))

    def test_installation_lock_is_held_across_tagged_installer(self):
        (self.repo / "install.sh").write_text(
            'if [ "${1:-}" = --help ]; then echo --from-source app-current; exit 0; fi\n'
            'test -d "$ELSE_HOME/run/install.lock" || exit 71\n'
            'echo held > "$ELSE_HOME/lock-observed"\n' + HARNESS)
        self.git("add", "install.sh")
        self.git("commit", "-qm", "observe installer lock")
        self.git("tag", "-f", "v0.0.2")
        self.run_installer("--yes", "--app-only", "--version", "0.0.2")
        self.assertTrue((self.installation / "lock-observed").is_file())
        lock = self.installation / "run/install.lock"
        self.assertFalse(lock.exists())
        lock.mkdir()
        result = self.run_installer("--yes", "--app-only", "--version", "0.0.1", success=False)
        self.assertIn("another installer holds", result.stderr)
        self.assertEqual(self.pointer("app-current"), Path("versions/0.0.2"))

    def test_failed_publication_never_exposes_an_incomplete_version(self):
        self.run_installer(TEST_ACTION="release", ELSE_VERSION="0.0.1")
        result = self.run_installer(success=False, before_action='mv() { return 74; }\n',
                                    TEST_ACTION="release", ELSE_VERSION="0.0.2")
        self.assertEqual(result.returncode, 74)
        self.assertFalse((self.installation / "versions/0.0.2").exists())
        self.assertEqual((self.installation / "current").readlink(), Path("versions/0.0.1"))

    def test_new_version_appears_with_both_binaries_and_profile(self):
        marker = self.root / "partial-version"
        self.run_installer(before_action=r'''
mv() {
    command mv "$@"
    if [ -d "$VERSIONS/0.0.2" ] &&
       { [ ! -f "$VERSIONS/0.0.2/bin/else-desktop" ] || [ ! -f "$VERSIONS/0.0.2/.build-profile" ]; }; then
        touch "$TEST_PARTIAL_VERSION"
    fi
}
''', TEST_ACTION="release", ELSE_VERSION="0.0.2", TEST_PARTIAL_VERSION=str(marker))
        self.assertFalse(marker.exists(), "a version must appear as a complete payload")

    def test_versioned_installer_handoff(self):
        (self.repo / "install.sh").write_text('''
if [ "${1:-}" = --help ]; then echo --from-source app-current; exit 0; fi
printf '%s\\n' "$ELSE_INSTALL_STAGE" > "$TEST_PHASE"
''' + HARNESS)
        self.git("add", "install.sh")
        self.git("commit", "-qm", "installer supporting prebuilt releases")
        self.git("tag", "-f", "v0.0.2")
        phase = self.root / "phase"
        self.run_installer("--yes", "--app-only", "--version", "0.0.2", TEST_PHASE=str(phase))
        self.assertEqual(phase.read_text().strip(), "2")
        self.assertTrue((self.installation / "current/bin/else-desktop").is_file())

    def test_bad_downloads_never_replace_current_installation(self):
        self.run_installer(TEST_ACTION="release", ELSE_VERSION="0.0.1")
        assets = self.root / "releases/download/v0.0.2"
        desktop = assets / release_binaries.archive_name("else-desktop", "0.0.2", "linux-amd64")
        checksums = assets / "SHA256SUMS"
        for failure in ("checksum", "missing-checksum", "duplicate-checksum", "missing-archive", "wrong-version"):
            with self.subTest(failure=failure):
                self.assets("0.0.2", "0.0.1" if failure == "wrong-version" else None)
                if failure == "checksum":
                    desktop.write_bytes(b"corrupt archive")
                elif failure == "missing-checksum":
                    checksums.write_text("".join(line for line in checksums.read_text().splitlines(True) if desktop.name not in line))
                elif failure == "duplicate-checksum":
                    checksums.write_text(checksums.read_text() + next(line for line in checksums.read_text().splitlines(True) if desktop.name in line))
                elif failure == "missing-archive":
                    desktop.unlink()
                self.run_installer(success=False, TEST_ACTION="release", ELSE_VERSION="0.0.2")
                self.assertEqual((self.installation / "current").readlink(), Path("versions/0.0.1"))
                self.assertFalse((self.installation / "versions/0.0.2").exists())
                self.assertEqual(list((self.installation / "versions").glob(".install-*")), [])

    def test_release_without_binaries_does_not_build_locally(self):
        (self.root / "releases/latest/download/SHA256SUMS").unlink()
        result = self.run_installer("--yes", "--app-only", success=False)
        self.assertIn("--from-source", result.stderr)
        self.assertFalse((self.installation / "toolchain").exists())

    def test_unsupported_macos_architecture_is_rejected(self):
        result = self.run_installer("--yes", "--app-only", success=False, TEST_DISTRO="macos")
        self.assertIn("Apple silicon", result.stderr)
        self.assertEqual(self.requests, [])

    def source_toolchain(self):
        toolchain = self.installation / "toolchain/cargo/bin"
        executable(toolchain / "rustc", "#!/bin/sh\necho 'rustc 1.99.1'\n")
        executable(toolchain / "cargo", r'''#!/bin/bash
set -eu
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml)
mkdir -p "$CARGO_TARGET_DIR/release"
printf '#!/bin/sh\necho "else-desktop %s"\n' "$version" > "$CARGO_TARGET_DIR/release/else-desktop"
echo compiled > "$CARGO_TARGET_DIR/compiled"
''')

    def test_source_build_is_explicit(self):
        self.source_toolchain()
        self.run_installer("--yes", "--app-only", "--from-source", "--version", "0.0.2")
        self.assertTrue((self.installation / "build/compiled").is_file())
        self.assertIn("build-essential", (self.root / "packages").read_text().split())
        self.assertEqual(self.requests, [])

    def test_source_install_runs_the_legacy_release_policy(self):
        marker = self.root / "legacy-policy"
        (self.repo / "install.sh").write_text('''
if [ "${1:-}" = --help ]; then echo --server-only; exit 0; fi
for argument in "$@"; do
    if [ "$argument" = --from-source ]; then exit 77; fi
done
test "$ELSE_INSTALL_STAGE" = 2
printf 'legacy database and service policy' > "$TEST_LEGACY_POLICY"
''')
        self.git("add", "install.sh")
        self.git("commit", "-qm", "legacy release policy")
        self.git("tag", "-f", "v0.0.2")
        self.source_toolchain()
        for arguments in (("--yes", "--app-only", "--from-source", "--version", "0.0.2"), ("--from-source",)):
            with self.subTest(arguments=arguments):
                marker.unlink(missing_ok=True)
                self.run_installer(*arguments, ELSE_VERSION="0.0.2", TEST_LEGACY_POLICY=str(marker))
                self.assertTrue(marker.is_file(), "the tagged installer's policy must run")
                self.assertEqual(marker.read_text(), "legacy database and service policy")
        self.assertEqual(self.requests, [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
