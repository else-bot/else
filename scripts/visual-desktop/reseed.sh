#!/usr/bin/env bash
# Reseed a live session from up.sh, or pass BOX STATE_DIR explicitly.
set -euo pipefail
H=$(dirname "$(readlink -f "$0")")
ROOT=${ROOT:-$(dirname "$(dirname "$H")")}
STATE=${HARNESS_STATE_DIR:-$ROOT/.else-dev/visual-desktop}
if [[ $# == 0 ]]; then
  mapfile -t session < <(python3 - "$STATE/session.json" <<'PY'
import json,sys
s=json.load(open(sys.argv[1]))
print(s['box'])
print(s['state'])
PY
)
  BOX=${session[0]:?run up.sh first} INSIDE=${session[1]:?session has no state directory}
else
  BOX=${1:?box required} INSIDE=${2:?state directory required}
fi
BHOME=$(omabox -b "$BOX" path --home)
command cp -f "$H/seed.py" "$BHOME/seed.py"
omabox -b "$BOX" run -- python3 /home/sbx/seed.py "$INSIDE"
