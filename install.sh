#!/usr/bin/env bash
# else.bot installer and updater.
#
#   curl -fsSL https://raw.githubusercontent.com/else-bot/else/main/install.sh | bash
#
# Installs the latest release into ~/.else, or updates an existing
# installation to it; running it again is how you update (`elsebot update`
# does the same). Linux (Debian 12+, Ubuntu 24.04+, Arch Linux / Omarchy;
# x86_64) gets the API, the desktop, and everything they need; macOS gets the
# desktop client only (the API needs Linux; untested so far).
#
# Everything it installs or keeps lives in ~/.else:
#   src/        the release's source (a git checkout at the release tag)
#   versions/   built releases, versions/<version>/bin/{else-api,else-desktop}
#   current     -> versions/<version>, the release in use
#   bin/        elsebot, and else-api / else-desktop through current
#   config/     .env (settings and database password, 0600), compose.yaml
#   data/       the installation: credentials, bot computers, artifacts, backups
#   bots/       each bot's project directory (bots/<bot id>), where its provider
#               CLI starts on the host; mounted as ~/work in its computer
#   toolchain/  Rust (rustup and cargo), only for building else.bot
#   microsandbox/  the VM runtime (msb and libkrunfw)
#   build/      the build cache
#   logs/       install and update logs; the API log without systemd
# Outside it: system packages (with sudo, only when missing), the Docker
# volume of the database, the guest image in Docker, the systemd user unit
# ~/.config/systemd/user/else-api.service, and the links ~/.local/bin/elsebot
# and ~/.local/share/applications/else-bot.desktop.
#
# An update builds the new release first, while the old one keeps running;
# then it asks the API to stop taking work and refuses (changing nothing)
# if bots are working. Otherwise it stops the API, backs up first when the
# release changes the database, switches versions, starts the new API, and
# switches back if it does not come up. The previous release is kept.
#
# Settings (environment): ELSE_VERSION=0.0.2 installs that release instead
# of the latest; ELSE_HOME, ELSE_REPO, ELSE_API_PORT, ELSE_SERVICE_NAME,
# ELSE_DATABASE_URL (use an existing PostgreSQL instead of Docker), and
# ELSE_INSTALL_NO_SYSTEMD=1 exist mostly for tests.
set -euo pipefail

usage() {
	cat <<'EOF'
Usage: install.sh [options]      (or: curl -fsSL …/install.sh | bash -s -- [options])

  -y, --yes            do not ask for confirmation (sudo may still ask for a password)
  --version X.Y.Z      install or update to this release instead of the latest
  --server-only        Linux: no desktop client (headless servers)
  --skip-guest-image   do not build the guest VM image (bot computers then use a plain image)
  -h, --help           show this help
EOF
}

# --- Settings ---------------------------------------------------------------

MIN_RUST="1.94.0"
MSB_VERSION="0.7.13"
COMPOSE_VERSION="5.6.1"
PG_MAJOR="18"
GUEST_TAG="else-guest:2"

ELSE_HOME="${ELSE_HOME:-$HOME/.else}"
REPO_URL="${ELSE_REPO:-https://github.com/else-bot/else.git}"
API_PORT="${ELSE_API_PORT:-7878}"
SERVICE="${ELSE_SERVICE_NAME:-else-api}"
WANT_VERSION="${ELSE_VERSION:-}"
ASSUME_YES=0
SERVER_ONLY=0
SKIP_GUEST=0
ARGS=("$@")

SRC="$ELSE_HOME/src"
BUILD="$ELSE_HOME/build"
VERSIONS="$ELSE_HOME/versions"
CURRENT="$ELSE_HOME/current"
BIN="$ELSE_HOME/bin"
CONFIG="$ELSE_HOME/config"
DATA="$ELSE_HOME/data"
LOGS="$ELSE_HOME/logs"
ENV_FILE="$CONFIG/.env"
export RUSTUP_HOME="$ELSE_HOME/toolchain/rustup"
export CARGO_HOME="$ELSE_HOME/toolchain/cargo"
MSB_PREFIX="$ELSE_HOME/microsandbox"
MSB="$MSB_PREFIX/bin/msb"
USER="${USER:-$(id -un)}"

# --- Output -----------------------------------------------------------------

if [ -t 1 ]; then
	BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; RESET=$'\033[0m'
else
	BOLD=""; DIM=""; RED=""; GREEN=""; YELLOW=""; RESET=""
fi
WARNINGS=""
NEXT=""
step() { printf '\n%s==> %s%s\n' "$BOLD" "$1" "$RESET"; }
info() { printf '    %s\n' "$1"; }
ok() { printf '    %s✓%s %s\n' "$GREEN" "$RESET" "$1"; }
warn() { printf '    %s!%s %s\n' "$YELLOW" "$RESET" "$1"; WARNINGS="${WARNINGS}  - $1"$'\n'; }
die() { printf '\n%serror:%s %s\n' "$RED" "$RESET" "$1" >&2; exit 1; }
next() { NEXT="${NEXT}  - $1"$'\n'; }
have() { command -v "$1" >/dev/null 2>&1; }

# Prompts read the terminal, also under `curl … | bash`.
confirm() {
	[ "$ASSUME_YES" = 1 ] && return 0
	if ! { true </dev/tty; } 2>/dev/null; then
		die "$1 Re-run with --yes to proceed without a terminal."
	fi
	printf '    %s [y/N] ' "$1"
	local answer
	read -r answer </dev/tty
	case "$answer" in y | Y | yes | YES) return 0 ;; *) return 1 ;; esac
}

# version_ge A B: A >= B for dotted numbers.
version_ge() {
	[ "$(printf '%s\n%s\n' "$2" "$1" | sort -t. -k1,1n -k2,2n -k3,3n | head -1)" = "$2" ]
}

SUDO=""
SUDO_READY=0
sudo_ready() {
	[ "$SUDO_READY" = 1 ] && return
	have sudo || die "sudo is required to install system packages"
	SUDO="sudo"
	info "${DIM}sudo may ask for your password${RESET}"
	# The password prompt reads the terminal, also under `curl … | bash`.
	# shellcheck disable=SC2024
	sudo -v </dev/tty 2>/dev/null || sudo -v || die "sudo was refused"
	SUDO_READY=1
}

# --- Platform ---------------------------------------------------------------

OS=""
ARCH=""
DISTRO=""
DISTRO_NAME=""
detect_platform() {
	OS="$(uname -s)"
	ARCH="$(uname -m)"
	case "$OS" in
	Linux)
		[ -r /etc/os-release ] || die "cannot identify this Linux distribution (/etc/os-release is missing)"
		# shellcheck disable=SC1091
		. /etc/os-release
		DISTRO_NAME="${PRETTY_NAME:-$ID}"
		case " ${ID:-} ${ID_LIKE:-} " in
		*" arch "* | *" omarchy "*) DISTRO=arch ;;
		*" ubuntu "*) DISTRO=ubuntu ;;
		*" debian "*) DISTRO=debian ;;
		*) die "unsupported Linux distribution: $DISTRO_NAME (supported: Debian, Ubuntu, Arch Linux / Omarchy)" ;;
		esac
		;;
	Darwin)
		DISTRO=macos
		DISTRO_NAME="macOS $(sw_vers -productVersion 2>/dev/null || true)"
		;;
	*) die "unsupported operating system: $OS (supported: Linux, macOS)" ;;
	esac
	case "$ARCH" in
	x86_64 | amd64) ARCH=x86_64 ;;
	aarch64 | arm64) ARCH=aarch64 ;;
	*) die "unsupported architecture: $ARCH" ;;
	esac
	if [ "$(id -u)" = 0 ]; then
		die "run the installer as the user who will own else.bot, not as root; it asks for sudo when needed"
	fi
}

# --- System packages --------------------------------------------------------

pkg_installed() {
	case "$DISTRO" in
	debian | ubuntu) [ "$(dpkg-query -W -f='${Status}' "$1" 2>/dev/null || true)" = "install ok installed" ] ;;
	arch) pacman -Q "$1" >/dev/null 2>&1 || pacman -Qg "$1" >/dev/null 2>&1 ;;
	macos) return 0 ;;
	esac
}

missing_packages() {
	local pkg missing=""
	for pkg in "$@"; do
		pkg_installed "$pkg" || missing="$missing $pkg"
	done
	echo "${missing# }"
}

APT_UPDATED=0
install_packages() {
	[ $# -gt 0 ] || return 0
	sudo_ready
	case "$DISTRO" in
	debian | ubuntu)
		if [ "$APT_UPDATED" = 0 ]; then
			$SUDO env DEBIAN_FRONTEND=noninteractive apt-get update -qq
			APT_UPDATED=1
		fi
		$SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "$@"
		;;
	arch) $SUDO pacman -Sy --needed --noconfirm "$@" ;;
	esac
}

# The bootstrap only needs git, curl, and certificates.
ensure_bootstrap_tools() {
	if [ "$DISTRO" = macos ]; then
		have git || die "git is missing: run 'xcode-select --install', then try again"
		return
	fi
	local missing
	missing="$(missing_packages git curl ca-certificates)"
	[ -z "$missing" ] && return
	info "Missing to fetch else.bot: $missing"
	confirm "Install them with sudo?" || die "git and curl are required"
	# shellcheck disable=SC2086
	install_packages $missing
}

apt_packages() {
	local list="build-essential pkg-config curl ca-certificates git jq tar gzip"
	if [ "$SERVER_ONLY" = 0 ]; then
		list="$list libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libxcb1-dev libx11-xcb-dev libx11-dev libfontconfig-dev libfreetype-dev libvulkan1 libzstd-dev"
	fi
	echo "$list"
}

pacman_packages() {
	local list="base-devel pkgconf curl ca-certificates git jq tar gzip postgresql-libs"
	if [ "$SERVER_ONLY" = 0 ]; then
		list="$list libxkbcommon libxkbcommon-x11 wayland libxcb libx11 fontconfig freetype2 vulkan-icd-loader zstd"
	fi
	echo "$list"
}

# Docker Engine with the Compose plugin; any existing Docker is kept. Not
# needed with an external database and no guest image.
docker_packages() {
	if [ -n "${ELSE_DATABASE_URL:-}" ] && [ "$SKIP_GUEST" = 1 ]; then return 0; fi
	if have docker; then
		docker compose version >/dev/null 2>&1 && return 0
		case "$DISTRO" in
		ubuntu) echo "docker-compose-v2" ;;
		arch) echo "docker-compose" ;;
		esac
		return 0
	fi
	case "$DISTRO" in
	ubuntu) echo "docker.io docker-compose-v2" ;;
	# Debian 12's docker-compose is the old v1 tool; see install_compose_plugin.
	debian) if [ "${VERSION_ID%%.*}" -ge 13 ] 2>/dev/null; then echo "docker.io docker-compose"; else echo "docker.io"; fi ;;
	arch) echo "docker docker-compose" ;;
	esac
}

# Docker's own Compose plugin where none is packaged (Debian 12), system-wide.
install_compose_plugin() {
	have docker || return 0
	docker compose version >/dev/null 2>&1 && return 0
	local base="https://github.com/docker/compose/releases/download/v$COMPOSE_VERSION"
	local asset="docker-compose-linux-$ARCH" tmp
	info "Installing the Docker Compose plugin $COMPOSE_VERSION"
	tmp="$(mktemp -d)"
	curl -fsSL "$base/$asset" -o "$tmp/$asset"
	curl -fsSL "$base/$asset.sha256" -o "$tmp/$asset.sha256"
	(cd "$tmp" && sha256sum -c --quiet "$asset.sha256") || die "checksum verification of the Compose plugin failed"
	sudo_ready
	$SUDO install -D -m 755 "$tmp/$asset" /usr/local/lib/docker/cli-plugins/docker-compose
	rm -rf "$tmp"
	docker compose version >/dev/null 2>&1 || die "docker compose still does not work"
}

PG_DUMP=""
PG_RESTORE=""
PSQL=""
pg_major() { "$1" --version 2>/dev/null | sed -n 's/.* \([0-9][0-9]*\)\..*/\1/p' | head -1; }
find_pg_tools() {
	local dir major
	for dir in "/usr/lib/postgresql/$PG_MAJOR/bin" /usr/bin /usr/local/bin; do
		if [ -x "$dir/pg_dump" ] && [ -x "$dir/pg_restore" ]; then
			major="$(pg_major "$dir/pg_dump")"
			if [ -n "$major" ] && [ "$major" -ge "$PG_MAJOR" ]; then
				PG_DUMP="$dir/pg_dump"
				PG_RESTORE="$dir/pg_restore"
				PSQL="$dir/psql"
				return 0
			fi
		fi
	done
	return 1
}

# Debian and Ubuntu ship older client tools: use the PostgreSQL apt repository.
setup_pgdg() {
	if [ -f /etc/apt/sources.list.d/pgdg.list ] || [ -f /etc/apt/sources.list.d/pgdg.sources ]; then return 0; fi
	info "Adding the PostgreSQL apt repository (apt.postgresql.org) for postgresql-client-$PG_MAJOR"
	install_packages postgresql-common
	$SUDO /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y >/dev/null
}

linux_packages() {
	step "System packages"
	local wanted="" missing need_pg=0
	case "$DISTRO" in
	debian | ubuntu) wanted="$(apt_packages) $(docker_packages)" ;;
	arch) wanted="$(pacman_packages) $(docker_packages)" ;;
	esac
	# shellcheck disable=SC2086
	missing="$(missing_packages $wanted)"
	find_pg_tools || need_pg=1
	if [ -z "$missing" ] && [ "$need_pg" = 0 ]; then
		ok "all present"
	else
		[ -n "$missing" ] && info "Missing: $missing"
		if [ "$need_pg" = 1 ] && [ "$DISTRO" != arch ]; then
			info "Missing: postgresql-client-$PG_MAJOR (pg_dump and pg_restore for backups)"
		fi
		confirm "Install them with sudo?" || die "cannot continue without the system packages"
		# shellcheck disable=SC2086
		install_packages $missing
		if [ "$need_pg" = 1 ] && [ "$DISTRO" != arch ]; then
			setup_pgdg
			APT_UPDATED=0
			install_packages "postgresql-client-$PG_MAJOR"
		fi
		# Debian 13 and newer Ubuntus ship the Docker CLI separately.
		if ! have docker && [ "$DISTRO" != arch ] && [ -n "$(docker_packages)" ]; then
			install_packages docker-cli
		fi
		find_pg_tools || die "pg_dump $PG_MAJOR or newer is still missing"
		ok "installed"
	fi
	install_compose_plugin
	ok "pg_dump: $PG_DUMP ($("$PG_DUMP" --version | head -1))"
}

# --- Docker, KVM, and the sandbox -------------------------------------------

DOCKER_VIA_SG=0
in_group() { case " $(id -nG "$USER" 2>/dev/null || true) " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
active_in_session() { case " $(id -nG 2>/dev/null || true) " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
has_systemd() { [ -d /run/systemd/system ]; }

# docker ARGS…, through `sg docker` while the group is new to this session.
docker_run() {
	if [ "$DOCKER_VIA_SG" = 0 ]; then
		docker "$@"
	else
		local quoted="" arg
		for arg in "$@"; do quoted="$quoted $(printf '%q' "$arg")"; done
		sg docker -c "docker$quoted"
	fi
}

linux_access() {
	step "Docker, KVM, and the provider sandbox"
	if have docker; then
		if has_systemd && ! systemctl is-active --quiet docker 2>/dev/null; then
			sudo_ready
			if $SUDO systemctl enable --now docker >/dev/null 2>&1; then ok "Docker service started"; else warn "could not start the Docker service"; fi
		fi
		if ! in_group docker && getent group docker >/dev/null 2>&1; then
			if confirm "Add $USER to the docker group (Docker without sudo)?"; then
				sudo_ready
				$SUDO usermod -aG docker "$USER"
				ok "added to the docker group"
				next "log out and back in so the docker group applies to new sessions"
			fi
		fi
		if in_group docker && ! active_in_session docker; then DOCKER_VIA_SG=1; fi
		if docker_run info >/dev/null 2>&1; then ok "Docker works"; else warn "Docker is not reachable from this session (no daemon running, or no permission)"; fi
	fi
	if [ ! -e /dev/kvm ]; then
		warn "/dev/kvm is missing: bot computers need hardware virtualization (VT-x/AMD-V, or nested virtualization in a VM)"
	elif [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
		ok "/dev/kvm is usable"
	elif getent group kvm >/dev/null 2>&1 && ! in_group kvm; then
		if confirm "Add $USER to the kvm group (needed to run bot computers)?"; then
			sudo_ready
			$SUDO usermod -aG kvm "$USER"
			ok "added to the kvm group"
			next "log out and back in (or reboot) so the kvm group applies, then: elsebot restart"
		fi
	else
		warn "/dev/kvm is not accessible to $USER"
	fi
}

# --- Rust (scoped to ~/.else/toolchain) -------------------------------------

ensure_rust() {
	step "Rust toolchain ($ELSE_HOME/toolchain)"
	export PATH="$CARGO_HOME/bin:$PATH"
	if [ -x "$CARGO_HOME/bin/rustc" ]; then
		local current
		current="$("$CARGO_HOME/bin/rustc" --version | awk '{print $2}')"
		if ! version_ge "$current" "$MIN_RUST"; then
			info "rustc $current is older than $MIN_RUST; updating"
			"$CARGO_HOME/bin/rustup" update stable --no-self-update >/dev/null
		fi
	else
		info "Installing Rust with rustup (only for building else.bot; your own Rust is not touched)"
		curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs |
			sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path >/dev/null
	fi
	ok "$("$CARGO_HOME/bin/rustc" --version)"
}

# --- Microsandbox (scoped to ~/.else/microsandbox) --------------------------

ensure_msb() {
	step "Microsandbox $MSB_VERSION ($MSB_PREFIX)"
	if [ -x "$MSB" ] && [ "$("$MSB" --version 2>/dev/null | awk '{print $2}')" = "$MSB_VERSION" ]; then
		ok "msb $MSB_VERSION"
		return
	fi
	# The official installer always takes the latest release: install the
	# verified one from the same bundle, checksum checked. msb finds its
	# libkrunfw in ../lib, so the prefix can be anywhere.
	local target="linux-$ARCH" base tmp lib abi
	base="https://github.com/superradcompany/microsandbox/releases/download/v$MSB_VERSION"
	tmp="$(mktemp -d)"
	curl -fsSL "$base/microsandbox-$target.tar.gz" -o "$tmp/bundle.tar.gz"
	curl -fsSL "$base/checksums.sha256" -o "$tmp/checksums.sha256"
	(cd "$tmp" && grep -F " microsandbox-$target.tar.gz" checksums.sha256 | sed "s|microsandbox-$target.tar.gz|bundle.tar.gz|" | sha256sum -c --quiet -) ||
		die "checksum verification of the Microsandbox bundle failed"
	tar -xzf "$tmp/bundle.tar.gz" -C "$tmp"
	lib="$(cd "$tmp" && find . -maxdepth 1 -name 'libkrunfw.so.*.*.*' | sed 's|^\./||' | head -1)"
	[ -n "$lib" ] || die "the Microsandbox bundle has no libkrunfw library"
	abi="${lib#libkrunfw.so.}"
	abi="${abi%%.*}"
	mkdir -p "$MSB_PREFIX/bin" "$MSB_PREFIX/lib"
	install -m 755 "$tmp/msb" "$MSB"
	install -m 644 "$tmp/$lib" "$MSB_PREFIX/lib/$lib"
	ln -sf "$lib" "$MSB_PREFIX/lib/libkrunfw.so.$abi"
	ln -sf "libkrunfw.so.$abi" "$MSB_PREFIX/lib/libkrunfw.so"
	rm -rf "$tmp"
	ok "msb $("$MSB" --version | awk '{print $2}')"
}

# --- Source and versions ----------------------------------------------------

# The release to install: the newest vX.Y.Z tag, or the one asked for.
resolve_release() {
	if [ -n "$WANT_VERSION" ]; then
		WANT_VERSION="${WANT_VERSION#v}"
		return
	fi
	local tags
	tags="$(git ls-remote --tags --refs "$REPO_URL" 'v*')" ||
		die "cannot reach $REPO_URL"
	WANT_VERSION="$(printf '%s\n' "$tags" |
		sed -n 's|.*refs/tags/v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$|\1|p' |
		sort -t. -k1,1n -k2,2n -k3,3n | tail -1)"
	[ -n "$WANT_VERSION" ] || die "no else.bot release found at $REPO_URL (tags vX.Y.Z)"
}

fetch_source() {
	step "Source v$WANT_VERSION ($SRC)"
	mkdir -p "$ELSE_HOME"
	if [ -d "$SRC/.git" ]; then
		git -C "$SRC" remote set-url origin "$REPO_URL"
		git -C "$SRC" fetch -q --force --tags origin
	else
		rm -rf "$SRC"
		git clone -q --no-checkout "$REPO_URL" "$SRC"
	fi
	git -C "$SRC" rev-parse -q --verify "refs/tags/v$WANT_VERSION" >/dev/null ||
		die "release v$WANT_VERSION does not exist at $REPO_URL"
	# A managed checkout: local changes are discarded.
	git -C "$SRC" -c advice.detachedHead=false checkout -q --force "v$WANT_VERSION"
	git -C "$SRC" clean -q -fdx
	ok "checked out v$WANT_VERSION"
}

installed_version() {
	[ -x "$CURRENT/bin/else-api" ] && "$CURRENT/bin/else-api" version 2>/dev/null | awk '{print $2}'
}

source_version() {
	sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$SRC/Cargo.toml" | head -1
}

build_release() {
	local version="$1" dest="$VERSIONS/$1"
	local packages="else-api else-desktop"
	[ "$DISTRO" = macos ] && packages="else-desktop"
	[ "$SERVER_ONLY" = 1 ] && packages="else-api"
	local pkg built=1
	for pkg in $packages; do
		if ! [ -x "$dest/bin/$pkg" ] || [ "$("$dest/bin/$pkg" --version 2>/dev/null | awk '{print $2}')" != "$version" ]; then
			built=0
		fi
	done
	step "Build else.bot $version"
	if [ "$built" = 1 ]; then
		ok "already built ($dest)"
		return
	fi
	info "Building ${packages}; the first build takes several minutes"
	local args="" log="$LOGS/build-$version.log"
	for pkg in $packages; do args="$args -p $pkg"; done
	# shellcheck disable=SC2086
	if ! (cd "$SRC" && CARGO_TARGET_DIR="$BUILD" "$CARGO_HOME/bin/cargo" build --release --locked $args) >"$log" 2>&1; then
		tail -20 "$log" >&2
		die "the build failed; the full log is $log"
	fi
	mkdir -p "$dest/bin"
	for pkg in $packages; do
		install -m 755 "$BUILD/release/$pkg" "$dest/bin/$pkg"
	done
	ok "built into $dest"
}

# Point `current` at a release (atomically).
switch_to() {
	ln -sfn "versions/$1" "$ELSE_HOME/current.tmp"
	mv -fT "$ELSE_HOME/current.tmp" "$CURRENT" 2>/dev/null || {
		# BSD mv has no -T: replace the link in two steps.
		rm -f "$CURRENT"
		mv -f "$ELSE_HOME/current.tmp" "$CURRENT"
	}
}

# Keep the release in use and the one before it.
prune_versions() {
	local keep_a="$1" keep_b="${2:-}" dir name
	for dir in "$VERSIONS"/*; do
		[ -d "$dir" ] || continue
		name="$(basename "$dir")"
		[ "$name" = "$keep_a" ] || [ "$name" = "$keep_b" ] || rm -rf "$dir"
	done
}

install_commands() {
	mkdir -p "$BIN" "$HOME/.local/bin" "$CONFIG"
	install -m 755 "$SRC/scripts/elsebot" "$BIN/elsebot"
	# How elsebot runs the API here: a systemd user unit, or a process it
	# manages itself where there is no systemd user session.
	local manager=process
	if [ -z "${ELSE_INSTALL_NO_SYSTEMD:-}" ] && systemctl --user show-environment >/dev/null 2>&1; then
		manager=systemd
	fi
	printf 'ELSEBOT_SERVICE=%s\nELSEBOT_MANAGER=%s\nELSEBOT_PSQL=%s\n' "$SERVICE" "$manager" "$PSQL" >"$CONFIG/elsebot.env"
	ln -sfn ../current/bin/else-api "$BIN/else-api"
	[ "$SERVER_ONLY" = 1 ] || ln -sfn ../current/bin/else-desktop "$BIN/else-desktop"
	if [ ! -e "$HOME/.local/bin/elsebot" ] || [ -L "$HOME/.local/bin/elsebot" ]; then
		ln -sfn "$BIN/elsebot" "$HOME/.local/bin/elsebot"
	fi
	case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) next "add ~/.local/bin to your PATH to use the elsebot command" ;; esac
	if [ "$DISTRO" != macos ] && [ "$SERVER_ONLY" = 0 ]; then
		mkdir -p "$HOME/.local/share/applications"
		cat >"$HOME/.local/share/applications/else-bot.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=else.bot
Comment=Your personal AI assistant
Exec=$BIN/elsebot desktop
Terminal=false
Categories=Network;Chat;
EOF
	fi
}

# --- Configuration and services ---------------------------------------------

env_value() { sed -n "s/^$1=//p" "$ENV_FILE" | tail -1; }
config_set() {
	local tmp
	tmp="$(mktemp "$CONFIG/.env.XXXXXX")"
	if grep -q "^$1=" "$ENV_FILE"; then
		awk -v k="$1" -v v="$2" 'index($0, k"=")==1{print k"="v; next} {print}' "$ENV_FILE" >"$tmp"
	else
		cat "$ENV_FILE" >"$tmp"
		printf '%s=%s\n' "$1" "$2" >>"$tmp"
	fi
	chmod 600 "$tmp"
	mv -f "$tmp" "$ENV_FILE"
}

free_port() {
	local port
	for port in 55432 55433 55434 55435 55436; do
		if ! (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then echo "$port"; return; fi
	done
	die "no free local port for PostgreSQL (tried 55432-55436)"
}

write_config() {
	step "Configuration ($ENV_FILE)"
	if [ -f "$ENV_FILE" ]; then
		ok "kept"
		return
	fi
	if [ -e "$DATA/secret-key" ] || [ -e "$DATA/owner-token" ]; then
		die "$DATA holds an installation without its configuration ($ENV_FILE); restore the configuration or move $DATA away"
	fi
	mkdir -p "$CONFIG" "$DATA"
	chmod 700 "$CONFIG" "$DATA"
	local database_url password="" port=""
	if [ -n "${ELSE_DATABASE_URL:-}" ]; then
		database_url="$ELSE_DATABASE_URL"
	else
		password="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
		port="$(free_port)"
		database_url="postgresql://else:$password@127.0.0.1:$port/else"
	fi
	(
		umask 077
		cat >"$ENV_FILE" <<EOF
# else.bot settings, written by the installer. The API reads this file from
# its working directory ($CONFIG); Docker Compose reads it too.
DATABASE_URL=$database_url
ELSE_API_BIND=127.0.0.1:$API_PORT
ELSE_DATA_DIR=$DATA
ELSE_MSB_BIN=$MSB
ELSE_PG_DUMP=$PG_DUMP
ELSE_PG_RESTORE=$PG_RESTORE
EOF
		if [ -z "${ELSE_DATABASE_URL:-}" ]; then
			cat >>"$ENV_FILE" <<EOF

# PostgreSQL (Docker Compose project in this directory)
COMPOSE_PROJECT_NAME=else-bot
POSTGRES_DB=else
POSTGRES_USER=else
POSTGRES_PASSWORD=$password
POSTGRES_PORT=$port
EOF
		fi
	)
	ok "written (mode 0600)"
}

# The bots' project directories; set once (an owner's own choice is kept),
# including for installations made before they existed.
ensure_bots_dir() {
	if ! grep -q '^ELSE_BOTS_DIR=' "$ENV_FILE"; then
		config_set ELSE_BOTS_DIR "$ELSE_HOME/bots"
	fi
	mkdir -p "$ELSE_HOME/bots"
	chmod 700 "$ELSE_HOME/bots"
}

# Provider CLIs found now; refreshed on every run (they move with updates).
refresh_provider_paths() {
	local codex claude
	codex="$(command -v codex 2>/dev/null || true)"
	claude="$(command -v claude 2>/dev/null || true)"
	if [ -n "$codex" ]; then config_set ELSE_CODEX_BIN "$codex"; fi
	if [ -n "$claude" ]; then config_set ELSE_CLAUDE_BIN "$claude"; fi
}

start_database() {
	[ -n "${ELSE_DATABASE_URL:-}" ] && return 0
	step "PostgreSQL (Docker Compose project else-bot)"
	cat >"$CONFIG/compose.yaml" <<'EOF'
# PostgreSQL for this else.bot installation, written by the installer.
services:
  postgres:
    image: postgres:18.6
    restart: unless-stopped
    environment:
      POSTGRES_DB: ${POSTGRES_DB:?}
      POSTGRES_USER: ${POSTGRES_USER:?}
      POSTGRES_PASSWORD: ${POSTGRES_PASSWORD:?}
    ports:
      - "127.0.0.1:${POSTGRES_PORT:?}:5432"
    volumes:
      - postgres_data:/var/lib/postgresql
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -h 127.0.0.1 -U \"$${POSTGRES_USER}\" -d \"$${POSTGRES_DB}\""]
      interval: 5s
      timeout: 5s
      retries: 10
      start_period: 10s
    stop_grace_period: 30s

volumes:
  postgres_data:
EOF
	docker_run compose --project-directory "$CONFIG" up -d --wait >/dev/null 2>&1 ||
		die "PostgreSQL did not start; see: docker compose --project-directory $CONFIG logs"
	ok "running on 127.0.0.1:$(env_value POSTGRES_PORT)"
}

build_guest_image() {
	[ "$SKIP_GUEST" = 1 ] && return 0
	step "Guest VM image $GUEST_TAG"
	local home="$DATA/microsandbox" images=""
	[ -d "$home" ] && images="$(MSB_HOME="$home" "$MSB" image list --format json 2>/dev/null || true)"
	case "$images" in
	*"\"$GUEST_TAG\""*) ok "already loaded" ;;
	*)
		info "Building it and loading it into $home (about 15 minutes the first time)"
		local log="$LOGS/guest-image.log"
		if [ "$DOCKER_VIA_SG" = 0 ]; then
			ELSE_GUEST_TAG="$GUEST_TAG" ELSE_MSB_BIN="$MSB" "$SRC/scripts/build-guest-image.sh" "$DATA" >"$log" 2>&1
		else
			sg docker -c "ELSE_GUEST_TAG=$GUEST_TAG ELSE_MSB_BIN=$(printf '%q' "$MSB") $(printf '%q' "$SRC/scripts/build-guest-image.sh") $(printf '%q' "$DATA")" >"$log" 2>&1
		fi || {
			tail -20 "$log" >&2
			die "the guest image build failed; the full log is $log"
		}
		ok "loaded"
		;;
	esac
	config_set ELSE_VM_IMAGE "$GUEST_TAG"
	config_set ELSE_VM_DESKTOP 1
}

run_setup() {
	step "else-api setup"
	local log="$LOGS/setup.log"
	if (cd "$CONFIG" && "$CURRENT/bin/else-api" setup) >"$log" 2>&1; then
		grep -v ' INFO \| WARN ' "$log" | sed 's/^/    /'
		return 0
	fi
	tail -20 "$log" >&2
	return 1
}

elsebot() { ELSE_HOME="$ELSE_HOME" "$BIN/elsebot" "$@"; }

api_running() { curl -fsS --max-time 3 "http://127.0.0.1:$API_PORT/healthz" >/dev/null 2>&1; }

start_api() {
	step "Start the API"
	elsebot service-install >/dev/null
	elsebot start >/dev/null 2>&1 || true
	local _
	for _ in $(seq 1 60); do
		if curl -fsS --max-time 2 "http://127.0.0.1:$API_PORT/readyz" >/dev/null 2>&1; then
			ok "running on http://127.0.0.1:$API_PORT"
			return 0
		fi
		sleep 1
	done
	return 1
}

# Ask the running API to stop taking work; refuse the update if bots work.
prepare_running_api() {
	local token response status
	token="$(cat "$DATA/owner-token")"
	response="$(printf 'Authorization: Bearer %s\n' "$token" |
		curl -sS --max-time 30 -X POST -H @- -w '\n%{http_code}' \
			"http://127.0.0.1:$API_PORT/v1/maintenance/update" 2>&1)" || response=$'\n000'
	status="${response##*$'\n'}"
	response="${response%$'\n'*}"
	case "$status" in
	200)
		local jobs queued
		jobs="$(printf '%s' "$response" | sed -n 's/.*"background_jobs":\([0-9]*\).*/\1/p')"
		queued="$(printf '%s' "$response" | sed -n 's/.*"queued_turns":\([0-9]*\).*/\1/p')"
		ok "the API stopped taking work"
		if [ "${queued:-0}" -gt 0 ]; then info "$queued queued messages will run after the update"; fi
		if [ "${jobs:-0}" -gt 0 ]; then info "$jobs background jobs keep running in the bot computers"; fi
		;;
	409)
		local message
		message="$(printf '%s' "$response" | sed -n 's/.*"message":"\([^"]*\)".*/\1/p')"
		die "not updating: ${message:-bots are working}"
		;;
	*) die "could not prepare the running API for the update (HTTP $status): $response" ;;
	esac
}

# A backup before a release that changes the database, with the old binary.
backup_if_migrating() {
	local from="$1" to="$2" added
	added="$(git -C "$SRC" diff --name-only --diff-filter=A "v$from" "v$to" -- apps/api/migrations 2>/dev/null || echo unknown)"
	[ -z "$added" ] && return 0
	step "Backup before the database changes"
	local log="$LOGS/backup-before-$to.log"
	if (cd "$CONFIG" && "$CURRENT/bin/else-api" backup) >"$log" 2>&1; then
		ok "$(sed -n 's/^backup set: //p' "$log" | tail -1)"
	else
		tail -10 "$log" >&2
		return 1
	fi
}

# libpq settings (PGHOST… PGPASSWORD) from a postgresql:// URL, so no
# password is ever on a command line. Fails on URLs it does not understand.
pg_env_from_url() {
	local url="$1" rest auth hostport
	case "$url" in postgresql://* | postgres://*) ;; *) return 1 ;; esac
	rest="${url#*://}"
	case "$rest" in *@*) ;; *) return 1 ;; esac
	auth="${rest%@*}"
	rest="${rest##*@}"
	hostport="${rest%%/*}"
	PGUSER="${auth%%:*}"
	PGPASSWORD="${auth#*:}"
	PGHOST="${hostport%%:*}"
	PGPORT="${hostport##*:}"
	[ "$PGPORT" != "$hostport" ] || PGPORT=5432
	PGDATABASE="${rest#*/}"
	PGDATABASE="${PGDATABASE%%\?*}"
	[ -n "$PGUSER" ] && [ -n "$PGHOST" ] && [ -n "$PGDATABASE" ] && [ "$PGDATABASE" != "$rest" ]
}

# A database role that can only read, for inspecting the installation
# (`elsebot db`): it sees every table, current and future, and its sessions
# are read-only. Refreshed on every run; its login is in readonly.env (0600).
setup_readonly_access() {
	step "Read-only database access (elsebot db)"
	if [ -z "$PSQL" ] || [ ! -x "$PSQL" ]; then
		warn "psql is missing; elsebot db is not available"
		return 0
	fi
	local file="$CONFIG/readonly.env" password="" PGUSER PGPASSWORD PGHOST PGPORT PGDATABASE
	if ! pg_env_from_url "$(env_value DATABASE_URL)"; then
		warn "DATABASE_URL is not a postgresql://user:password@host:port/database URL; elsebot db is not available"
		return 0
	fi
	[ -f "$file" ] && password="$(sed -n 's/^PGPASSWORD=//p' "$file")"
	[ -n "$password" ] || password="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
	local log="$LOGS/readonly-access.log"
	if ! PGUSER="$PGUSER" PGPASSWORD="$PGPASSWORD" PGHOST="$PGHOST" PGPORT="$PGPORT" PGDATABASE="$PGDATABASE" \
		ELSE_READONLY_PASSWORD="$password" "$PSQL" -X -q -v ON_ERROR_STOP=1 >"$log" 2>&1 <<'SQL'; then
\getenv password ELSE_READONLY_PASSWORD
SELECT format('CREATE ROLE else_readonly LOGIN PASSWORD %L', :'password')
 WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'else_readonly') \gexec
ALTER ROLE else_readonly WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE PASSWORD :'password';
ALTER ROLE else_readonly SET default_transaction_read_only = on;
SELECT format('GRANT CONNECT ON DATABASE %I TO else_readonly', current_database()) \gexec
GRANT USAGE ON SCHEMA public TO else_readonly;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO else_readonly;
GRANT SELECT ON ALL SEQUENCES IN SCHEMA public TO else_readonly;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO else_readonly;
SQL
		tail -5 "$log" >&2
		warn "could not set up the read-only role; elsebot db is not available (log: $log)"
		return 0
	fi
	(
		umask 077
		printf 'PGHOST=%s\nPGPORT=%s\nPGDATABASE=%s\nPGUSER=else_readonly\nPGPASSWORD=%s\n' \
			"$PGHOST" "$PGPORT" "$PGDATABASE" "$password" >"$file"
	)
	ok "role else_readonly (read-only), login in $file"
}

check_providers() {
	step "Provider CLIs"
	if have codex; then
		ok "codex $(codex --version 2>/dev/null | awk '{print $NF}')"
		codex login status >/dev/null 2>&1 || next "log in to Codex once: codex login"
	else
		warn "Codex CLI not found: install it (https://github.com/openai/codex), run 'codex login', then: elsebot update"
	fi
	if have claude; then
		ok "claude $(claude --version 2>/dev/null | awk '{print $1}')"
	else
		info "Claude Code not found (optional): https://claude.com/claude-code"
	fi
}

# --- Stage 2: install or update from the release's own source ---------------

linux_stage2() {
	local target installed mode
	target="$(source_version)"
	[ "$target" = "$WANT_VERSION" ] || die "the source at v$WANT_VERSION reports version $target"
	installed="$(installed_version || true)"
	if [ ! -f "$ENV_FILE" ]; then
		mode=install
	elif [ "$installed" = "$target" ]; then
		mode=current
	else
		mode=update
	fi
	case "$mode" in
	install) info "Installing else.bot $target into $ELSE_HOME" ;;
	update) info "Updating else.bot ${installed:-?} -> $target" ;;
	current) info "else.bot $target is installed; checking the installation" ;;
	esac

	linux_packages
	linux_access
	ensure_rust
	ensure_msb
	build_release "$target"
	write_config
	ensure_bots_dir
	refresh_provider_paths
	start_database
	build_guest_image

	if [ "$mode" = update ] && [ -n "$installed" ]; then
		step "Switch $installed -> $target"
		local was_running=0
		if api_running; then
			was_running=1
			prepare_running_api
		fi
		elsebot stop >/dev/null 2>&1 || true
		ok "the API stopped"
		if ! backup_if_migrating "$installed" "$target"; then
			[ "$was_running" = 1 ] && elsebot start >/dev/null 2>&1
			die "the backup before the update failed; nothing was changed and $installed runs again"
		fi
		switch_to "$target"
		install_commands
		if ! run_setup || ! start_api; then
			warn "$target did not start; switching back to $installed"
			elsebot stop >/dev/null 2>&1 || true
			switch_to "$installed"
			install_commands
			elsebot start >/dev/null 2>&1 || true
			die "the update to $target failed and $installed runs again (logs: elsebot logs). If the database was changed, restore the backup above."
		fi
		prune_versions "$target" "$installed"
		ok "updated to $target (kept $installed in $VERSIONS)"
		setup_readonly_access
	else
		switch_to "$target"
		install_commands
		run_setup || die "else-api setup failed (log: $LOGS/setup.log)"
		if api_running; then
			ok "the API is running"
		else
			start_api || die "the API did not start; see: elsebot logs"
		fi
		setup_readonly_access
	fi
	check_providers
	[ "$SERVER_ONLY" = 0 ] && next "open the app: elsebot desktop (or else.bot in your launcher)"
	return 0
}

macos_stage2() {
	step "Xcode"
	if ! xcode-select -p >/dev/null 2>&1; then
		xcode-select --install >/dev/null 2>&1 || true
		die "install the Xcode command line tools (a dialog was opened), then run the installer again"
	fi
	# GPUI compiles its Metal shaders at build time, which needs Xcode itself.
	xcrun -sdk macosx -f metal >/dev/null 2>&1 ||
		die "the Metal compiler is missing: install Xcode, run 'sudo xcode-select -s /Applications/Xcode.app' and 'xcodebuild -downloadComponent MetalToolchain', then run the installer again"
	ok "Xcode with the Metal compiler"
	SERVER_ONLY=0
	ensure_rust
	local target
	target="$(source_version)"
	build_release "$target"
	switch_to "$target"
	install_commands
	mkdir -p "$DATA" "$CONFIG"
	chmod 700 "$DATA" "$CONFIG"
	next "copy the owner token from the Linux machine running the API: scp SERVER:.else/data/owner-token $DATA/owner-token && chmod 600 $DATA/owner-token"
	next "open a tunnel to it (keep it running): ssh -N -L $API_PORT:127.0.0.1:$API_PORT SERVER"
	next "start the desktop: elsebot desktop"
}

summary() {
	step "Done"
	if [ -n "$WARNINGS" ]; then printf '%sWarnings:%s\n%s' "$YELLOW" "$RESET" "$WARNINGS"; fi
	if [ -n "$NEXT" ]; then printf '%sNext:%s\n%s' "$BOLD" "$RESET" "$NEXT"; fi
}

# --- Main -------------------------------------------------------------------

main() {
	while [ $# -gt 0 ]; do
		case "$1" in
		-y | --yes) ASSUME_YES=1 ;;
		--version) WANT_VERSION="${2:?--version needs a value}"; shift ;;
		--server-only) SERVER_ONLY=1 ;;
		--skip-guest-image) SKIP_GUEST=1 ;;
		-h | --help) usage; exit 0 ;;
		*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
		esac
		shift
	done
	detect_platform

	# Stage 1: fetch the release, then run its own installer, so an update
	# always runs the new release's logic.
	if [ "${ELSE_INSTALL_STAGE:-1}" != 2 ]; then
		printf '%selse.bot installer%s\n' "$BOLD" "$RESET"
		info "System: $DISTRO_NAME ($ARCH)"
		ensure_bootstrap_tools
		resolve_release
		fetch_source
		export ELSE_INSTALL_STAGE=2 ELSE_VERSION="$WANT_VERSION"
		exec bash "$SRC/install.sh" "${ARGS[@]}"
	fi

	mkdir -p "$LOGS"
	if [ "$DISTRO" = macos ]; then macos_stage2; else linux_stage2; fi
	summary
}

# Everything above is definitions: a truncated download runs nothing.
main "$@"
