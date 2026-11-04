#!/usr/bin/env bash
# Install and update else.bot in fresh distribution containers, as an
# unprivileged user with sudo, the way a person would: `curl … | bash`.
# Nothing touches this host's installation.
#
#   scripts/test-install.sh [DISTRO...]
#
# DISTRO: debian-12, debian-13, ubuntu-24.04, ubuntu-26.04, arch (default: all)
#
# Releases come from a throwaway git repository built from the working tree:
# v0.0.1 is the tree as it is, v0.0.2 bumps the version and adds a no-op
# migration (so the update backs up first). Each container installs v0.0.1
# through a pipe, checks the layout and the running API, is refused an update
# while a bot works (which keeps working, and work is accepted again),
# updates to v0.0.2 once idle (history kept, new messages answered), and
# runs the installer once more,
# which changes nothing. SQLite is bundled; containers
# have no systemd, Docker daemon, or KVM, so the API runs as a managed
# process and there is no guest image or bot computer.
#
# The Rust toolchain and build caches stay in Docker volumes
# (else-install-*); remove them with:
#   docker volume rm $(docker volume ls -q -f name=else-install-)
set -euo pipefail
cd "$(dirname "$0")/.."

DISTROS=("$@")
[ ${#DISTROS[@]} -gt 0 ] || DISTROS=(debian-12 debian-13 ubuntu-24.04 ubuntu-26.04 arch)

image_for() {
	case "$1" in
	debian-12) echo debian:bookworm ;;
	debian-13) echo debian:trixie ;;
	ubuntu-24.04) echo ubuntu:24.04 ;;
	ubuntu-26.04) echo ubuntu:26.04 ;;
	arch) echo archlinux:latest ;;
	*) echo "unknown distribution: $1" >&2; exit 2 ;;
	esac
}

work="$(mktemp -d)"
network="else-install-net-$$"
containers=()
cleanup() {
	local c
	for c in "${containers[@]}"; do docker rm -f "$c" >/dev/null 2>&1 || true; done
	docker network rm "$network" >/dev/null 2>&1 || true
	rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# --- Releases v0.0.1 and v0.0.2 from the working tree ------------------------

echo "== releases v0.0.1 and v0.0.2 from the working tree"
mkdir -p "$work/tree"
tar --exclude=./target --exclude=./.git -cf - . | tar -C "$work/tree" -xf -
(
	cd "$work/tree"
	commit() { git -c user.name=test -c user.email=test@example.invalid commit -q -m "$1"; }
	git init -q -b main
	git add -A
	commit "release 0.0.1"
	git tag v0.0.1
	sed -i '/^\[workspace.package\]/,/^\[/s/^version = ".*"/version = "0.0.2"/' Cargo.toml
	awk '/^name = "else-(api|desktop)"$/{print; getline; print "version = \"0.0.2\""; next} {print}' Cargo.lock >Cargo.lock.new
	mv Cargo.lock.new Cargo.lock
	last="$(find apps/api/migrations -name '[0-9]*.sql' -printf '%f\n' | sort | tail -1)"
	next="$(printf '%04d' $((10#${last%%_*} + 1)))"
	printf -- '-- A test release: changes nothing.\nSELECT 1;\n' >"apps/api/migrations/${next}_test_release.sql"
	git add -A
	commit "release 0.0.2"
	git tag v0.0.2
)
git clone -q --bare "$work/tree" "$work/repo.git"
chmod -R a+rX "$work"

# --- The scenario, inside each container -------------------------------------

cat >"$work/prepare.sh" <<'EOF'
set -eu
if command -v apt-get >/dev/null; then
	apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq sudo >/dev/null
else
	pacman -Sy --noconfirm --needed sudo >/dev/null
fi
useradd -m -s /bin/bash tester
echo 'tester ALL=(ALL) NOPASSWD:ALL' >/etc/sudoers.d/tester
# The test repository belongs to another user (older git reads this only
# from the global configuration; git itself is installed later).
printf '[safe]\n\tdirectory = *\n' >/home/tester/.gitconfig
chown -R tester:tester /home/tester
EOF

cat >"$work/scenario.sh" <<'EOF'
set -euo pipefail
fail() { echo "CHECK FAILED: $*" >&2; exit 1; }
export ELSE_REPO=/repo.git ELSE_INSTALL_NO_SYSTEMD=1
H=~/.else
URL=http://127.0.0.1:7878
uuid() { cat /proc/sys/kernel/random/uuid; }
api() { curl -fsS --max-time 10 -H "Authorization: Bearer $(cat $H/data/owner-token)" -H 'content-type: application/json' "$@"; }
health() { curl -fsS --max-time 5 $URL/healthz 2>/dev/null | sed -n 's/.*"version":"\([^"]*\)".*/\1/p'; }
wait_health() { local _; for _ in $(seq 1 60); do [ "$(health)" = "$1" ] && return 0; sleep 1; done; fail "the API does not run $1"; }
send() { api -X POST "$URL/v1/bots/$bot/turns" -d "{\"request_id\":\"$(uuid)\",\"text\":\"$1\",\"provider_id\":\"synthetic\"}" | jq -r .turn.id; }
turn_status() { api "$URL/v1/bots/$bot/conversation" | jq -r --arg t "$1" '.turns[] | select(.id == $t) | .status'; }
wait_status() {
	local _
	for _ in $(seq 1 60); do [ "$(turn_status "$1")" = "$2" ] && return 0; sleep 1; done
	fail "turn $1 is $(turn_status "$1"), not $2"
}

echo "--- the installer refuses root"
if sudo bash /src/install.sh --yes >/tmp/root.log 2>&1; then fail "ran as root"; fi
grep -q "not as root" /tmp/root.log || fail "no root refusal"

echo "--- install v0.0.1 through a pipe"
cat /src/install.sh | ELSE_VERSION=0.0.1 bash -s -- --yes --skip-guest-image 2>&1 | tee /tmp/install.log
[ "$(readlink $H/current)" = versions/0.0.1 ] || fail "current is $(readlink $H/current)"
for f in $H/versions/0.0.1/bin/else-api $H/versions/0.0.1/bin/else-desktop $H/bin/elsebot \
	$H/microsandbox/bin/msb $H/toolchain/cargo/bin/cargo $H/data/owner-token; do
	[ -e "$f" ] || fail "$f missing"
done
[ "$($H/microsandbox/bin/msb --version)" = "msb 0.7.13" ] || fail "msb is not 0.7.13"
[ "$(stat -c %a $H/config/.env)" = 600 ] || fail ".env is not 0600"
[ "$(readlink ~/.local/bin/elsebot)" = "$H/bin/elsebot" ] || fail "no elsebot link"
for outside in ~/.cargo ~/.rustup ~/.microsandbox ~/.config/else-bot ~/.local/share/else-bot; do
	[ ! -e "$outside" ] || fail "$outside was created outside ~/.else"
done
for bin in else-api else-desktop; do
	if ldd $H/current/bin/$bin | grep -q 'not found'; then fail "$bin has unresolved libraries"; fi
done
wait_health 0.0.1
~/.local/bin/elsebot status | tee /tmp/status
grep -q "running on" /tmp/status || fail "elsebot status"

echo "--- elsebot db reads and cannot write"
check_db() {
    [ "$(stat -c %a $H/data/else.sqlite3)" = 600 ] || fail "SQLite file is not 0600"
    [ "$(~/.local/bin/elsebot db 'select count(*) >= 1 from bots')" = 1 ] || fail "elsebot db cannot read"
    if ~/.local/bin/elsebot db 'delete from bots' >/tmp/write.log 2>&1; then fail "elsebot db could write"; fi
    grep -qi 'readonly\|read-only' /tmp/write.log || fail "write was not refused as read-only"
}
check_db

echo "--- an update is refused while a bot works"
echo "ELSE_SYNTHETIC_PROVIDER=1" >>$H/config/.env
~/.local/bin/elsebot restart
wait_health 0.0.1
bot="$(api -X POST $URL/v1/bots -d "{\"request_id\":\"$(uuid)\",\"name\":\"Worker\",\"role\":\"works\"}" | jq -r .bot.id)"
busy="$(send '[synthetic:hang]')"
wait_status "$busy" running
if ~/.local/bin/elsebot update --yes --skip-guest-image >/tmp/refused.log 2>&1; then
	cat /tmp/refused.log
	fail "updated while busy"
fi
cat /tmp/refused.log
grep -q "not updating" /tmp/refused.log || fail "no refusal message"
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
~/.local/bin/elsebot update --yes --skip-guest-image 2>&1 | tee /tmp/update.log
[ "$(readlink $H/current)" = versions/0.0.2 ] || fail "current is $(readlink $H/current)"
wait_health 0.0.2
[ -d $H/versions/0.0.1 ] || fail "the previous release was not kept"
grep -q "Backup before the database changes" /tmp/update.log || fail "no backup before the migration"
ls $H/data/backups/*/complete.json >/dev/null || fail "no completed backup set"
[ "$(turn_status "$done_before")" = completed ] || fail "history was lost"
[ "$(api $URL/v1/bots | jq '[.bots[] | select(.name == "Worker")] | length')" = 1 ] || fail "the bot was lost"
after="$(send 'on the new version')"
wait_status "$after" completed
check_db

echo "--- running it again changes nothing"
~/.local/bin/elsebot update --yes --skip-guest-image 2>&1 | tee /tmp/again.log
grep -q "0.0.2 is installed; checking" /tmp/again.log || fail "not detected as current"
grep -q "already built" /tmp/again.log || fail "rebuilt"
grep -q "all present" /tmp/again.log || fail "installed packages again"
wait_health 0.0.2
~/.local/bin/elsebot stop
echo "ALL CHECKS PASSED"
EOF
chmod 644 "$work"/*.sh

# --- Run ---------------------------------------------------------------------

docker network create "$network" >/dev/null
mkdir -p target/install-tests
results=()
for distro in "${DISTROS[@]}"; do
	image="$(image_for "$distro")"
	log="target/install-tests/$distro.log"
    box="else-install-test-$distro-$$"
    containers+=("$box")
    echo "== $distro ($image) -> $log"
    started=$(date +%s)
	# In the background and waited for: an interrupt then reaches the trap
	# at once, which removes the containers.
	timeout 3600 docker run --rm --name "$box" --network "$network" --platform linux/amd64 \
		-v "$PWD:/src:ro" -v "$work/repo.git:/repo.git:ro" -v "$work:/harness:ro" \
		-v "else-install-toolchain:/home/tester/.else/toolchain" \
		-v "else-install-build-$distro:/home/tester/.else/build" \
        "$image" bash -c "bash /harness/prepare.sh && su - tester -c 'bash /harness/scenario.sh'" \
		>"$log" 2>&1 &
	if wait $!; then
		results+=("$distro: PASS ($(($(date +%s) - started)) s)")
	else
		results+=("$distro: FAIL ($(($(date +%s) - started)) s), see $log")
	fi
	echo "   ${results[${#results[@]}-1]}"
done

echo
printf '%s\n' "${results[@]}"
for r in "${results[@]}"; do
	case "$r" in *FAIL*) exit 1 ;; esac
done
