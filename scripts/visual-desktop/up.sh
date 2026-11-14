#!/usr/bin/env bash
# Run from the repo root. Optional API_BINARY and DESKTOP_BINARY use prebuilt executables.
# Creates a fresh box only when this harness does not already own a live run.
set -euo pipefail
H=$(dirname "$(readlink -f "$0")")
ROOT=${ROOT:-$(dirname "$(dirname "$H")")}
exec python3 "$H/up.py" "$ROOT"
