#!/usr/bin/env python3
"""Offline checks for installer components, database configuration, and release switching."""
import json
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import plistlib
import pty
import select
import shutil
import subprocess
import sys
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

    def update(self, url, seed_token=True, **overrides):
        if url is not None:
            self.config.write_text(f"DATABASE_URL={url}\nELSE_API_BIND=127.0.0.1:7878\nCUSTOM_SETTING=kept\n")
        if seed_token:
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
COMPONENTS_FLAG=server
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
api_running() { [ "${API_RUNNING:-0}" = 1 ]; }
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

    def test_reinstall_keeps_the_running_api_and_does_not_acquire_its_data_lock(self):
        self.assertEqual(self.update("legacy://localhost/else").returncode, 0)
        (self.home / "events").unlink()
        result = self.update(None, API_RUNNING="1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse((self.home / "events").exists(), "reinstall must not run setup, stop or start")

    def test_fresh_install_still_runs_setup_when_another_listener_answers(self):
        result = self.update(None, seed_token=False, API_RUNNING="1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("setup versions/0.0.6 sqlite:", (self.home / "events").read_text())


# The installer's real flag parsing, stage 2, build selection, and commands,
# with a fake cargo and recorders for the steps that need the system.
COMPONENT_HARNESS = DEFINITIONS + r'''
detect_platform() { DISTRO=arch; ARCH=x86_64; }
source_version() { echo "$ELSE_VERSION"; }
ensure_rust() { :; }
linux_packages() {
    case " $(pacman_packages) " in
    *" wayland "*) echo "packages app" >>"$ELSE_HOME/events" ;;
    *) echo "packages server" >>"$ELSE_HOME/events" ;;
    esac
}
for step in linux_access ensure_msb decide_guest_image write_config ensure_bots_dir \
    refresh_provider_paths build_guest_image setup_service run_setup start_api \
    check_providers api_running prepare_running_api backup_if_migrating; do
    eval "$step() { echo $step >>\"\$ELSE_HOME/events\"; }"
done
api_running() { echo api_running >>"$ELSE_HOME/events"; return 1; }
main "$@"
'''

FAKE_CARGO = r'''#!/bin/sh
echo "$@" >>"$ELSE_HOME/cargo.log"
[ "${FAIL_BUILD:-0}" = 1 ] && exit 1
mkdir -p "$CARGO_TARGET_DIR/release"
while [ $# -gt 0 ]; do
    if [ "$1" = -p ]; then
        shift
        printf '#!/bin/sh\necho "%s %s"\n' "$1" "$ELSE_VERSION" >"$CARGO_TARGET_DIR/release/$1"
        chmod +x "$CARGO_TARGET_DIR/release/$1"
    fi
    shift
done
'''

SERVER_STEPS = {"linux_access", "ensure_msb", "decide_guest_image", "write_config",
                "ensure_bots_dir", "setup_service", "run_setup", "api_running", "check_providers"}


class InstallerComponentTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.home = Path(directory.name)
        self.else_home = self.home / ".else"
        (self.else_home / "src/scripts").mkdir(parents=True)
        shutil.copy(ROOT / "scripts/elsebot", self.else_home / "src/scripts/elsebot")
        cargo = self.else_home / "toolchain/cargo/bin/cargo"
        cargo.parent.mkdir(parents=True)
        cargo.write_text(FAKE_CARGO)
        cargo.chmod(0o755)

    def env(self, version, **overrides):
        return {
            "PATH": os.environ["PATH"], "HOME": str(self.home), "USER": "tester",
            "ELSE_HOME": str(self.else_home), "ELSE_VERSION": version,
            "ELSE_INSTALL_STAGE": "2", "ELSE_INSTALL_NO_SYSTEMD": "1", **overrides,
        }

    def install(self, *args, version="0.0.6", **overrides):
        self.reset_logs()
        # A new session: no controlling terminal, as under `curl … | bash` in CI.
        return subprocess.run(["bash", "-c", COMPONENT_HARNESS, "install.sh", *args],
                              env=self.env(version, **overrides), capture_output=True,
                              text=True, stdin=subprocess.DEVNULL, start_new_session=True)

    def install_on_terminal(self, answers, *args, version="0.0.6"):
        """Run with a controlling terminal, typing each answer at a prompt."""
        self.reset_logs()
        pid, fd = pty.fork()
        if pid == 0:
            os.execve("/bin/bash", ["bash", "-c", COMPONENT_HARNESS, "install.sh", *args],
                      self.env(version))
        output, pending = b"", list(answers)
        while True:
            ready, _, _ = select.select([fd], [], [], 10)
            if not ready:
                os.kill(pid, 9)
                os.waitpid(pid, 0)
                os.close(fd)
                self.fail(f"installer terminal stalled: {output.decode(errors='replace')}")
            try:
                chunk = os.read(fd, 4096)
            except OSError:
                break
            if not chunk:
                break
            output += chunk
            if output.count(b"Choose 1-3") > len(answers):
                os.kill(pid, 9)
                os.waitpid(pid, 0)
                os.close(fd)
                self.fail(f"installer asked an unexpected question: {output.decode(errors='replace')}")
            if pending and output.count(b"Choose 1-3") > len(answers) - len(pending):
                os.write(fd, pending.pop(0).encode() + b"\n")
        _, status = os.waitpid(pid, 0)
        os.close(fd)
        return os.waitstatus_to_exitcode(status), output.decode(errors="replace")

    def reset_logs(self):
        for name in ("events", "cargo.log"):
            (self.else_home / name).unlink(missing_ok=True)

    def lines(self, name):
        path = self.else_home / name
        return path.read_text().splitlines() if path.exists() else []

    def components(self):
        return (self.else_home / "config/components").read_text().strip()

    def binary(self, version, name):
        return self.else_home / "versions" / version / "bin" / name

    def assert_ok(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_yes_on_a_fresh_machine_installs_both(self):
        self.assert_ok(self.install("--yes"))
        self.assertEqual(self.components(), "both")
        self.assertEqual(self.lines("cargo.log"), [
            "build --profile release --locked -p else-api -p else-desktop"])
        self.assertTrue(SERVER_STEPS <= set(self.lines("events")))
        self.assertIn("packages app", self.lines("events"))

    def test_enter_on_a_terminal_installs_both(self):
        code, output = self.install_on_terminal([""])
        self.assertEqual(code, 0, output)
        self.assertIn("Choose 1-3 [both]", output)
        self.assertEqual(self.components(), "both")

    def test_app_only_skips_the_server(self):
        code, output = self.install_on_terminal(["9", "3"])
        self.assertEqual(code, 0, output)
        self.assertIn("answer 1, 2, or 3", output)
        self.assertEqual(self.components(), "app")
        self.assertEqual(self.lines("events"), ["packages app"])
        self.assertEqual(self.lines("cargo.log"), ["build --profile release --locked -p else-desktop"])
        for absent in ("config/.env", "data", "bots", "bin/else-api", "versions/0.0.6/bin/else-api"):
            self.assertFalse((self.else_home / absent).exists(), absent)
        self.assertEqual(os.readlink(self.else_home / "current"), "versions/0.0.6")
        self.assertTrue((self.else_home / "bin/else-desktop").exists())
        self.assertTrue((self.home / ".local/share/applications/else-bot.desktop").exists())
        self.assertIn("ELSEBOT_AUTOSTART=\n", (self.else_home / "config/elsebot.env").read_text())
        self.assertEqual((self.else_home / "config").stat().st_mode & 0o777, 0o700)
        status = subprocess.run([self.else_home / "bin/elsebot", "status"], capture_output=True,
                                text=True, env=self.env("0.0.6"))
        self.assertIn("installed: 0.0.6", status.stdout)
        self.assertIn("not on this machine", status.stdout)

    def test_server_only_builds_no_app(self):
        self.assert_ok(self.install("--server-only", "--yes"))
        self.assertEqual(self.components(), "server")
        self.assertEqual(self.lines("cargo.log"), ["build --profile release --locked -p else-api"])
        self.assertIn("packages server", self.lines("events"))
        self.assertFalse((self.else_home / "bin/else-desktop").exists())

    def test_app_only_updates_stay_app_only(self):
        self.assert_ok(self.install("--app-only", "--yes", version="0.0.5"))
        result = self.install("--yes", version="0.0.6")
        self.assert_ok(result)
        self.assertIn("Updating else.bot 0.0.5 -> 0.0.6 (app)", result.stdout)
        self.assertEqual(self.components(), "app")
        self.assertEqual(self.lines("events"), ["packages app"])
        self.assertEqual(os.readlink(self.else_home / "current"), "versions/0.0.6")
        self.assertTrue(self.binary("0.0.5", "else-desktop").exists(), "previous release kept")
        code, output = self.install_on_terminal([])
        self.assertEqual(code, 0, output)
        self.assertIn("already built", output)

    def test_terminal_updates_keep_the_installed_components_without_asking(self):
        for flag, components, packages in (
            ("--both", "both", "-p else-api -p else-desktop"),
            ("--server-only", "server", "-p else-api"),
            ("--app-only", "app", "-p else-desktop"),
        ):
            for legacy in (False, True):
                with self.subTest(components=components, legacy=legacy):
                    shutil.rmtree(self.else_home / "versions", ignore_errors=True)
                    (self.else_home / "current").unlink(missing_ok=True)
                    shutil.rmtree(self.else_home / "config", ignore_errors=True)
                    self.assert_ok(self.install(flag, "--yes", version="0.0.5"))
                    if components != "app":
                        (self.else_home / "config/.env").write_text("ELSE_API_BIND=127.0.0.1:7878\n")
                    if legacy:
                        (self.else_home / "config/components").unlink()
                    code, output = self.install_on_terminal([], version="0.0.6")
                    self.assertEqual(code, 0, output)
                    self.assertIn(f"Updating else.bot 0.0.5 -> 0.0.6 ({components})", output)
                    self.assertEqual(self.lines("cargo.log"), [f"build --profile release --locked {packages}"])
                    self.assertEqual(self.components(), components)
                    code, output = self.install_on_terminal([], version="0.0.6")
                    self.assertEqual(code, 0, output)
                    self.assertIn("already built", output)
                    self.assertEqual(self.lines("cargo.log"), [])

    def test_adding_the_server_to_the_app_keeps_the_app(self):
        self.assert_ok(self.install("--app-only", "--yes"))
        desktop = self.binary("0.0.6", "else-desktop").stat().st_ino
        result = self.install("--server-only", "--yes")
        self.assert_ok(result)
        self.assertIn("Keeping the installed app: installing both", result.stdout)
        self.assertEqual(self.components(), "both")
        self.assertEqual(self.lines("cargo.log"), ["build --profile release --locked -p else-api"])
        self.assertEqual(self.binary("0.0.6", "else-desktop").stat().st_ino, desktop)
        self.assertTrue(SERVER_STEPS <= set(self.lines("events")))

    def test_adding_the_app_to_a_server_keeps_the_server(self):
        (self.else_home / "config").mkdir()
        (self.else_home / "config/.env").write_text("ELSE_API_BIND=127.0.0.1:7878\n")
        self.binary("0.0.6", "else-api").parent.mkdir(parents=True)
        self.binary("0.0.6", "else-api").write_text("#!/bin/sh\necho 'else-api 0.0.6'\n")
        self.binary("0.0.6", "else-api").chmod(0o755)
        (self.else_home / "current").symlink_to("versions/0.0.6")
        result = self.install("--app-only", "--yes")
        self.assert_ok(result)
        self.assertIn("Keeping the installed server: installing both", result.stdout)
        self.assertEqual(self.lines("cargo.log"), ["build --profile release --locked -p else-desktop"])
        self.assertIn("else-api 0.0.6", self.binary("0.0.6", "else-api").read_text())
        self.assertTrue({"run_setup", "check_providers"} <= set(self.lines("events")))
        self.assertEqual(self.components(), "both")

    def test_conflicting_flags_are_refused(self):
        result = self.install("--server-only", "--app-only")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("choose one of --both, --server-only, and --app-only", result.stderr)
        self.assertFalse((self.else_home / "current").exists())

    def test_interrupted_server_addition_is_kept_on_the_next_update(self):
        self.assert_ok(self.install("--app-only", "--yes", version="0.0.5"))
        server = self.binary("0.0.5", "else-api")
        server.write_text("#!/bin/sh\necho 'else-api 0.0.5'\n")
        server.chmod(0o755)
        (self.else_home / "config/.env").write_text("ELSE_API_BIND=127.0.0.1:7878\n")
        self.assert_ok(self.install("--yes", version="0.0.6"))
        self.assertEqual(self.components(), "both")
        self.assertTrue(self.binary("0.0.6", "else-api").exists())
        self.assertTrue(self.binary("0.0.6", "else-desktop").exists())
        self.assertTrue(SERVER_STEPS <= set(self.lines("events")))

    def test_invalid_saved_components_stop_before_the_install(self):
        self.assert_ok(self.install("--app-only", "--yes", version="0.0.5"))
        (self.else_home / "config/components").write_text("invalid\n")
        result = self.install("--yes", version="0.0.6")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid saved install components", result.stderr)
        self.assertEqual(os.readlink(self.else_home / "current"), "versions/0.0.5")
        self.assertEqual(self.lines("cargo.log"), [])

    def test_a_failed_install_does_not_remember_the_choice(self):
        result = self.install("--app-only", "--yes", FAIL_BUILD="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.else_home / "config/components").exists())
        self.assert_ok(self.install("--yes"))
        self.assertEqual(self.components(), "both")


APP_HARNESS = DEFINITIONS + r'''
DISTRO=macos
COMPONENTS="${TEST_COMPONENTS:-both}"
for version in "$@"; do
    switch_to "$version"
    install_commands
done
'''


def stub(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(0o755)


class MacAppTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.home = Path(directory.name)
        self.else_home = self.home / "custom"
        for script in ("elsebot", "macos_app.py"):
            stub(self.else_home / "src/scripts" / script, (ROOT / "scripts" / script).read_text())
        for version in ("0.0.5", "0.0.6"):
            stub(self.else_home / f"versions/{version}/bin/else-desktop",
                 f'#!/bin/sh\necho "else-desktop {version}"\n')
        self.app = self.else_home / "Applications/else.bot.app"

    def install(self, *versions, else_home=None, **overrides):
        env = {"PATH": os.environ["PATH"], "HOME": str(self.home), "USER": "tester",
               "ELSE_HOME": str(else_home or self.else_home), "ELSE_INSTALL_NO_SYSTEMD": "1",
               "ELSE_AUTOSTART": "0", **overrides}
        result = subprocess.run(["bash", "-c", APP_HARNESS, "install.sh", *versions], env=env,
                                capture_output=True, text=True, stdin=subprocess.DEVNULL)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result

    def executable(self, app=None):
        return (app or self.app) / "Contents/MacOS/else-desktop"

    def leftovers(self):
        return sorted(p.name for p in self.app.parent.iterdir()
                      if p.name not in {self.app.name, f".{self.app.name}.lock"})

    def test_the_app_is_a_complete_copy_with_only_the_installation_root(self):
        self.install("0.0.5")
        parts = self.app / "Contents"
        info = plistlib.loads((parts / "Info.plist").read_bytes())
        self.assertEqual(info["CFBundleIdentifier"], "bot.else.desktop")
        self.assertEqual(info["CFBundleExecutable"], "else-desktop")
        self.assertEqual(info["CFBundleShortVersionString"], "0.0.5")
        self.assertNotIn("LSEnvironment", info)
        self.assertEqual(json.loads((parts / "Resources/installation.json").read_text()),
                         {"home": str(self.else_home.resolve())})
        self.assertFalse(self.executable().is_symlink())
        self.assertIn("0.0.5", self.executable().read_text())
        self.assertIn(f"ELSEBOT_APP={self.app}\n", (self.else_home / "config/elsebot.env").read_text())
        inode = self.executable().stat().st_ino
        self.install("0.0.5")
        self.assertEqual(self.executable().stat().st_ino, inode, "an unchanged app is kept")
        self.assertEqual(self.leftovers(), [])

    def test_update_and_rollback_republish_and_keep_a_running_copy(self):
        self.install("0.0.5")
        running = self.executable().open()
        self.addCleanup(running.close)
        self.install("0.0.6")
        self.assertIn("0.0.6", self.executable().read_text())
        info = plistlib.loads((self.app / "Contents/Info.plist").read_bytes())
        self.assertEqual(info["CFBundleVersion"], "0.0.6")
        self.assertIn("0.0.5", running.read(), "the running app's file is unchanged")
        self.install("0.0.6", "0.0.5")
        self.assertIn("0.0.5", self.executable().read_text())
        self.assertEqual(self.leftovers(), [])

    def test_an_interrupted_publication_is_recovered(self):
        self.install("0.0.5")
        old = self.app.parent / f".else.bot.app.{'0' * 32}.old"
        self.app.rename(old)
        staged = self.app.parent / f".else.bot.app.{'0' * 32}.new"
        stub(staged / "Contents/MacOS/else-desktop", "partial")
        (staged / "Contents/Resources").mkdir()
        (staged / "Contents/Resources/installation.json").write_text(json.dumps({"home": str(self.else_home)}))
        self.install("0.0.6")
        self.assertIn("0.0.6", self.executable().read_text())
        self.assertEqual(self.leftovers(), [])

    def test_an_app_of_another_installation_is_left_alone(self):
        stub(self.executable(), "someone else's app")
        result = subprocess.run([sys.executable, ROOT / "scripts/macos_app.py", "--binary",
                                 self.else_home / "versions/0.0.5/bin/else-desktop", "--app", self.app,
                                 "--home", self.else_home, "--version", "0.0.5"],
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertEqual(self.executable().read_text(), "someone else's app")

    def test_recovery_preserves_unrelated_and_foreign_backups(self):
        self.app.parent.mkdir(parents=True)
        unrelated = self.app.parent / ".else.bot.app.backup"
        stub(unrelated / "keep", "personal backup")
        foreign = self.app.parent / f".else.bot.app.{'f' * 32}.old"
        stub(foreign / "Contents/Resources/installation.json", '{"home":"/another-installation"}')
        self.install("0.0.5")
        self.assertEqual((unrelated / "keep").read_text(), "personal backup")
        self.assertTrue(foreign.exists())
        self.assertIn("0.0.5", self.executable().read_text())

    def test_an_occupied_public_app_uses_this_installations_private_app(self):
        default = self.home / ".else"
        shutil.copytree(self.else_home, default, symlinks=True)
        public = self.home / "Applications/else.bot.app"
        stub(public / "Contents/MacOS/else-desktop", "someone else's app")
        self.install("0.0.5", else_home=default)
        private = default / "Applications/else.bot.app"
        self.assertIn("0.0.5", (private / "Contents/MacOS/else-desktop").read_text())
        self.assertIn(f"ELSEBOT_APP={private}\n", (default / "config/elsebot.env").read_text())
        self.assertEqual((public / "Contents/MacOS/else-desktop").read_text(), "someone else's app")

    def test_concurrent_publishers_leave_one_complete_app(self):
        def publish(index):
            version = ("0.0.5", "0.0.6")[index % 2]
            return subprocess.run([sys.executable, ROOT / "scripts/macos_app.py", "--binary",
                                   self.else_home / f"versions/{version}/bin/else-desktop", "--app", self.app,
                                   "--home", self.else_home, "--version", version],
                                  capture_output=True, text=True)
        with ThreadPoolExecutor(4) as workers:
            results = list(workers.map(publish, range(16)))
        for result in results:
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        version = plistlib.loads((self.app / "Contents/Info.plist").read_bytes())["CFBundleVersion"]
        self.assertIn(version, self.executable().read_text())
        self.assertEqual(self.leftovers(), [])

    def test_the_default_installation_uses_the_users_applications(self):
        default = self.home / ".else"
        shutil.copytree(self.else_home, default, symlinks=True)
        self.install("0.0.5", else_home=default)
        app = self.home / "Applications/else.bot.app"
        self.assertEqual(json.loads((app / "Contents/Resources/installation.json").read_text()),
                         {"home": str(default.resolve())})

    def test_an_update_through_a_home_alias_refreshes_the_same_public_app(self):
        default = self.home / ".else"
        shutil.copytree(self.else_home, default, symlinks=True)
        self.install("0.0.5", else_home=default)
        alias = self.home / "home-alias"
        alias.symlink_to(self.home, target_is_directory=True)
        self.install("0.0.6", else_home=default, HOME=str(alias))
        app = self.home / "Applications/else.bot.app"
        self.assertIn("0.0.6", (app / "Contents/MacOS/else-desktop").read_text())
        self.assertFalse((default / "Applications/else.bot.app").exists())

    def test_the_app_is_not_published_without_the_app_component(self):
        self.install("0.0.5", TEST_COMPONENTS="server")
        self.assertFalse(self.app.exists())
        self.assertNotIn("ELSEBOT_APP", (self.else_home / "config/elsebot.env").read_text())


class DesktopCommandTests(unittest.TestCase):
    """`elsebot desktop` with stub uname, open, and desktop binaries."""

    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.home = self.root / "else"
        self.app = self.root / "Applications/else.bot.app"
        stub(self.home / "bin/elsebot", (ROOT / "scripts/elsebot").read_text())
        stub(self.home / "versions/0.0.6/bin/else-desktop", '#!/bin/sh\n'
             'echo "desktop pwd=$(pwd) home=$ELSE_HOME url=${ELSE_API_URL:-none} '
             'data=${ELSE_DATA_DIR:-none} args=$*" >"$RECORD"\n')
        (self.home / "current").symlink_to("versions/0.0.6")
        (self.home / "config").mkdir()
        (self.home / "config/elsebot.env").write_text(f"ELSEBOT_MANAGER=process\nELSEBOT_APP={self.app}\n")
        self.stubs = self.root / "stubs"
        stub(self.stubs / "open", '#!/bin/sh\necho "open $*" >"$RECORD"\n')
        self.app.mkdir(parents=True)

    def desktop(self, system, *args):
        stub(self.stubs / "uname", f"#!/bin/sh\necho {system}\n")
        record = self.root / "record"
        record.unlink(missing_ok=True)
        env = {"PATH": f"{self.stubs}:{os.environ['PATH']}", "HOME": str(self.root), "RECORD": str(record),
               "ELSE_API_URL": "http://127.0.0.1:1", "ELSE_DATA_DIR": "/checkout/data"}
        result = subprocess.run([self.home / "bin/elsebot", "desktop", *args], env=env,
                                capture_output=True, text=True, cwd=self.root)
        return result, record.read_text().strip() if record.exists() else None

    def test_macos_opens_the_installed_app_and_forwards_arguments(self):
        result, record = self.desktop("Darwin", "--flag", "two words")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(record, f"open -a {self.app} --args --flag two words")
        _, record = self.desktop("Darwin")
        self.assertEqual(record, f"open -a {self.app}")

    def test_version_queries_run_the_binary_directly(self):
        _, record = self.desktop("Darwin", "--version")
        self.assertTrue(record.startswith("desktop "), record)
        self.assertTrue(record.endswith("args=--version"), record)

    def test_macos_without_the_app_says_how_to_publish_it(self):
        self.app.rmdir()
        result, record = self.desktop("Darwin")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("elsebot update", result.stderr)
        self.assertIsNone(record)

    def test_linux_runs_the_binary_with_only_this_installation(self):
        result, record = self.desktop("Linux", "--flag")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(record, f"desktop pwd=/ home={self.home.resolve()} url=none data=none args=--flag")


CONFIG_HARNESS = DEFINITIONS + r'''
API_PORT=7878
step() { :; }
ok() { :; }
write_config
config_set ELSE_CODEX_BIN "$CODEX"
config_set ELSE_CODEX_BIN "$CODEX"
for key in DATABASE_URL ELSE_DATA_DIR ELSE_MSB_BIN ELSE_CODEX_BIN; do
    printf '%s\0' "$(env_value "$key")"
done
'''
TRICKY = "custom install it's \"x\" $HOME \\ #1"


class DotenvConfigTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)

    def write_config(self, home, codex="/usr/bin/codex"):
        env = {"PATH": os.environ["PATH"], "HOME": str(self.root), "USER": "tester",
               "ELSE_HOME": str(home), "CODEX": codex}
        result = subprocess.run(["bash", "-c", CONFIG_HARNESS], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.split("\0")[:-1]

    def test_special_characters_read_back_verbatim(self):
        home = self.root / TRICKY
        codex = "/opt/a b/co'dex\\"
        self.assertEqual(self.write_config(home, codex), [
            f"sqlite://{home}/data/else.sqlite3", f"{home}/data",
            f"{home}/microsandbox/bin/msb", codex,
        ])
        config = (home / "config/.env").read_text()
        self.assertIn("\nELSE_API_BIND=127.0.0.1:7878\n", config)
        self.assertEqual(config.count("ELSE_CODEX_BIN="), 1)

    def test_simple_values_stay_unquoted(self):
        home = self.root / "plain"
        self.write_config(home)
        config = (home / "config/.env").read_text()
        self.assertIn(f"\nDATABASE_URL=sqlite://{home}/data/else.sqlite3\n", config)
        self.assertIn("\nELSE_CODEX_BIN=/usr/bin/codex\n", config)

    def test_multiline_setting_is_rejected_without_echoing_it(self):
        home = self.root / "multiline"
        env = {"PATH": os.environ["PATH"], "HOME": str(self.root), "USER": "tester",
               "ELSE_HOME": str(home), "CODEX": "/private-value\nsecond-line"}
        result = subprocess.run(["bash", "-c", CONFIG_HARNESS], env=env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("a setting cannot span lines", result.stderr)
        self.assertNotIn("private-value", result.stdout + result.stderr)
        self.assertNotIn("ELSE_CODEX_BIN=", (home / "config/.env").read_text())

    def test_elsebot_db_opens_the_configured_database(self):
        home = self.root / TRICKY
        self.write_config(home)
        stub(home / "bin/elsebot", (ROOT / "scripts/elsebot").read_text())
        stubs, record = self.root / "stubs", self.root / "record"
        stub(stubs / "sqlite3", '#!/bin/sh\nfor a in "$@"; do printf "%s\\0" "$a"; done >"$RECORD"\n')
        env = {"PATH": f"{stubs}:{os.environ['PATH']}", "HOME": str(self.root), "RECORD": str(record)}
        result = subprocess.run([home / "bin/elsebot", "db", "select 1"], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(record.read_text().split("\0")[:-1],
                         ["-readonly", f"{home}/data/else.sqlite3", "select 1"])

    @unittest.skipUnless(os.environ.get("ELSE_API_BIN"), "set ELSE_API_BIN to a built else-api")
    def test_else_api_setup_reads_the_written_configuration(self):
        home = self.root / "custom install it's"
        self.write_config(home)
        result = subprocess.run([os.environ["ELSE_API_BIN"], "setup"], cwd=home / "config",
                                env={"PATH": os.environ["PATH"], "HOME": str(self.root)},
                                capture_output=True, text=True, timeout=120)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((home / "data/else.sqlite3").is_file(), result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
