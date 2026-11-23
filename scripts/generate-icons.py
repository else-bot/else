#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["Pillow==12.0.0"]
# ///
"""Generate the desktop and iOS icons from media/branding SVG sources. Requires rsvg-convert."""
import argparse
from io import BytesIO
from pathlib import Path
import shutil
import subprocess
from xml.etree import ElementTree

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
BRANDING = ROOT / "media/branding"
ICONS = BRANDING / "icons"
IOS_ICON = ROOT / "apps/mobile/icon.png"
SIZES = (16, 32, 64, 128, 256, 512, 1024)
SVG = "{http://www.w3.org/2000/svg}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="check committed icons without changing files")
    args = parser.parse_args()
    if not shutil.which("rsvg-convert"):
        raise SystemExit("Install librsvg for rsvg-convert, then rerun this script.")
    if not args.check:
        ICONS.mkdir(parents=True, exist_ok=True)
    macos_source = tile("else-bot-macos.svg")
    outputs = {}
    images = []
    for size in SIZES:
        png = subprocess.check_output([
            "rsvg-convert", "--width", str(size), "--height", str(size), str(BRANDING / "else-bot.svg")])
        outputs[f"else-bot-{size}.png"] = png
        macos_png = subprocess.check_output([
            "rsvg-convert", "--width", str(size), "--height", str(size)], input=macos_source)
        images.append(Image.open(BytesIO(macos_png)))
    icns = BytesIO()
    images[-1].save(icns, format="ICNS", append_images=images[:-1])
    outputs["else-bot.icns"] = icns.getvalue()
    # The App Store refuses an iOS icon with an alpha channel.
    ios_png = subprocess.check_output([
        "rsvg-convert", "--width", "1024", "--height", "1024"], input=tile("else-bot-ios.svg"))
    ios = BytesIO()
    Image.open(BytesIO(ios_png)).convert("RGB").save(ios, format="PNG")
    outputs = {ICONS / name: content for name, content in outputs.items()}
    outputs[IOS_ICON] = ios.getvalue()
    for path, content in outputs.items():
        if args.check:
            if not path.is_file() or path.read_bytes() != content:
                raise SystemExit(f"{path.relative_to(ROOT)} is stale; run uv run scripts/generate-icons.py")
        else:
            path.write_bytes(content)
    print(f"{'Verified' if args.check else 'Generated'} {len(SIZES)} PNG sizes, else-bot.icns and the iOS icon")


def tile(name):
    """An icon tile's SVG with its mascot image inlined, which rsvg-convert does not load."""
    svg = ElementTree.parse(BRANDING / name).getroot()
    mascot = svg.find(f".//{SVG}image")
    logo = ElementTree.parse(BRANDING / mascot.attrib.pop("href")).getroot()
    mascot.tag = logo.tag
    mascot.set("viewBox", logo.attrib["viewBox"])
    mascot.extend(child for child in logo if child.tag not in (f"{SVG}title", f"{SVG}desc"))
    return ElementTree.tostring(svg)


if __name__ == "__main__":
    main()
