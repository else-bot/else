#!/usr/bin/env bash
# Run install.sh in fresh distribution containers, as an unprivileged user
# with sudo, and check what it installed. Nothing touches this host's
# installation: each run gets a disposable container and a copy of the
# checkout (without target/ and .git/).
#
#   scripts/test-install.sh [--quick] [DISTRO...]
#
# DISTRO: debian-12, debian-13, ubuntu-24.04, ubuntu-26.04, arch (default: all)
# --quick  skip the release build (packages, Rust, Microsandbox, config only)
#
# Containers have no systemd, Docker daemon, or KVM, so the database, guest
# image, and service steps are skipped (--skip-database); their logic runs
# only on a real host. Cargo's registry and each distribution's target
# directory are cached in Docker volumes (else-install-*), so a second full
# run rebuilds quickly; remove them with: docker volume rm $(docker volume ls -q -f name=else-install-)
set -euo pipefail
cd "$(dirname "$0")/.."

QUICK=0
DISTROS=()
for arg in "$@"; do
	case "$arg" in
	--quick) QUICK=1 ;;
	-h | --help) sed -n '2,16p' "$0"; exit 0 ;;
	*) DISTROS+=("$arg") ;;
	esac
done
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

# Runs as root in the container: a sudo-capable user and a copy of the repo.
read -r -d '' PREPARE <<'EOF' || true
set -eu
if command -v apt-get >/dev/null; then
	apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq sudo >/dev/null
else
	pacman -Sy --noconfirm --needed sudo >/dev/null
fi
useradd -m -s /bin/bash tester
echo 'tester ALL=(ALL) NOPASSWD:ALL' >/etc/sudoers.d/tester
mkdir -p /home/tester/else /home/tester/.cargo/registry
tar -C /src --exclude=./target --exclude=./.git -cf - . | tar -C /home/tester/else -xf -
chown -R tester:tester /home/tester
EOF

# Runs as the tester: install, check, install again.
read -r -d '' CHECK <<'EOF' || true
set -euo pipefail
cd ~/else
fail() { echo "CHECK FAILED: $*" >&2; exit 1; }
echo "--- install.sh as root is refused"
if sudo ./install.sh --yes >/tmp/root.log 2>&1; then fail "ran as root"; fi
grep -q "not as root" /tmp/root.log || fail "no root refusal message"

echo "--- first run"
./install.sh --yes --skip-database $INSTALL_ARGS 2>&1 | tee /tmp/first.log

echo "--- checks"
. ~/.cargo/env
rustc --version
command -v bwrap >/dev/null || fail "bubblewrap missing"
command -v docker >/dev/null || fail "docker missing"
docker compose version >/dev/null 2>&1 || fail "docker compose plugin missing"
[ "$(~/.microsandbox/bin/msb --version)" = "msb 0.7.13" ] || fail "msb is not 0.7.13"
env=~/.config/else-bot/.env
[ "$(stat -c %a "$env")" = 600 ] || fail ".env is not 0600"
grep -q '^DATABASE_URL=postgresql://else:' "$env" || fail "no DATABASE_URL"
pg_dump=$(sed -n 's/^ELSE_PG_DUMP=//p' "$env")
"$pg_dump" --version | grep -Eq ' (1[89]|[2-9][0-9])\.' || fail "pg_dump is older than 18: $("$pg_dump" --version)"
if [ "$QUICK" = 0 ]; then
	for bin in else-api else-desktop; do
		[ -x ~/.local/bin/$bin ] || fail "$bin not installed"
		if ldd ~/.local/bin/$bin | grep -q 'not found'; then
			ldd ~/.local/bin/$bin | grep 'not found'
			fail "$bin has unresolved libraries"
		fi
	done
	# An extra argument makes it print its usage and exit (no args = serve).
	usage="$(~/.local/bin/else-api serve extra 2>&1 || true)"
	grep -q "usage: else-api" <<<"$usage" || fail "else-api does not run: $usage"
fi

echo "--- second run is idempotent"
./install.sh --yes --skip-database $INSTALL_ARGS 2>&1 | tee /tmp/second.log
grep -q "all present" /tmp/second.log || fail "the second run installed packages again"
grep -q "kept the existing configuration" /tmp/second.log || fail "the second run rewrote the configuration"
grep -q "msb 0.7.13" /tmp/second.log || fail "msb not detected on the second run"
echo "ALL CHECKS PASSED"
EOF

harness="$(mktemp -d)"
# On any exit, including Ctrl+C: remove this run's container and files.
container=""
cleanup() {
	[ -n "$container" ] && docker rm -f "$container" >/dev/null 2>&1
	rm -rf "$harness"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
printf '%s\n' "$PREPARE" \
	'mkdir -p /cache/registry /cache/target' \
	'chown tester:tester /cache/registry /cache/target' \
	'rmdir /home/tester/.cargo/registry && ln -s /cache/registry /home/tester/.cargo/registry' \
	'ln -s /cache/target /home/tester/else/target' >"$harness/prepare.sh"
printf '%s\n' "$CHECK" >"$harness/check.sh"
chmod 755 "$harness" && chmod 644 "$harness"/*.sh

results=()
mkdir -p target/install-tests
for distro in "${DISTROS[@]}"; do
	image="$(image_for "$distro")"
	log="target/install-tests/$distro.log"
	args=""
	[ "$QUICK" = 1 ] && args="--skip-build"
	echo "== $distro ($image) -> $log"
	started=$(date +%s)
	container="else-install-test-$distro-$$"
	if timeout 3600 docker run --rm --name "$container" --platform linux/amd64 \
		-v "$PWD:/src:ro" -v "$harness:/harness:ro" \
		-v else-install-cargo-registry:/cache/registry \
		-v "else-install-target-$distro:/cache/target" \
		"$image" bash -c "bash /harness/prepare.sh && su - tester -c 'INSTALL_ARGS=\"$args\" QUICK=$QUICK bash /harness/check.sh'" \
		>"$log" 2>&1; then
		results+=("$distro: PASS ($(($(date +%s) - started)) s)")
	else
		results+=("$distro: FAIL ($(($(date +%s) - started)) s), see $log")
	fi
	container=""
	echo "   ${results[${#results[@]}-1]}"
done

echo
printf '%s\n' "${results[@]}"
for r in "${results[@]}"; do
	case "$r" in *FAIL*) exit 1 ;; esac
done
