#!/usr/bin/env python3
"""Publish an installation's macOS app: a complete copied bundle, replaced whole.

    macos_app.py --binary current/bin/else-desktop --app ~/Applications/else.bot.app \
        --home ~/.else --version 0.0.12

The bundle holds the desktop executable, its Info.plist, and
Contents/Resources/installation.json, which names only the installation root
(no settings, paths to secrets, or environment). An app already there is
replaced only when this installation published it. A running app keeps the
executable it mapped: the old bundle is renamed away, never overwritten.
Exit status 3 means the app belongs to something else and was left alone.
"""
import argparse
import fcntl
import filecmp
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import sys
import uuid

IDENTIFIER = "bot.else.desktop"
NAME = "else.bot"
EXECUTABLE = "else-desktop"
FOREIGN = 3


def contents(app):
    return app / "Contents"


def info(version):
    return plistlib.dumps({
        "CFBundleIdentifier": IDENTIFIER, "CFBundleName": NAME, "CFBundleDisplayName": NAME,
        "CFBundleExecutable": EXECUTABLE, "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": version, "CFBundleVersion": version,
        "NSHighResolutionCapable": True})


def marker(home):
    return json.dumps({"home": str(home)}).encode() + b"\n"


def owner(app):
    try:
        home = Path(json.loads((contents(app) / "Resources/installation.json").read_text())["home"])
        return home.resolve() if home.is_absolute() else None
    except (OSError, ValueError, KeyError, TypeError):
        return None


def current(app, binary, version, home):
    parts = contents(app)
    try:
        return (filecmp.cmp(binary, parts / "MacOS" / EXECUTABLE, shallow=False)
                and (parts / "Info.plist").read_bytes() == info(version)
                and (parts / "Resources/installation.json").read_bytes() == marker(home))
    except OSError:
        return False


def recover(app, home):
    for leftover in sorted(app.parent.glob(f".{app.name}.*")):
        if (not re.fullmatch(rf"\.{re.escape(app.name)}\.[0-9a-f]{{32}}\.(new|old)", leftover.name)
                or leftover.is_symlink() or owner(leftover) != home):
            continue
        if leftover.name.endswith(".old") and not app.exists() and not app.is_symlink():
            leftover.rename(app)
        else:
            shutil.rmtree(leftover, ignore_errors=True)


def publish(binary, app, home, version):
    binary, home = Path(binary).resolve(strict=True), Path(home)
    if not home.is_absolute():
        raise SystemExit("macos_app.py: --home must be an absolute path")
    home = home.resolve(strict=True)
    app.parent.mkdir(parents=True, exist_ok=True)
    app = app.parent.resolve() / app.name
    lock_path = app.parent / f".{app.name}.lock"
    with os.fdopen(os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600), "a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        return publish_locked(binary, app, home, version)


def publish_locked(binary, app, home, version):
    recover(app, home)
    if app.exists() or app.is_symlink():
        if app.is_symlink() or owner(app) != home:
            print(f"{app} was not installed for {home}; leaving it alone", file=sys.stderr)
            return FOREIGN
        if current(app, binary, version, home):
            return 0
    tag = uuid.uuid4().hex
    staged, old = app.parent / f".{app.name}.{tag}.new", app.parent / f".{app.name}.{tag}.old"
    try:
        parts = contents(staged)
        (parts / "Resources").mkdir(parents=True)
        (parts / "Resources/installation.json").write_bytes(marker(home))
        (parts / "MacOS").mkdir()
        shutil.copy2(binary, parts / "MacOS" / EXECUTABLE)
        (parts / "Info.plist").write_bytes(info(version))
        if app.exists():
            app.rename(old)
        try:
            staged.rename(app)
        except OSError:
            if old.exists():
                old.rename(app)
            raise
    finally:
        shutil.rmtree(staged, ignore_errors=True)
    shutil.rmtree(old, ignore_errors=True)
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--binary", required=True)
    parser.add_argument("--app", required=True, type=Path)
    parser.add_argument("--home", required=True)
    parser.add_argument("--version", required=True)
    args = parser.parse_args(argv)
    return publish(args.binary, args.app, args.home, args.version)


if __name__ == "__main__":
    sys.exit(main())
