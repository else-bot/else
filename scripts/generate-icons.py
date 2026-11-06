#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["Pillow==12.0.0"]
# ///
"""Generate desktop icons from media/branding/else-bot.svg. Requires rsvg-convert."""
import argparse
from io import BytesIO
from pathlib import Path
import shutil
import subprocess

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
BRANDING = ROOT / "media/branding"
ICONS = BRANDING / "icons"
SIZES = (16, 32, 64, 128, 256, 512, 1024)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="check committed icons without changing files")
    args = parser.parse_args()
    if not shutil.which("rsvg-convert"):
        raise SystemExit("Install librsvg for rsvg-convert, then rerun this script.")
    if not args.check:
        ICONS.mkdir(parents=True, exist_ok=True)
    outputs = {}
    images = []
    for size in SIZES:
        png = subprocess.check_output([
            "rsvg-convert", "--width", str(size), "--height", str(size), str(BRANDING / "else-bot.svg")])
        outputs[f"else-bot-{size}.png"] = png
        images.append(Image.open(BytesIO(png)))
    icns = BytesIO()
    images[-1].save(icns, format="ICNS", append_images=images[:-1])
    outputs["else-bot.icns"] = icns.getvalue()
    for name, content in outputs.items():
        path = ICONS / name
        if args.check:
            if not path.is_file() or path.read_bytes() != content:
                raise SystemExit(f"{path.relative_to(ROOT)} is stale; run uv run scripts/generate-icons.py")
        else:
            path.write_bytes(content)
    print(f"{'Verified' if args.check else 'Generated'} {len(SIZES)} PNG sizes and else-bot.icns")


if __name__ == "__main__":
    main()
