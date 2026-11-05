#!/bin/sh
# Regenerates infra/guest/x11vnc/keymap-settle.patch from the verified pristine source.
set -eu
R=$(cd "$(dirname "$0")/../../.." && pwd)
S=$R/.else-dev/pr62-arm64/x11vnc-source/upstream/x11vnc-0.9.17/src
W=$(mktemp -d)
mkdir -p "$W/a/src" "$W/b/src"
for f in keyboard.h keyboard.c connections.c; do /bin/cp -f "$S/$f" "$W/a/src/"; done
python3 -I "$R/.else-dev/pr62-arm64/guest-keymap-fix/make-patch.py" "$S" "$W/b/src"
{
cat <<'HDR'
Description: Settle and recycle added keysyms; release on viewer exit
 add_keysym() mapped a missing keysym and the caller pressed it at once,
 so a client could read the press before the new mapping and lose the
 character. It now waits for the server (XSync) and a 20 ms settle before
 the press. When no keycode is empty it recycles the least recently
 mapped added keycode that is not held, instead of ignoring the
 keystroke, and lookups that still name a recycled keycode are treated
 as misses. A key release never adds a mapping.
 .
 Presses queued from a viewer that has hung up are dropped, so a long
 commit cannot keep typing for seconds after the viewer left, while its
 releases still apply. When a viewer goes, keys and buttons x11vnc still
 holds for it are released and the server is synced before the -gone
 command runs, so that command can acknowledge that the viewer's input
 is finished; the connection's port is read at accept, since the socket is
 closed before that command runs. Diagnostics never name keys.
Author: else.bot
Forwarded: not-needed
Last-Update: 2026-11-05
---
HDR
cd "$W"
for f in keyboard.h keyboard.c connections.c; do diff -u --label a/src/$f --label b/src/$f a/src/$f b/src/$f || true; done
} > "$R/infra/guest/x11vnc/keymap-settle.patch"
rm -rf "$W"
