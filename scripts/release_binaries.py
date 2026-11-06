#!/usr/bin/env python3
"""Package native release builds and collect the complete release asset set."""
import argparse
import hashlib
import io
from pathlib import Path
import tarfile
import tempfile
import tomllib

import macos_app

PLATFORMS = ("linux-amd64", "linux-arm64", "macos-arm64")
BINARIES = ("else-api", "else-desktop")


def archive_name(binary, version, platform):
    return f"{binary}-v{version}-{platform}.tar.gz"


def package(version, platform, binaries, output):
    output.mkdir(parents=True, exist_ok=True)
    for binary in BINARIES:
        source = binaries / binary
        if not source.is_file() or not source.stat().st_mode & 0o111:
            raise ValueError(f"missing executable: {source}")
    for binary in BINARIES:
        archive = output / archive_name(binary, version, platform)
        with tempfile.TemporaryDirectory(dir=output) as directory:
            staged = Path(directory) / archive.name
            with tarfile.open(staged, "w:gz", dereference=True) as tar:
                member = binary
                if binary == "else-desktop" and platform == "macos-arm64":
                    for name in ("else.bot.app", "else.bot.app/Contents", "else.bot.app/Contents/MacOS",
                                 "else.bot.app/Contents/Resources"):
                        info = tarfile.TarInfo(name)
                        info.type, info.mode = tarfile.DIRTYPE, 0o755
                        tar.addfile(info)
                    plist = macos_app.info(version)
                    info = tarfile.TarInfo("else.bot.app/Contents/Info.plist")
                    info.size, info.mode = len(plist), 0o644
                    tar.addfile(info, io.BytesIO(plist))
                    tar.add(macos_app.ICON, arcname=f"else.bot.app/Contents/Resources/{macos_app.ICON.name}")
                    member = "else.bot.app/Contents/MacOS/else-desktop"
                info = tar.gettarinfo(binaries / binary, arcname=member)
                info.mode = 0o755
                with (binaries / binary).open("rb") as file:
                    tar.addfile(info, file)
            staged.replace(archive)
        print(archive)


def release_assets(version, directory):
    archives = [directory / archive_name(binary, version, platform)
                for platform in PLATFORMS for binary in BINARIES]
    for path in archives:
        if not path.is_file() or path.stat().st_size == 0:
            raise ValueError(f"missing release archive: {path}")
    checksums = directory / "SHA256SUMS"
    lines = []
    for path in archives:
        with path.open("rb") as file:
            digest = hashlib.file_digest(file, "sha256").hexdigest()
        lines.append(f"{digest}  {path.name}\n")
    checksums.write_text("".join(lines))
    return [*archives, checksums]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--platform", required=True, choices=PLATFORMS)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("target/release-assets"))
    args = parser.parse_args()
    with Path("Cargo.toml").open("rb") as file:
        version = tomllib.load(file)["workspace"]["package"]["version"]
    package(version, args.platform, args.binaries, args.output)


if __name__ == "__main__":
    main()
