#!/usr/bin/env bash
# Disposable demo installation for manual and smoke checks.
#
# Uses its own SQLite file and data
# directory (/tmp/else-demo by default), so it never touches the owner's
# installation. Commands:
#
#   scripts/demo.sh reset            recreate the demo database and data dir, run setup
#   scripts/demo.sh api              run the API (synthetic provider enabled)
#   scripts/demo.sh desktop          run the desktop against the demo API
#   scripts/demo.sh curl ARGS...     curl with the demo owner token (sent via stdin)
#   scripts/demo.sh sql "QUERY"      run SQL against the demo database
#   scripts/demo.sh drop             remove the demo database and data dir
set -euo pipefail
cd "$(dirname "$0")/.."

DEMO=${ELSE_DEMO_DIR:-/tmp/else-demo}
PORT=${ELSE_DEMO_PORT:-17878}
set -a
# shellcheck disable=SC1091
[ -f .env ] && . ./.env
set +a

MARKER="else.bot disposable demo"
owned_or_absent() {
    if [ -e "$DEMO" ] && { [ ! -f "$DEMO/.demo-owned" ] || [ "$(cat "$DEMO/.demo-owned")" != "$MARKER" ]; }; then
        echo "refusing: $DEMO was not created by scripts/demo.sh" >&2
        exit 1
    fi
}

# Never inherit the owner's credentials or credential paths.
unset OWNER_TOKEN_FILE SECRET_KEY SECRET_KEY_FILE ELSE_OWNER_TOKEN ELSE_OWNER_TOKEN_FILE

export ELSE_DATA_DIR="$DEMO/data"
export OWNER_TOKEN_FILE="$DEMO/data/owner-token" SECRET_KEY_FILE="$DEMO/data/secret-key"
export DATABASE_URL="sqlite://$DEMO/data/else.sqlite3"
export ELSE_BOTS_DIR="$DEMO/projects"
export ELSE_API_BIND="127.0.0.1:$PORT"
export ELSE_SYNTHETIC_PROVIDER=${ELSE_SYNTHETIC_PROVIDER:-1}

case "${1:-}" in
reset)
	owned_or_absent
	rm -rf "$DEMO"
	mkdir -p "$DEMO"
	printf '%s\n' "$MARKER" >"$DEMO/.demo-owned"
	cargo run -q -p else-api -- setup
	;;
api) exec cargo run -q -p else-api -- serve ;;
desktop)
	export ELSE_API_URL="http://127.0.0.1:$PORT"
	export ELSE_OWNER_TOKEN_FILE="$DEMO/data/owner-token"
	exec cargo run -q -p else-desktop
	;;
curl)
	shift
	printf 'Authorization: Bearer %s\n' "$(cat "$DEMO/data/owner-token")" |
		curl -s -H @- -H 'content-type: application/json' "$@"
	;;
sql) sqlite3 "$DEMO/data/else.sqlite3" "$2" ;;
drop)
	owned_or_absent
	rm -rf "$DEMO"
	;;
*)
	sed -n '2,15p' "$0"
	exit 2
	;;
esac
