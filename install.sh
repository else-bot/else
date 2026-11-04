#!/usr/bin/env bash
# else.bot installer and updater.
#
#   curl -fsSL https://raw.githubusercontent.com/else-bot/else/main/install.sh | bash
#
# Installs the latest release into ~/.else, or updates an existing
# installation to it; running it again is how you update (`elsebot update`
# does the same). Linux (Debian 12+, Ubuntu 24.04+, Arch Linux / Omarchy;
# x86_64) and macOS (Apple silicon) get the API, the desktop, and everything
# they need. macOS needs Xcode, and a running Docker (OrbStack or Docker Desktop)
# only when building the optional guest image. SQLite is bundled in the API.
#
# On a fresh machine it asks what this machine runs (--both, --server-only,
# --app-only; Enter or --yes: both); an update keeps what is installed
# without asking: the server (the API and everything bots need), the app (the
# desktop, which connects to a server here or elsewhere), or both. Adding a
# component keeps what is installed: choosing the app on a server installs
# both. An app-only installation has no server settings, data, or service.
#
# Everything it installs or keeps lives in ~/.else:
#   src/        the release's source (a git checkout at the release tag)
#   versions/   built releases, versions/<version>/bin/{else-api,else-desktop}
#   current     -> versions/<version>, the release in use
#   bin/        elsebot, and else-api / else-desktop through current
#   config/     .env (settings, 0600)
#   data/       else.sqlite3, credentials, bot computers, artifacts, backups
#   bots/       each bot's project directory (bots/<bot id>), where its provider
#               CLI starts on the host; mounted as ~/work in its computer
#   toolchain/  Rust (rustup and cargo), only for building else.bot
#   microsandbox/  the VM runtime (msb and libkrunfw)
#   build/      the build cache
#   logs/       install and update logs; the API log without systemd
# Outside it: system packages (with sudo, only when missing; Homebrew's
# SQLite CLI for inspection on Linux), the optional guest image
# in Docker, the API's service (Linux: the systemd user unit
# ~/.config/systemd/user/else-api.service, and lingering for your user when
# it starts at boot; macOS: the launchd agent
# ~/Library/LaunchAgents/bot.else.else-api.plist), and the links
# ~/.local/bin/elsebot and ~/.local/share/applications/else-bot.desktop
# (Linux).
#
# The API can start by itself (asked once, remembered; --autostart or
# --no-autostart to choose): at boot on Linux, at login on macOS, where the
# agent needs your session for the Keychain login of Claude Code.
#
# An update builds the new release first, while the old one keeps running;
# then it asks the API to stop taking work and refuses (changing nothing)
# if bots are working. Otherwise it stops the API, backs up first when the
# release changes the database, switches versions, starts the new API, and
# switches back if it does not come up. The previous release is kept.
# Alpha installations with a non-SQLite database URL start with SQLite instead;
# their previous database history is not imported.
#
# Settings (environment): ELSE_VERSION=0.0.2 installs that release instead
# of the latest; ELSE_HOME, ELSE_REPO, ELSE_API_PORT, ELSE_SERVICE_NAME,
# ELSE_DATABASE_URL (override the SQLite file location), and
# ELSE_INSTALL_NO_SYSTEMD=1 (no systemd or launchd: elsebot runs the API as a
# plain process) exist mostly for tests.
set -euo pipefail

usage() {
	cat <<'EOF'
Usage: install.sh [options]      (or: curl -fsSL …/install.sh | bash -s -- [options])

  -y, --yes            do not ask for confirmation (sudo may still ask for a password)
  --version X.Y.Z      install or update to this release instead of the latest
  --both               the server and the app (the default on a fresh machine)
  --server-only        the server only (home hardware or a VPS)
  --app-only           the app only, connecting to a server elsewhere
  --autostart          start the API by itself: at boot (Linux) or at login (macOS)
  --no-autostart       only start it with: elsebot start
  --skip-guest-image   do not build the guest VM image (bot computers then use a plain image)
  -h, --help           show this help
EOF
}

# --- Settings ---------------------------------------------------------------

MIN_RUST="1.94.0"
MSB_VERSION="0.7.13"
GUEST_TAG="else-guest:2"

ELSE_HOME="${ELSE_HOME:-$HOME/.else}"
REPO_URL="${ELSE_REPO:-https://github.com/else-bot/else.git}"
API_PORT="${ELSE_API_PORT:-7878}"
SERVICE="${ELSE_SERVICE_NAME:-else-api}"
WANT_VERSION="${ELSE_VERSION:-}"
ASSUME_YES=0
COMPONENTS=""
COMPONENTS_FLAG=""
SKIP_GUEST=0
AUTOSTART="${ELSE_AUTOSTART:-}"
ARGS=("$@")

SRC="$ELSE_HOME/src"
BUILD="$ELSE_HOME/build"
LINK_PREFIX="$HOME/.local"
TOOLCHAIN="$ELSE_HOME/toolchain"
MSB_PREFIX="$ELSE_HOME/microsandbox"
# The native E2E lanes keep immutable prerequisites/build artifacts between
# disposable installations. Normal installations keep their original layout.
TEST_CACHE="${ELSE_INSTALL_TEST_CACHE:-}"
BUILD_PROFILE="${ELSE_INSTALL_TEST_PROFILE:-release}"
if [ -n "$TEST_CACHE" ]; then
	BUILD="$TEST_CACHE/build"
	TOOLCHAIN="$TEST_CACHE/toolchain"
	MSB_PREFIX="$TEST_CACHE/microsandbox"
	LINK_PREFIX="$ELSE_HOME/links"
fi
VERSIONS="$ELSE_HOME/versions"
CURRENT="$ELSE_HOME/current"
BIN="$ELSE_HOME/bin"
CONFIG="$ELSE_HOME/config"
DATA="$ELSE_HOME/data"
LOGS="$ELSE_HOME/logs"
ENV_FILE="$CONFIG/.env"
COMPONENTS_FILE="$CONFIG/components"
export RUSTUP_HOME="$TOOLCHAIN/rustup"
export CARGO_HOME="$TOOLCHAIN/cargo"
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

# A question whose answer is yes with --yes or without a terminal.
ask_yes() {
	[ "$ASSUME_YES" = 1 ] && return 0
	{ true </dev/tty; } 2>/dev/null || return 0
	printf '    %s [Y/n] ' "$1"
	local answer
	read -r answer </dev/tty
	case "$answer" in n | N | no | NO) return 1 ;; *) return 0 ;; esac
}

want_server() { [ "$COMPONENTS" != app ]; }
want_app() { [ "$COMPONENTS" != server ]; }

set_components_flag() {
	[ -z "$COMPONENTS_FLAG" ] || [ "$COMPONENTS_FLAG" = "$1" ] ||
		die "choose one of --both, --server-only, and --app-only"
	COMPONENTS_FLAG="$1"
}

installed_components() {
	local server=0 app=0 remembered
	if [ -f "$COMPONENTS_FILE" ]; then
		remembered="$(cat "$COMPONENTS_FILE")"
		case "$remembered" in
		both) server=1; app=1 ;;
		server) server=1 ;;
		app) app=1 ;;
		*) die "invalid saved install components in $COMPONENTS_FILE" ;;
		esac
	fi
	if [ -f "$ENV_FILE" ] || [ -x "$CURRENT/bin/else-api" ]; then server=1; fi
	[ -x "$CURRENT/bin/else-desktop" ] && app=1
	case "$server$app" in
	11) echo both ;;
	10) echo server ;;
	01) echo app ;;
	esac
}

choose_components() {
	local installed choice answer
	installed="$(installed_components)"
	choice="$COMPONENTS_FLAG"
	if [ -z "$choice" ]; then
		choice="${installed:-both}"
		if [ -z "$installed" ] && [ "$ASSUME_YES" = 0 ] && { true </dev/tty; } 2>/dev/null; then
			step "What this machine runs"
			info "1) both    the server and the app"
			info "2) server  the server only (home hardware or a VPS)"
			info "3) app     the app only, connecting to a server elsewhere"
			while true; do
				printf '    Choose 1-3 [%s]: ' "$choice"
				read -r answer </dev/tty
				case "$answer" in
				"") break ;;
				1 | both) choice=both; break ;;
				2 | server) choice=server; break ;;
				3 | app) choice=app; break ;;
				*) info "answer 1, 2, or 3" ;;
				esac
			done
		fi
	fi
	COMPONENTS="$choice"
	if [ -n "$installed" ] && [ "$installed" != "$choice" ]; then
		COMPONENTS=both
		info "Keeping the installed $installed: installing both"
	fi
}

# Recorded once the installation succeeded.
remember_components() {
	printf '%s\n' "$COMPONENTS" >"$COMPONENTS_FILE.tmp"
	mv -f "$COMPONENTS_FILE.tmp" "$COMPONENTS_FILE"
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
	local list="build-essential pkg-config curl ca-certificates git jq tar gzip sqlite3"
	if want_app; then
		list="$list libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libxcb1-dev libx11-xcb-dev libx11-dev libfontconfig-dev libfreetype-dev libvulkan1 libzstd-dev"
	fi
	echo "$list"
}

pacman_packages() {
	local list="base-devel pkgconf curl ca-certificates git jq tar gzip sqlite"
	if want_app; then
		list="$list libxkbcommon libxkbcommon-x11 wayland libxcb libx11 fontconfig freetype2 vulkan-icd-loader zstd"
	fi
	echo "$list"
}

# Docker is only used to build the optional guest image.
docker_packages() {
    docker_needed || return 0
    have docker && return 0
    case "$DISTRO" in
        debian | ubuntu) echo "docker.io" ;;
        arch) echo "docker" ;;
    esac
}

linux_packages() {
    step "System packages"
    local wanted="" missing
    case "$DISTRO" in
        debian | ubuntu) wanted="$(apt_packages) $(docker_packages)" ;;
        arch) wanted="$(pacman_packages) $(docker_packages)" ;;
    esac
    # shellcheck disable=SC2086
    missing="$(missing_packages $wanted)"
    if [ -z "$missing" ]; then
        ok "all present"
    else
        info "Missing: $missing"
        confirm "Install them with sudo?" || die "cannot continue without the system packages"
        # shellcheck disable=SC2086
        install_packages $missing
        ok "installed"
    fi
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
	if docker_needed && have docker; then
		step "Docker, to build the guest image"
		if has_systemd && ! systemctl is-active --quiet docker 2>/dev/null; then
			sudo_ready
			if $SUDO systemctl start docker >/dev/null 2>&1; then ok "Docker service started"; else warn "could not start the Docker service"; fi
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
	step "KVM"
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
	step "Rust toolchain ($TOOLCHAIN)"
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
	local target="linux-$ARCH" base tmp lib abi sha="sha256sum -c --quiet -"
	if [ "$DISTRO" = macos ]; then
		target="darwin-$ARCH"
		sha="shasum -a 256 -c --quiet -"
	fi
	base="https://github.com/superradcompany/microsandbox/releases/download/v$MSB_VERSION"
	tmp="$(mktemp -d)"
	curl -fsSL "$base/microsandbox-$target.tar.gz" -o "$tmp/bundle.tar.gz"
	curl -fsSL "$base/checksums.sha256" -o "$tmp/checksums.sha256"
	# shellcheck disable=SC2086
	(cd "$tmp" && grep -F " microsandbox-$target.tar.gz" checksums.sha256 | sed "s|microsandbox-$target.tar.gz|bundle.tar.gz|" | $sha) ||
		die "checksum verification of the Microsandbox bundle failed"
	tar -xzf "$tmp/bundle.tar.gz" -C "$tmp"
	mkdir -p "$MSB_PREFIX/bin" "$MSB_PREFIX/lib"
	install -m 755 "$tmp/msb" "$MSB"
	if [ "$DISTRO" = macos ]; then
		# libkrunfw.<abi>.dylib; msb is signed with the Hypervisor entitlement.
		lib="$(cd "$tmp" && find . -maxdepth 1 -name 'libkrunfw.*.dylib' | sed 's|^\./||' | head -1)"
		[ -n "$lib" ] || die "the Microsandbox bundle has no libkrunfw library"
		install -m 644 "$tmp/$lib" "$MSB_PREFIX/lib/$lib"
		ln -sf "$lib" "$MSB_PREFIX/lib/libkrunfw.dylib"
	else
		lib="$(cd "$tmp" && find . -maxdepth 1 -name 'libkrunfw.so.*.*.*' | sed 's|^\./||' | head -1)"
		[ -n "$lib" ] || die "the Microsandbox bundle has no libkrunfw library"
		abi="${lib#libkrunfw.so.}"
		abi="${abi%%.*}"
		install -m 644 "$tmp/$lib" "$MSB_PREFIX/lib/$lib"
		ln -sf "$lib" "$MSB_PREFIX/lib/libkrunfw.so.$abi"
		ln -sf "libkrunfw.so.$abi" "$MSB_PREFIX/lib/libkrunfw.so"
	fi
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

# Both binaries report the one version; an app-only installation has no API.
installed_version() {
	local bin
	for bin in else-api else-desktop; do
		if [ -x "$CURRENT/bin/$bin" ]; then
			"$CURRENT/bin/$bin" --version 2>/dev/null | awk '{print $2}'
			return
		fi
	done
	return 1
}

source_version() {
	sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$SRC/Cargo.toml" | head -1
}

# Builds only the binaries this installation needs and lacks, so adding a
# component to an installed release leaves the other binary in place.
build_release() {
	local version="$1" dest="$VERSIONS/$1" pkg missing=""
	local built_profile output="$BUILD_PROFILE"
	built_profile="$(cat "$dest/.build-profile" 2>/dev/null || echo release)"
	[ "$BUILD_PROFILE" != dev ] || output=debug
	for pkg in $(want_server && echo else-api) $(want_app && echo else-desktop); do
		if [ "$built_profile" != "$BUILD_PROFILE" ] || ! [ -x "$dest/bin/$pkg" ] ||
			[ "$("$dest/bin/$pkg" --version 2>/dev/null | awk '{print $2}')" != "$version" ]; then
			missing="$missing $pkg"
		fi
	done
	step "Build else.bot $version"
	if [ -z "$missing" ]; then
		ok "already built ($dest)"
		return
	fi
	info "Building${missing} ($BUILD_PROFILE); the first build takes several minutes"
	local args="" log="$LOGS/build-$version.log"
	for pkg in $missing; do args="$args -p $pkg"; done
	# shellcheck disable=SC2086
	if ! (cd "$SRC" && CARGO_TARGET_DIR="$BUILD" "$CARGO_HOME/bin/cargo" build --profile "$BUILD_PROFILE" --locked $args) >"$log" 2>&1; then
		tail -20 "$log" >&2
		die "the build failed; the full log is $log"
	fi
	mkdir -p "$dest/bin"
	# Renamed into place: the API may be running from this release.
	for pkg in $missing; do
		install -m 755 "$BUILD/$output/$pkg" "$dest/bin/.$pkg.new"
		mv -f "$dest/bin/.$pkg.new" "$dest/bin/$pkg"
	done
	printf '%s\n' "$BUILD_PROFILE" >"$dest/.build-profile"
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
	mkdir -p "$BIN" "$LINK_PREFIX/bin" "$CONFIG"
	chmod 700 "$CONFIG"
	install -m 755 "$SRC/scripts/elsebot" "$BIN/elsebot"
	# How elsebot runs the API here: a systemd user unit (Linux), a launchd
	# agent (macOS), or a process it manages itself where there is neither.
	local manager=process previous="" previous_autostart=""
	if [ -z "${ELSE_INSTALL_NO_SYSTEMD:-}" ]; then
		if [ "$DISTRO" = macos ]; then
			manager=launchd
		elif systemctl --user show-environment >/dev/null 2>&1; then
			manager=systemd
		fi
	fi
	if [ -f "$CONFIG/elsebot.env" ]; then
		previous="$(sed -n 's/^ELSEBOT_MANAGER=//p' "$CONFIG/elsebot.env")"
		previous_autostart="$(sed -n 's/^ELSEBOT_AUTOSTART=//p' "$CONFIG/elsebot.env")"
	fi
	# An API run by another manager (a process before the launchd agent) is
	# stopped by the elsebot that started it; this run starts it again.
	if [ -n "$previous" ] && [ "$previous" != "$manager" ] && [ -x "$BIN/elsebot" ]; then
		elsebot stop >/dev/null 2>&1 || true
	fi
	# Asked once with the server; the answer is kept for updates.
	if [ -z "$AUTOSTART" ]; then AUTOSTART="$previous_autostart"; fi
	if [ -z "$AUTOSTART" ] && want_server; then
		AUTOSTART=0
		if [ "$manager" != process ]; then
			local when="at boot"
			[ "$manager" = launchd ] && when="when you log in"
			if ask_yes "Start the API by itself $when?"; then AUTOSTART=1; fi
		fi
	fi
	printf 'ELSEBOT_SERVICE=%s\nELSEBOT_MANAGER=%s\nELSEBOT_AUTOSTART=%s\n' \
		"$SERVICE" "$manager" "$AUTOSTART" >"$CONFIG/elsebot.env"
	if want_server; then ln -sfn ../current/bin/else-api "$BIN/else-api"; fi
	if want_app; then ln -sfn ../current/bin/else-desktop "$BIN/else-desktop"; fi
	if [ ! -e "$LINK_PREFIX/bin/elsebot" ] || [ -L "$LINK_PREFIX/bin/elsebot" ]; then
		ln -sfn "$BIN/elsebot" "$LINK_PREFIX/bin/elsebot"
	fi
	case ":$PATH:" in *":$LINK_PREFIX/bin:"*) ;; *) next "add $LINK_PREFIX/bin to your PATH to use the elsebot command" ;; esac
	if [ "$DISTRO" != macos ] && want_app; then
		mkdir -p "$LINK_PREFIX/share/applications"
		cat >"$LINK_PREFIX/share/applications/else-bot.desktop" <<EOF
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
    local database_url="${ELSE_DATABASE_URL:-sqlite://$DATA/else.sqlite3}"
    case "$database_url" in sqlite:*) ;; *) die "ELSE_DATABASE_URL must be a SQLite URL" ;; esac
	(
		umask 077
		cat >"$ENV_FILE" <<EOF
# else.bot settings, written by the installer. The API reads this file from
# its working directory ($CONFIG). SQLite needs no server.
DATABASE_URL=$database_url
ELSE_API_BIND=127.0.0.1:$API_PORT
ELSE_DATA_DIR=$DATA
ELSE_MSB_BIN=$MSB
EOF

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

GUEST_IMAGE_STATE=missing
decide_guest_image() {
	if [ "$DISTRO" = macos ] && [ "$ARCH" != aarch64 ]; then
		warn "bot computers need Apple silicon (Microsandbox has no Intel macOS build); they will report unavailable"
		SKIP_GUEST=1
		NO_MSB=1
	fi
	if [ "$SKIP_GUEST" = 1 ]; then
		GUEST_IMAGE_STATE=skip
		return 0
	fi
	GUEST_IMAGE_STATE=missing
	local home="$DATA/microsandbox" images
	[ -d "$home" ] || return 0
	[ -x "$MSB" ] || ensure_msb
	if ! images="$(MSB_HOME="$home" "$MSB" image list --format json)"; then
		ensure_msb
		images="$(MSB_HOME="$home" "$MSB" image list --format json)" ||
			die "could not list the guest images in $home with $MSB"
	fi
	case "$images" in *"\"$GUEST_TAG\""*) GUEST_IMAGE_STATE=cached ;; esac
}
docker_needed() { [ "$GUEST_IMAGE_STATE" = missing ]; }

build_guest_image() {
	step "Guest VM image $GUEST_TAG"
	case "$GUEST_IMAGE_STATE" in
	skip)
		ok "not needed (no guest image)"
		return 0
		;;
	cached) ok "already loaded" ;;
	missing)
		info "Building it and loading it into $DATA/microsandbox (about 15 minutes the first time)"
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

# The API's service, and whether it starts by itself. Without systemd or
# launchd it only starts with `elsebot start`.
setup_service() {
	step "API service"
	local manager
	manager="$(sed -n 's/^ELSEBOT_MANAGER=//p' "$CONFIG/elsebot.env")"
	elsebot service-install >/dev/null || die "could not install the API's $manager service"
	case "$manager" in
	process)
		[ "$AUTOSTART" = 1 ] && warn "the API cannot start by itself here (no systemd user session); start it with: elsebot start"
		ok "a process managed by elsebot"
		return 0
		;;
	launchd)
		if [ "$AUTOSTART" = 1 ]; then
			ok "launchd agent, starts when you log in"
		else
			ok "launchd agent, starts with: elsebot start (--autostart to start it at login)"
		fi
		return 0
		;;
	esac
	if [ "$AUTOSTART" != 1 ]; then
		ok "systemd user unit, starts with: elsebot start (--autostart to start it at boot)"
		return 0
	fi
	# A user's units start at boot, before anyone logs in, only with lingering.
	if [ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null || true)" != yes ]; then
		if ! loginctl enable-linger "$USER" >/dev/null 2>&1; then
			sudo_ready
			$SUDO loginctl enable-linger "$USER" >/dev/null 2>&1 || true
		fi
	fi
	if [ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null || true)" = yes ]; then
		ok "systemd user unit, starts at boot (lingering on for $USER)"
	else
		warn "could not turn on lingering for $USER (loginctl enable-linger): the API starts at your login, not at boot"
	fi
}

api_running() { curl -fsS --max-time 3 "http://127.0.0.1:$API_PORT/healthz" >/dev/null 2>&1; }

start_api() {
	step "Start the API"
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

macos_prerequisites() {
	step "Xcode"
	if ! xcode-select -p >/dev/null 2>&1; then
		xcode-select --install >/dev/null 2>&1 || true
		die "install the Xcode command line tools (a dialog was opened), then run the installer again"
	fi
	if want_app; then
		# GPUI compiles its Metal shaders at build time, which needs Xcode itself.
		xcrun -sdk macosx -f metal >/dev/null 2>&1 ||
			die "the Metal compiler is missing: install Xcode, run 'sudo xcode-select -s /Applications/Xcode.app' and 'xcodebuild -downloadComponent MetalToolchain', then run the installer again"
		ok "Xcode with the Metal compiler"
	else
		ok "command line tools"
	fi
	docker_needed || return 0
	step "Docker, to build the guest image"
	if ! have docker; then
		die "Docker is missing: install OrbStack (https://orbstack.dev) or Docker Desktop, start it, then run the installer again"
	elif ! docker info >/dev/null 2>&1; then
		die "Docker is not running: start OrbStack or Docker Desktop, then run the installer again"
	else
		ok "Docker works ($(docker info --format '{{.OperatingSystem}}' 2>/dev/null || echo running))"
	fi
}

NO_MSB=0
stage2() {
	local target installed mode database_url=""
	target="$(source_version)"
	[ "$target" = "$WANT_VERSION" ] || die "the source at v$WANT_VERSION reports version $target"
	choose_components
	installed="$(installed_version || true)"
	if want_server; then
		[ ! -f "$ENV_FILE" ] || database_url="$(env_value DATABASE_URL)"
		if [ ! -f "$ENV_FILE" ]; then
			mode=install
		elif [ -n "$database_url" ] && [[ "$database_url" != sqlite:* ]]; then
			mode=reset
		elif [ "$installed" = "$target" ]; then
			mode=current
		else
			mode=update
		fi
	elif [ -z "$installed" ]; then
		mode=install
	elif [ "$installed" = "$target" ]; then
		mode=current
	else
		mode=update
	fi
	case "$mode" in
	install) info "Installing else.bot $target ($COMPONENTS) into $ELSE_HOME" ;;
	reset) info "Setting up SQLite for else.bot $target; previous database history is not imported" ;;
	update) info "Updating else.bot ${installed:-?} -> $target ($COMPONENTS)" ;;
	current) info "else.bot $target is installed; checking the installation ($COMPONENTS)" ;;
	esac

	if want_server; then decide_guest_image; else GUEST_IMAGE_STATE=skip; fi
	if [ "$DISTRO" = macos ]; then
		macos_prerequisites
	else
		linux_packages
		if want_server; then linux_access; fi
	fi
	ensure_rust
	if want_server && [ "$NO_MSB" != 1 ]; then ensure_msb; fi
	build_release "$target"
	if want_server; then
		install_server "$mode" "$installed" "$target"
	else
		step "Switch to $target"
		switch_to "$target"
		install_commands
		if [ -n "$installed" ] && [ "$installed" != "$target" ]; then prune_versions "$target" "$installed"; fi
		ok "the app is installed; no server runs on this machine"
	fi
	if want_app; then
		if [ "$DISTRO" = macos ]; then
			next "open the app: elsebot desktop"
		else
			next "open the app: elsebot desktop (or else.bot in your launcher)"
		fi
	fi
	if ! want_server; then
		next "on first launch, enter your server's address and owner token (on the server: cat ~/.else/data/owner-token)"
	fi
	remember_components
	return 0
}

# The API, its settings, data, service, and computers; then the switch.
install_server() {
	local mode="$1" installed="$2" target="$3" database_url
	if [ "$mode" = reset ]; then
		database_url="${ELSE_DATABASE_URL:-sqlite://$DATA/else.sqlite3}"
		case "$database_url" in sqlite:*) ;; *) die "ELSE_DATABASE_URL must be a SQLite URL" ;; esac
		step "Set up SQLite"
		elsebot stop || die "could not stop the API before setting up SQLite"
		# Switch first so a retry cannot run the old binary against SQLite.
		switch_to "$target"
		config_set DATABASE_URL "$database_url"
	fi
	write_config
	ensure_bots_dir
	refresh_provider_paths
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
		setup_service
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
	else
		switch_to "$target"
		install_commands
		setup_service
		run_setup || die "else-api setup failed (log: $LOGS/setup.log)"
		if api_running; then
			ok "the API is running"
		else
			start_api || die "the API did not start; see: elsebot logs"
		fi
	fi
	check_providers
}

summary() {
	step "Done"
	if [ -n "$WARNINGS" ]; then printf '%sWarnings:%s\n%s' "$YELLOW" "$RESET" "$WARNINGS"; fi
	if [ -n "$NEXT" ]; then printf '%sNext:%s\n%s' "$BOLD" "$RESET" "$NEXT"; fi
}

# --- Main -------------------------------------------------------------------

main() {
	case "$BUILD_PROFILE" in dev | release) ;; *) die "ELSE_INSTALL_TEST_PROFILE must be dev or release" ;; esac
	if [ -n "$TEST_CACHE" ]; then
		case "$TEST_CACHE" in /*) ;; *) die "ELSE_INSTALL_TEST_CACHE must be an absolute path" ;; esac
	elif [ "$BUILD_PROFILE" != release ]; then
		die "ELSE_INSTALL_TEST_PROFILE requires ELSE_INSTALL_TEST_CACHE"
	fi
	while [ $# -gt 0 ]; do
		case "$1" in
		-y | --yes) ASSUME_YES=1 ;;
		--version) WANT_VERSION="${2:?--version needs a value}"; shift ;;
		--both) set_components_flag both ;;
		--server-only) set_components_flag server ;;
		--app-only) set_components_flag app ;;
		--autostart) AUTOSTART=1 ;;
		--no-autostart) AUTOSTART=0 ;;
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
		exec bash "$SRC/install.sh" ${ARGS[@]+"${ARGS[@]}"}
	fi

	mkdir -p "$LOGS"
	stage2
	summary
}

# Everything above is definitions: a truncated download runs nothing.
main "$@"
