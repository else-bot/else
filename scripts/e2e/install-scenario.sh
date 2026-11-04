#!/usr/bin/env bash
# Shared assertions for the new native arm64 installer lanes.
set -euo pipefail
fail() { echo "CHECK FAILED: $*" >&2; exit 1; }
export ELSE_REPO="${ELSE_REPO:-/repo.git}"
if [ "${ELSE_E2E_LAUNCHD:-0}" = 1 ]; then
    unset ELSE_INSTALL_NO_SYSTEMD
else
    export ELSE_INSTALL_NO_SYSTEMD=1
fi
SOURCE="${ELSE_E2E_SOURCE:-/src}"
LINK_PREFIX="${ELSE_E2E_LINK_PREFIX:-$HOME/.local}"
POLL="${ELSE_E2E_POLL_INTERVAL:-1}"
WAIT_SECONDS="${ELSE_E2E_WAIT_SECONDS:-60}"
H="${ELSE_HOME:-$HOME/.else}"
cli="$H/bin/elsebot"
URL="http://127.0.0.1:${ELSE_API_PORT:-7878}"
uuid() { if [ -r /proc/sys/kernel/random/uuid ]; then cat /proc/sys/kernel/random/uuid; else uuidgen; fi; }
mode() { if [ "$(uname -s)" = Darwin ]; then stat -f %Lp "$1"; else stat -c %a "$1"; fi; }
api() { curl -fsS --max-time 10 -H "Authorization: Bearer $(cat $H/data/owner-token)" -H 'content-type: application/json' "$@"; }
health() { curl -fsS --max-time 5 $URL/healthz 2>/dev/null | sed -n 's/.*"version":"\([^"]*\)".*/\1/p'; }
wait_health() { local deadline=$((SECONDS + WAIT_SECONDS)); while [ "$SECONDS" -lt "$deadline" ]; do [ "$(health)" = "$1" ] && return 0; sleep "$POLL"; done; fail "the API does not run $1"; }
send() { api -X POST "$URL/v1/bots/$bot/turns" -d "{\"request_id\":\"$(uuid)\",\"text\":\"$1\",\"provider_id\":\"synthetic\"}" | jq -r .turn.id; }
turn_status() { api "$URL/v1/bots/$bot/conversation" | jq -r --arg t "$1" '.turns[] | select(.id == $t) | .status'; }
wait_status() {
	local deadline=$((SECONDS + WAIT_SECONDS))
	while [ "$SECONDS" -lt "$deadline" ]; do [ "$(turn_status "$1")" = "$2" ] && return 0; sleep "$POLL"; done
	fail "turn $1 is $(turn_status "$1"), not $2"
}

echo "--- the installer refuses root"
if [ "$(uname -s)" != Darwin ]; then
    if sudo bash "$SOURCE/install.sh" --yes >"$H-root.log" 2>&1; then fail "ran as root"; fi
    grep -q "not as root" "$H-root.log" || fail "no root refusal"
fi

echo "--- install v0.0.1 through a pipe"
cat "$SOURCE/install.sh" | ELSE_VERSION=0.0.1 bash -s -- --yes --skip-guest-image 2>&1 | tee "$H-install.log"
[ "$(readlink $H/current)" = versions/0.0.1 ] || fail "current is $(readlink $H/current)"
cache="${ELSE_INSTALL_TEST_CACHE:-$H}"
for f in "$H/versions/0.0.1/bin/else-api" "$H/versions/0.0.1/bin/else-desktop" "$cli" \
    "$cache/microsandbox/bin/msb" "$cache/toolchain/cargo/bin/cargo" "$H/data/owner-token"; do
	[ -e "$f" ] || fail "$f missing"
done
[ "$("$cache/microsandbox/bin/msb" --version)" = "msb 0.7.13" ] || fail "msb is not 0.7.13"
[ "$(mode "$H/config/.env")" = 600 ] || fail ".env is not 0600"
grep -Fx "DATABASE_URL=sqlite://$H/data/else.sqlite3" "$H/config/.env" >/dev/null || fail "database is outside the disposable installation"
[ ! -e "$H/config/compose.yaml" ] || fail "the installer created database services"
[ "$(readlink "$LINK_PREFIX/bin/elsebot")" = "$H/bin/elsebot" ] || fail "no elsebot link"
if [ "$(uname -s)" != Darwin ]; then
    for outside in ~/.cargo ~/.rustup ~/.microsandbox ~/.config/else-bot ~/.local/share/else-bot; do
        [ ! -e "$outside" ] || fail "$outside was created outside ~/.else"
    done
fi
for bin in else-api else-desktop; do
	if [ "$(uname -s)" = Darwin ]; then
        file "$H/current/bin/$bin" | grep -q arm64 || fail "$bin is not native arm64"
        otool -L "$H/current/bin/$bin"
    else
        file "$H/current/bin/$bin" | grep -q 'ARM aarch64' || fail "$bin is not native arm64"
        if ldd "$H/current/bin/$bin" | grep -q 'not found'; then fail "$bin has unresolved libraries"; fi
    fi
done
wait_health 0.0.1
"$cli" status | tee "$H-status"
grep -q "running on" "$H-status" || fail "elsebot status"

echo "--- elsebot db reads and cannot write"
check_db() {
    [ "$(mode "$H/data/else.sqlite3")" = 600 ] || fail "SQLite file is not 0600"
    [ "$("$cli" db 'select count(*) >= 1 from bots')" = 1 ] || fail "elsebot db cannot read"
    [ "$("$cli" db 'pragma integrity_check')" = ok ] || fail "SQLite integrity check failed"
    if "$cli" db 'delete from bots' >"$H-write.log" 2>&1; then fail "elsebot db could write"; fi
    grep -qi 'readonly\|read-only' "$H-write.log" || { cat "$H-write.log"; fail "write was not refused as read-only"; }
}
check_db

echo "--- an update is refused while a bot works"
echo "ELSE_SYNTHETIC_PROVIDER=1" >>$H/config/.env
"$cli" restart
wait_health 0.0.1
bot="$(api -X POST $URL/v1/bots -d "{\"request_id\":\"$(uuid)\",\"name\":\"Worker\",\"role\":\"works\"}" | jq -r .bot.id)"
busy="$(send '[synthetic:hang]')"
wait_status "$busy" running
if "$cli" update --yes --skip-guest-image >"$H-refused.log" 2>&1; then
	cat "$H-refused.log"
	fail "updated while busy"
fi
cat "$H-refused.log"
grep -q "not updating" "$H-refused.log" || fail "no refusal message"
[ "$(readlink $H/current)" = versions/0.0.1 ] || fail "current changed"
[ "$(health)" = 0.0.1 ] || fail "0.0.1 no longer runs"
[ "$(turn_status "$busy")" = running ] || fail "the busy turn was disturbed"
queued="$(send 'after the refusal')"
[ -n "$queued" ] && [ "$queued" != null ] || fail "work is not accepted after the refusal"

echo "--- update to v0.0.2 once idle"
# Stop work is bot-wide: it also ends the message queued behind the busy turn.
api -X POST "$URL/v1/bots/$bot/cancel" -d "{\"request_id\":\"$(uuid)\"}" >/dev/null
wait_status "$busy" interrupted
done_before="$(send 'before the update')"
wait_status "$done_before" completed
"$cli" update --yes --skip-guest-image 2>&1 | tee "$H-update.log"
[ "$(readlink $H/current)" = versions/0.0.2 ] || fail "current is $(readlink $H/current)"
wait_health 0.0.2
[ -d $H/versions/0.0.1 ] || fail "the previous release was not kept"
grep -q "Backup before the database changes" "$H-update.log" || fail "no backup before the migration"
ls $H/data/backups/*/complete.json >/dev/null || fail "no completed backup set"
[ "$(turn_status "$done_before")" = completed ] || fail "history was lost"
[ "$(api $URL/v1/bots | jq '[.bots[] | select(.name == "Worker")] | length')" = 1 ] || fail "the bot was lost"
after="$(send 'on the new version')"
wait_status "$after" completed
check_db

echo "--- running it again changes nothing"
"$cli" update --yes --skip-guest-image 2>&1 | tee "$H-again.log"
grep -q "0.0.2 is installed; checking" "$H-again.log" || fail "not detected as current"
grep -q "already built" "$H-again.log" || fail "rebuilt"
if [ "$(uname -s)" = Darwin ]; then
    grep -q "not needed (no guest image)" "$H-again.log" || fail "the installer required Docker without a guest image"
else
    grep -q "all present" "$H-again.log" || fail "installed packages again"
fi
wait_health 0.0.2
"$cli" stop
[ "$(cat "$H/current/.build-profile")" = "${ELSE_INSTALL_TEST_PROFILE:-release}" ] || fail "wrong build profile"
echo "ALL CHECKS PASSED"
