#!/usr/bin/env bash
# Compatibility entry point: the "demo" session of scripts/dev.py.
#
#   scripts/demo.sh reset | api | desktop | status | drop
#   scripts/demo.sh curl [CURL ARGS...] /v1/PATH
#   scripts/demo.sh sql "QUERY"
#
# ELSE_DEMO_DIR names an explicit state directory; dev.py refuses it unless
# this checkout's demo session created it. ELSE_DEMO_PORT is no longer used:
# the API picks a free port and `scripts/demo.sh status` prints it.
set -euo pipefail
command=${1:-}
[ -n "$command" ] || exec python3 "$(dirname "$0")/dev.py" --help
shift
exec python3 "$(dirname "$0")/dev.py" "$command" --session demo ${ELSE_DEMO_DIR:+--dir "$ELSE_DEMO_DIR"} "$@"
