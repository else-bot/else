#!/usr/bin/env python3
"""Exercise removal against disposable installations and service managers."""
import json
import os
from pathlib import Path
import pty
import select
import shlex
import shutil
import subprocess
import tempfile
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]


def executable(path, source):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(source)
    path.chmod(0o755)


class Removal(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / 'home'
        self.installation = self.home / 'installation with spaces'
        self.cli = self.installation / 'bin/elsebot'
        self.cli.parent.mkdir(parents=True)
        shutil.copy2(ROOT / 'scripts/elsebot', self.cli)
        self.config = self.installation / 'config'
        self.config.mkdir()
        self.metadata = self.config / 'elsebot.env'
        self.metadata.write_text('ELSEBOT_MANAGER=process\n')
        self.stubs = self.root / 'stubs'
        self.stubs.mkdir()
        self.events = self.root / 'events'
        self.env = dict(os.environ, HOME=str(self.home), ELSE_HOME=str(self.installation),
                        XDG_CONFIG_HOME=str(self.home / '.config'),
                        PATH=f'{self.stubs}:{os.environ["PATH"]}', EVENTS=str(self.events))
        for key in ('ELSE_INSTALL_LOCK_PID', 'ELSE_DATA_DIR', 'DATABASE_URL'):
            self.env.pop(key, None)
        for name in ('data/history', 'bots/project', 'toolchain/bin/rustc', 'versions/old/bin/else-api',
                     'logs/else-api.log', 'run/stale.pid', 'config/desktop.servers.json'):
            path = self.installation / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('else.bot fixture')
        (self.config / '.env').touch()
        self.api = self.installation / 'versions/server/bin/else-api'
        executable(self.api, '\n'.join([
            '#!/bin/sh', '[ "$*" != --help ] || { echo "remove-state --yes"; exit; }',
            '[ "$*" = "remove-state --yes" ] || exit 2', 'echo cleanup >> "$EVENTS"', '']))
        (self.installation / 'server-current').symlink_to('versions/server')
        (self.installation / 'current').symlink_to('versions/app')
        executable(self.installation / 'versions/app/bin/else-api', '#!/bin/sh\nexit 42\n')
        self.link = self.home / '.local/bin/elsebot'
        self.link.parent.mkdir(parents=True)
        self.link.symlink_to(self.cli)
        self.desktop = self.home / '.local/share/applications/else-bot.desktop'
        self.desktop.parent.mkdir(parents=True)
        self.desktop.write_text(f'[Desktop Entry]\nExec={self.cli} desktop\n')
        self.icon = self.home / '.local/share/icons/hicolor/scalable/apps/else-bot.svg'
        self.icon.parent.mkdir(parents=True)
        self.icon.touch()

    def run_cli(self, *args):
        return subprocess.run(['bash', str(self.cli), *args], env=self.env,
                              capture_output=True, text=True, timeout=20)

    def assert_removed(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('else.bot removed.', result.stdout)
        self.assertFalse(self.installation.exists())
        self.assertFalse(self.link.is_symlink())
        self.assertFalse(self.desktop.exists())
        self.assertFalse(self.icon.exists())

    def systemd(self):
        self.metadata.write_text('ELSEBOT_MANAGER=systemd\nELSEBOT_SERVICE=custom-api\n')
        unit = self.home / '.config/systemd/user/custom-api.service'
        unit.parent.mkdir(parents=True)
        unit.write_text(f'[Service]\nWorkingDirectory={self.config}\n')
        executable(self.stubs / 'systemctl', '''#!/bin/sh
printf '%s\n' "$*" >> "$EVENTS"
case "$*" in
*LoadState*) echo loaded ;;
*WorkingDirectory*) printf '%s/config\n' "$ELSE_HOME" ;;
esac
''')
        return unit

    def test_complete_removal_uses_server_pointer_and_preserves_unrelated_files(self):
        unrelated = self.home / '.local/bin/other'
        unrelated.write_text('keep')
        self.assert_removed(self.run_cli('remove', '--yes'))
        self.assertEqual(self.events.read_text(), 'cleanup\n')
        self.assertEqual(unrelated.read_text(), 'keep')
        result = subprocess.run(['bash', str(ROOT / 'scripts/elsebot'), 'remove', '--yes'],
                                env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('already removed', result.stdout)

    def test_app_only_needs_no_server(self):
        (self.config / '.env').unlink()
        shutil.rmtree(self.installation / 'data')
        self.api.unlink()
        self.assert_removed(self.run_cli('remove', '--yes'))
        self.assertFalse(self.events.exists())

    def test_server_only_needs_no_desktop(self):
        self.desktop.unlink()
        self.icon.unlink()
        self.assert_removed(self.run_cli('remove', '--yes'))

    def test_help_bad_options_and_noninteractive_confirmation_do_not_delete(self):
        for args, success in ((['remove', '--help'], True), (['remove', '--force'], False),
                              (['remove', '--yes', 'extra'], False), (['remove'], False)):
            with self.subTest(args=args):
                result = self.run_cli(*args)
                self.assertEqual(result.returncode == 0, success, result.stderr)
                self.assertTrue(self.cli.exists())
                self.assertFalse(self.events.exists())

    def test_terminal_confirmation_and_cancellation(self):
        for answer in (b'no\n', b'remove\n'):
            master, slave = pty.openpty()
            try:
                process = subprocess.Popen(['bash', str(self.cli), 'remove'], env=self.env,
                                           stdin=slave, stdout=slave, stderr=slave)
                os.close(slave)
                output = b''
                deadline = time.monotonic() + 10
                while b'Type remove to continue:' not in output:
                    self.assertLess(time.monotonic(), deadline, output)
                    if select.select([master], [], [], 0.1)[0]:
                        output += os.read(master, 8192)
                os.write(master, answer)
                self.assertEqual(process.wait(timeout=10), 0, output)
            finally:
                os.close(master)
            self.assertEqual(self.installation.exists(), answer == b'no\n')

    def test_systemd_service_is_stopped_disabled_and_unregistered(self):
        unit = self.systemd()
        self.assert_removed(self.run_cli('remove', '--yes'))
        self.assertFalse(unit.exists())
        self.assertEqual(self.events.read_text().splitlines(), [
            '--user show -p LoadState --value custom-api.service',
            '--user show -p WorkingDirectory --value custom-api.service', '--user stop custom-api.service',
            '--user disable custom-api.service', 'cleanup', '--user daemon-reload',
            '--user reset-failed custom-api.service'])

    def test_service_stop_failure_leaves_software_data_and_registration(self):
        unit = self.systemd()
        executable(self.stubs / 'systemctl', '#!/bin/sh\ncase "$*" in *LoadState*) echo loaded ;; *WorkingDirectory*) echo \"$ELSE_HOME/config\" ;; *) exit 1 ;; esac\n')
        result = self.run_cli('remove', '--yes')
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(unit.exists())
        self.assertTrue(self.cli.exists())
        self.assertTrue((self.installation / 'data/history').exists())
        self.assertFalse((self.installation / 'run/install.lock').exists())

    def test_cleanup_failure_keeps_installation_for_retry(self):
        executable(self.api, '#!/bin/sh\nexit 19\n')
        result = self.run_cli('remove', '--yes')
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.cli.exists())
        self.assertTrue(self.link.exists())
        self.assertTrue(self.desktop.exists())
        self.assertFalse((self.installation / 'run/install.lock').exists())
        executable(self.api, '#!/bin/sh\n[ \"$*\" != --help ] || echo \"remove-state --yes\"\nexit 0\n')
        self.assert_removed(self.run_cli('remove', '--yes'))

    def test_older_server_reset_is_supported_and_lock_is_reclaimed(self):
        executable(self.api, '\n'.join([
            '#!/bin/sh', '[ "$*" != --help ] || { echo "reset --yes"; exit; }',
            '[ "$*" = "reset --yes" ] || exit 2', 'rm -rf "$ELSE_HOME/run/install.lock"',
            'echo legacy-cleanup >> "$EVENTS"', '']))
        self.assert_removed(self.run_cli('remove', '--yes'))
        self.assertEqual(self.events.read_text(), 'legacy-cleanup\n')

    def test_older_server_cleanup_preserves_a_new_installers_lock(self):
        executable(self.api, '\n'.join([
            '#!/bin/sh', '[ "$*" != --help ] || { echo "reset --yes"; exit; }',
            'rm -rf "$ELSE_HOME/run/install.lock"', 'mkdir "$ELSE_HOME/run/install.lock"',
            'echo another-installer > "$ELSE_HOME/run/install.lock/pid"', '']))
        result = self.run_cli('remove', '--yes')
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.cli.exists())
        self.assertEqual((self.installation / 'run/install.lock/pid').read_text(), 'another-installer\n')

    def test_stale_pid_does_not_stop_another_installations_server(self):
        binary = self.root / 'other-installation/bin/else-api'
        executable(binary, '#!/usr/bin/env python3\nimport time\ntime.sleep(60)\n')
        process = subprocess.Popen([str(binary), 'serve'])
        self.addCleanup(lambda: process.poll() is None and process.kill())
        (self.installation / 'run/else-api.pid').write_text(str(process.pid))
        self.assert_removed(self.run_cli('remove', '--yes'))
        self.assertIsNone(process.poll())
        process.terminate()
        process.wait(timeout=5)

    def test_link_resolves_installation_without_gnu_readlink(self):
        executable(self.stubs / 'readlink', '#!/bin/sh\n[ "$1" != -f ] || exit 1\nexec /usr/bin/readlink "$@"\n')
        self.env.pop('ELSE_HOME')
        result = subprocess.run([str(self.link), 'paths'], env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f'home:        {self.installation}', result.stdout)

    def test_foreign_launchers_and_links_are_preserved(self):
        self.desktop.write_text('Exec=/another/elsebot desktop\n')
        self.link.unlink()
        self.link.symlink_to('/another/bin/elsebot')
        result = self.run_cli('remove', '--yes')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.desktop.exists())
        self.assertTrue(self.icon.exists())
        self.assertEqual(self.link.readlink(), Path('/another/bin/elsebot'))

    def test_foreign_service_prevents_removal(self):
        unit = self.systemd()
        unit.write_text('[Service]\nWorkingDirectory=/another/config\n')
        result = self.run_cli('remove', '--yes')
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.cli.exists())
        self.assertFalse(self.events.exists())

    def test_app_only_removal_preserves_another_installations_service(self):
        unit = self.systemd()
        unit.write_text('[Service]\nWorkingDirectory=/another/config\n')
        self.metadata.write_text('ELSEBOT_MANAGER=process\nELSEBOT_SERVICE=custom-api\n')
        self.assert_removed(self.run_cli('remove', '--yes'))
        self.assertTrue(unit.exists())
        self.assertEqual(self.events.read_text(), 'cleanup\n')

    def test_installer_lock_prevents_removal(self):
        lock = self.installation / 'run/install.lock'
        lock.mkdir()
        (lock / 'pid').write_text('123')
        result = self.run_cli('remove', '--yes')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((lock / 'pid').read_text(), '123')
        self.assertFalse(self.events.exists())

    def test_home_and_ancestors_are_refused(self):
        for home in (self.home, self.root, Path('/')):
            with self.subTest(home=home):
                self.env['ELSE_HOME'] = str(home)
                result = self.run_cli('remove', '--yes')
                self.assertNotEqual(result.returncode, 0)
                self.assertTrue(self.cli.exists())
                self.assertFalse(self.events.exists())

    def test_symlinked_run_is_refused(self):
        shutil.rmtree(self.installation / 'run')
        target = self.root / 'other-run'
        target.mkdir()
        (self.installation / 'run').symlink_to(target)
        result = self.run_cli('remove', '--yes')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((target / 'install.lock').exists())

    def test_matching_desktop_process_is_stopped(self):
        binary = self.installation / 'versions/app/bin/else-desktop'
        shutil.copy2('/bin/sleep', binary)
        process = subprocess.Popen([str(binary), '60'])
        self.addCleanup(lambda: process.poll() is None and process.kill())
        reaper = threading.Thread(target=process.wait)
        reaper.start()
        result = self.run_cli('remove', '--yes')
        self.assert_removed(result)
        reaper.join(timeout=5)
        self.assertIsNotNone(process.returncode)

    def test_launchd_and_owned_macos_bundle_are_removed(self):
        app = self.home / 'Applications/else.bot.app'
        marker = app / 'Contents/Resources/installation.json'
        marker.parent.mkdir(parents=True)
        marker.write_text(json.dumps({'home': str(self.installation)}))
        self.metadata.write_text(f'ELSEBOT_MANAGER=launchd\nELSEBOT_APP={shlex.quote(str(app))}\n')
        plist = self.home / 'Library/LaunchAgents/bot.else.else-api.plist'
        plist.parent.mkdir(parents=True)
        plist.write_text(f'<string>{self.config}</string>\n')
        state = self.root / 'launchd-state'
        state.touch()
        executable(self.stubs / 'launchctl', f'''#!/bin/sh
printf '%s\n' "$*" >> "$EVENTS"
case "$1:$2" in
print:gui/*/bot.else.*) test -f {shlex.quote(str(state))} || exit 1; echo "working directory = $ELSE_HOME/config" ;;
bootout:*) rm -f {shlex.quote(str(state))} ;;
esac
''')
        self.assert_removed(self.run_cli('remove', '--yes'))
        self.assertFalse(app.exists())
        self.assertFalse(plist.exists())
        self.assertIn('bootout gui/', self.events.read_text())

    def test_macos_update_leftovers_are_removed_by_ownership_even_without_primary_bundle(self):
        app = self.home / 'Applications/else.bot.app'
        app.parent.mkdir()
        self.metadata.write_text(f'ELSEBOT_APP={shlex.quote(str(app))}\n')
        owned = []
        for suffix in ('old', 'new'):
            bundle = app.parent / f'.else.bot.app.{"a" * 32}.{suffix}'
            marker = bundle / 'Contents/Resources/installation.json'
            marker.parent.mkdir(parents=True)
            marker.write_text(json.dumps({'home': str(self.installation)}))
            owned.append(bundle)
        foreign = app.parent / f'.else.bot.app.{"b" * 32}.old'
        marker = foreign / 'Contents/Resources/installation.json'
        marker.parent.mkdir(parents=True)
        marker.write_text(json.dumps({'home': '/another/installation'}))
        link = app.parent / f'.else.bot.app.{"c" * 32}.new'
        link.symlink_to(foreign)
        lock = app.parent / '.else.bot.app.lock'
        lock.touch()
        self.assert_removed(self.run_cli('remove', '--yes'))
        self.assertTrue(all(not bundle.exists() for bundle in owned))
        self.assertTrue(foreign.exists())
        self.assertTrue(link.is_symlink())
        self.assertFalse(lock.exists())

    def test_loaded_foreign_launchd_agent_is_not_stopped(self):
        self.metadata.write_text('ELSEBOT_MANAGER=launchd\n')
        executable(self.stubs / 'launchctl', '\n'.join([
            '#!/bin/sh', 'echo "$*" >> "$EVENTS"',
            'case "$1:$2" in print:gui/*/bot.else.*) echo "working directory = /another/config" ;; esac', '']))
        result = self.run_cli('remove', '--yes')
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.cli.exists())
        self.assertNotIn('bootout', self.events.read_text())

    def test_foreign_macos_bundle_is_preserved(self):
        app = self.home / 'Applications/else.bot.app'
        marker = app / 'Contents/Resources/installation.json'
        marker.parent.mkdir(parents=True)
        marker.write_text(json.dumps({'home': '/another/installation'}))
        self.metadata.write_text(f'ELSEBOT_APP={shlex.quote(str(app))}\n')
        result = self.run_cli('remove', '--yes')
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(app.exists())
        self.assertTrue(self.cli.exists())
        self.assertFalse(self.events.exists())


if __name__ == '__main__':
    unittest.main()
