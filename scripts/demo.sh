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

MARKER="else.bot disposable demo database"
OWNER_DB=$(printf '%s' "${DATABASE_URL:-}" | sed -n 's|.*/\([^/?]*\).*|\1|p')
if [ "$DB" = "$POSTGRES_DB" ] || [ "$DB" = "$OWNER_DB" ]; then
	echo "refusing: demo database '$DB' is the owner's database" >&2
	exit 1
fi
# Destructive commands only touch a database created by this script.
owned_or_absent() {
	local comment
	comment=$(psql_ "$POSTGRES_DB" "SELECT shobj_description(oid, 'pg_database') FROM pg_database WHERE datname = '$DB'")
	if [ -n "$comment" ] && [ "$comment" != "$MARKER" ]; then
		echo "refusing: database '$DB' was not created by scripts/demo.sh" >&2
		exit 1
	fi
	if psql_ "$POSTGRES_DB" "SELECT 1 FROM pg_database WHERE datname = '$DB'" | grep -q 1 && [ -z "$comment" ]; then
		echo "refusing: database '$DB' exists without the demo marker" >&2
		exit 1
	fi
}

# Never inherit the owner's credentials or credential paths.
unset OWNER_TOKEN_FILE SECRET_KEY SECRET_KEY_FILE ELSE_OWNER_TOKEN ELSE_OWNER_TOKEN_FILE

export ELSE_DATA_DIR="$DEMO/data"
export OWNER_TOKEN_FILE="$DEMO/data/owner-token" SECRET_KEY_FILE="$DEMO/data/secret-key"
export DATABASE_URL="postgresql://$POSTGRES_USER:$POSTGRES_PASSWORD@127.0.0.1:${POSTGRES_PORT:-5432}/$DB"
export ELSE_API_BIND="127.0.0.1:$PORT"
export ELSE_SYNTHETIC_PROVIDER=${ELSE_SYNTHETIC_PROVIDER:-1}

case "${1:-}" in
reset)
	owned_or_absent
	psql_ "$POSTGRES_DB" "DROP DATABASE IF EXISTS $DB WITH (FORCE)"
	psql_ "$POSTGRES_DB" "CREATE DATABASE $DB"
	psql_ "$POSTGRES_DB" "COMMENT ON DATABASE $DB IS '$MARKER'"
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
	owned_or_absent
	psql_ "$POSTGRES_DB" "DROP DATABASE IF EXISTS $DB WITH (FORCE)"
	rm -rf "$DEMO"
	;;
*)
	sed -n '2,15p' "$0"
	exit 2
	;;
esac
