#!/usr/bin/env bash
# Disposable demo installation for manual and smoke checks.
#
# Uses its own database (else_demo on the Compose PostgreSQL) and data
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
DB=${ELSE_DEMO_DB:-else_demo}
PORT=${ELSE_DEMO_PORT:-17878}
set -a
# shellcheck disable=SC1091
[ -f .env ] && . ./.env
set +a

psql_() { docker compose exec -T postgres psql -U "$POSTGRES_USER" -d "$1" -Atq -c "$2"; }

export ELSE_DATA_DIR="$DEMO/data"
export DATABASE_URL="postgresql://$POSTGRES_USER:$POSTGRES_PASSWORD@127.0.0.1:${POSTGRES_PORT:-5432}/$DB"
export ELSE_API_BIND="127.0.0.1:$PORT"
export ELSE_SYNTHETIC_PROVIDER=${ELSE_SYNTHETIC_PROVIDER:-1}

case "${1:-}" in
reset)
	psql_ "$POSTGRES_DB" "DROP DATABASE IF EXISTS $DB WITH (FORCE)"
	psql_ "$POSTGRES_DB" "CREATE DATABASE $DB"
	rm -rf "$DEMO"
	mkdir -p "$DEMO"
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
sql) psql_ "$DB" "$2" ;;
drop)
	psql_ "$POSTGRES_DB" "DROP DATABASE IF EXISTS $DB WITH (FORCE)"
	rm -rf "$DEMO"
	;;
*)
	sed -n '2,15p' "$0"
	exit 2
	;;
esac
