# Reusable media

Keep reusable artwork and its editable sources here.

`branding/else-bot.svg` is the application logo. It is a vector redraw of
`branding/concepts/robot.png`, with a transparent background and no card or border.
The named paths separate the shell, visor, eyes, and ribbon for future variants.
Keep the concept image as a reference. Use the SVG or generated icons in products.

`branding/else-bot-macos.svg` places the logo on an Obsidian smoked-glass tile.
It references `else-bot.svg` so the robot artwork has one editable source.
The generator inlines that reference as SVG before rasterization to preserve
the robot's details at small sizes.

`branding/icons/` contains transparent logo PNGs from 16 to 1024 pixels and the macOS
ICNS with standard and Retina representations of the Obsidian tile. The release,
installer, and development bundles copy the ICNS into `Contents/Resources` and
declare it in `Info.plist`. The desktop binary embeds the 256-pixel PNG for X11 window icons.
The Linux installer registers the SVG in the hicolor icon theme.

## Regenerate the icons

Install [uv](https://docs.astral.sh/uv/) and librsvg, which provides `rsvg-convert`.
On macOS, install librsvg with `brew install librsvg`.
From the repository root, run:

```sh
uv run scripts/generate-icons.py
```

Run `uv run scripts/generate-icons.py --check` to check that the committed icons
match the SVG sources without changing files. Use the same librsvg version when
checking byte-for-byte output across machines.

Commit the regenerated files with changes to either SVG. Normal builds and installs
use the checked-in files and do not need the generation tools.

To export another size, run:

```sh
rsvg-convert --width 2048 --height 2048 media/branding/else-bot.svg > else-bot-2048.png
```

The macOS representations follow Apple's
[icon size guidance](https://developer.apple.com/library/archive/documentation/GraphicsAnimation/Conceptual/HighResolutionOSX/Optimizing/Optimizing.html#//apple_ref/doc/uid/TP40012302-CH7-SW8).
