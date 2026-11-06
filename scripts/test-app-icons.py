#!/usr/bin/env python3
"""Check application icon resources in installed and development macOS bundles."""
from pathlib import Path
import plistlib
import tempfile
import unittest

import dev
import macos_app


class AppIcons(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.binary = self.root / "else-desktop"
        self.binary.write_bytes(b"fixture executable")
        self.binary.chmod(0o755)
        self.app = self.root / "Applications/else.bot.app"

    def assert_icon(self, app):
        contents = app / "Contents"
        info = plistlib.loads((contents / "Info.plist").read_bytes())
        self.assertEqual(info["CFBundleIconFile"], "else-bot.icns")
        self.assertEqual((contents / "Resources/else-bot.icns").read_bytes(), macos_app.ICON.read_bytes())

    def test_installed_bundle_repairs_missing_or_stale_icon(self):
        for damaged in (None, b"stale icon"):
            with self.subTest(damaged=damaged):
                self.assertEqual(macos_app.publish(self.binary, self.app, self.root, "0.0.1"), 0)
                icon = self.app / "Contents/Resources/else-bot.icns"
                if damaged is None:
                    icon.unlink()
                else:
                    icon.write_bytes(damaged)
                self.assertEqual(macos_app.publish(self.binary, self.app, self.root, "0.0.1"), 0)
                self.assert_icon(self.app)
                self.assertEqual((self.app / "Contents/MacOS/else-desktop").read_bytes(), self.binary.read_bytes())

    def test_current_installed_bundle_is_not_replaced(self):
        macos_app.publish(self.binary, self.app, self.root, "0.0.1")
        before = self.app.stat().st_ino
        macos_app.publish(self.binary, self.app, self.root, "0.0.1")
        self.assertEqual(self.app.stat().st_ino, before)
        self.assert_icon(self.app)

    def test_development_bundle_has_icon(self):
        executable = dev.publish_app(self.binary, self.root / "dev", "bot.else.dev.fixture", "else.bot dev")
        self.assert_icon(executable.parents[2])
        self.assertEqual(executable.read_bytes(), self.binary.read_bytes())
        self.assertEqual(dev.publish_app(self.binary, self.root / "dev", "bot.else.dev.fixture", "else.bot dev"),
                         executable)


if __name__ == "__main__":
    unittest.main(verbosity=2)
