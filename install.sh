#!/usr/bin/env bash
set -euo pipefail

usage() {
	cat <<'EOF'
Usage: install.sh [options]      (or: curl -fsSL …/install.sh | bash -s -- [options])

  -y, --yes            do not ask for confirmation (sudo may still ask for a password)
  --version X.Y.Z      install or update to this release instead of the latest
                       (dev-COMMIT builds that commit of main)
  --channel CHANNEL    stable: tagged releases (the default)
                       dev: build the newest commit of main here, guest image included;
                       the choice is saved for later updates
  --both               the server and the app (the default on a fresh machine)
  --server-only        the server only (home hardware or a VPS)
  --app-only           the app only, connecting to a server elsewhere
                       component updates use app-current and server-current
  --from-source        build locally instead of downloading release binaries;
                       a release without binaries yet is built locally anyway
  --autostart          start the API by itself: at boot (Linux) or at login (macOS)
  --no-autostart       only start it with: elsebot start
  --skip-guest-image   do not download the guest VM image (bot computers then use a plain image)
  -h, --help           show this help
EOF
}


# Releases before Cargo.toml declared rust-version built with this toolchain.
LEGACY_MIN_RUST="1.94.0"
MSB_VERSION="0.7.13"
GUEST_IMAGE=""

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
FROM_SOURCE=0
CHANNEL_FLAG=""
ARGS=("$@")

SRC="$ELSE_HOME/src"
BUILD="$ELSE_HOME/build"
LINK_PREFIX="$HOME/.local"
TOOLCHAIN="$ELSE_HOME/toolchain"
MSB_PREFIX="$ELSE_HOME/microsandbox"
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
APP_CURRENT="$ELSE_HOME/app-current"
SERVER_CURRENT="$ELSE_HOME/server-current"
BIN="$ELSE_HOME/bin"
CONFIG="$ELSE_HOME/config"
DATA="$ELSE_HOME/data"
LOGS="$ELSE_HOME/logs"
ENV_FILE="$CONFIG/.env"
COMPONENTS_FILE="$CONFIG/components"
CHANNEL_FILE="$CONFIG/update-channel"
APP="$ELSE_HOME/Applications/else.bot.app"
[ "$ELSE_HOME" = "$HOME/.else" ] && APP="$HOME/Applications/else.bot.app"
export RUSTUP_HOME="$TOOLCHAIN/rustup"
export CARGO_HOME="$TOOLCHAIN/cargo"
MSB="$MSB_PREFIX/bin/msb"
USER="${USER:-$(id -un)}"


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
	if [ -f "$ENV_FILE" ] || [ -x "$SERVER_CURRENT/bin/else-api" ] || [ -x "$CURRENT/bin/else-api" ]; then server=1; fi
	{ [ -x "$APP_CURRENT/bin/else-desktop" ] || [ -x "$CURRENT/bin/else-desktop" ]; } && app=1
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
}

remember_components() {
	installed_components >"$COMPONENTS_FILE.tmp"
	mv -f "$COMPONENTS_FILE.tmp" "$COMPONENTS_FILE"
}

version_ge() {
	[ "$(printf '%s\n%s\n' "$2" "$1" | sort -t. -k1,1n -k2,2n -k3,3n | head -1)" = "$2" ]
}

# Installed versions are X.Y.Z releases or dev-COMMIT builds of main; each
# names its directory in $VERSIONS.
dev_build() { [[ "$1" =~ ^dev-[0-9a-f]{40}$ ]]; }
source_ref() { if dev_build "$1"; then echo "${1#dev-}"; else echo "v$1"; fi; }
release_name() { if dev_build "$1"; then echo "main@${1:4:7}"; else echo "v$1"; fi; }

saved_channel() {
	[ -f "$CHANNEL_FILE" ] || { echo stable; return 0; }
	case "$(cat "$CHANNEL_FILE")" in
	stable) echo stable ;;
	dev) echo dev ;;
	*) die "invalid saved update channel in $CHANNEL_FILE" ;;
	esac
}

installed_dev_build() {
	local pointer
	for pointer in "$APP_CURRENT" "$SERVER_CURRENT"; do
		if dev_build "$(basename "$(readlink "$pointer" 2>/dev/null || true)")"; then return 0; fi
	done
	return 1
}

remember_channel() {
	[ -n "$CHANNEL_FLAG" ] || return 0
	mkdir -p "$CONFIG"
	printf '%s\n' "$CHANNEL_FLAG" >"$CHANNEL_FILE.tmp"
	mv -f "$CHANNEL_FILE.tmp" "$CHANNEL_FILE"
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

ensure_bootstrap_tools() {
	if [ "$DISTRO" = macos ]; then
		have git || die "git is required to fetch the release's installation helpers"
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
	local list="curl ca-certificates git jq tar gzip sqlite3"
	if [ "$FROM_SOURCE" = 1 ]; then list="$list build-essential pkg-config"; fi
	if dev_build "$WANT_VERSION" && want_server; then list="$list python3"; fi
	if want_app; then
		if [ "$FROM_SOURCE" = 1 ]; then
			list="$list libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libxcb1-dev libx11-xcb-dev libx11-dev libfontconfig-dev libfreetype-dev libvulkan1 libzstd-dev"
		else
			list="$list libxkbcommon0 libxkbcommon-x11-0 libwayland-client0 libwayland-cursor0 libwayland-egl1 libxcb1 libx11-xcb1 libx11-6 libfontconfig1 libfreetype6 libvulkan1 libzstd1"
		fi
	fi
	echo "$list"
}

pacman_packages() {
	local list="curl ca-certificates git jq tar gzip sqlite"
	if [ "$FROM_SOURCE" = 1 ]; then list="$list base-devel pkgconf"; fi
	if dev_build "$WANT_VERSION" && want_server; then list="$list python"; fi
	if want_app; then
		list="$list libxkbcommon libxkbcommon-x11 wayland libxcb libx11 fontconfig freetype2 vulkan-icd-loader zstd"
	fi
	echo "$list"
}

linux_packages() {
    step "System packages"
    local wanted="" missing
    case "$DISTRO" in
        debian | ubuntu) wanted="$(apt_packages)" ;;
        arch) wanted="$(pacman_packages)" ;;
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

in_group() { case " $(id -nG "$USER" 2>/dev/null || true) " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
has_systemd() { [ -d /run/systemd/system ]; }

linux_access() {
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


ensure_rust() {
	step "Rust toolchain ($TOOLCHAIN)"
	export PATH="$CARGO_HOME/bin:$PATH"
	if [ -x "$CARGO_HOME/bin/rustc" ]; then
		local current minimum
		current="$("$CARGO_HOME/bin/rustc" --version | awk '{print $2}')"
		minimum="$(sed -n 's/^rust-version = "\(.*\)"$/\1/p' "$SRC/Cargo.toml" | head -1)"
		minimum="${minimum:-$LEGACY_MIN_RUST}"
		if ! version_ge "$current" "$minimum"; then
			info "rustc $current is older than $minimum; updating"
			"$CARGO_HOME/bin/rustup" update stable --no-self-update >/dev/null
		fi
	else
		info "Installing Rust with rustup (only for building else.bot; your own Rust is not touched)"
		curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs |
			sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path >/dev/null
	fi
	ok "$("$CARGO_HOME/bin/rustc" --version)"
}


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

releases_url() {
	if [ -n "${ELSE_RELEASES_URL:-}" ]; then
		printf '%s\n' "${ELSE_RELEASES_URL%/}"
		return
	fi
	local repo="${REPO_URL%.git}"
	case "$repo" in
	https://github.com/*) ;;
	git@github.com:*) repo="https://github.com/${repo#git@github.com:}" ;;
	*) die "set ELSE_RELEASES_URL for this repository, or use --from-source" ;;
	esac
	printf '%s/releases\n' "$repo"
}

# Returns 0 when the file arrived, 3 when the server does not have it
# (HTTP 404: not published yet), and 1 for any other failure.
fetch_release_file() {
	local code
	rm -f "$2"
	if ! code="$(curl --retry 3 --connect-timeout 15 -sSL -w '%{http_code}' -o "$2" "$1")"; then
		rm -f "$2"
		return 1
	fi
	case "$code" in
	200 | 000) return 0 ;;
	404) rm -f "$2"; return 3 ;;
	*) rm -f "$2"; return 1 ;;
	esac
}

# Prints the one well-formed checksum SHA256SUMS lists for an archive.
release_checksum() {
	local count hash
	count="$(awk -v name="$2" '$2 == name {n++} END {print n + 0}' "$1")"
	[ "$count" != 0 ] || die "SHA256SUMS has no checksum for $2"
	[ "$count" = 1 ] || die "SHA256SUMS lists $2 more than once"
	hash="$(awk -v name="$2" '$2 == name {print $1}' "$1")"
	[[ "$hash" =~ ^[0-9a-f]{64}$ ]] || die "invalid checksum for $2"
	printf '%s\n' "$hash"
}

release_platform() {
	case "$DISTRO/$ARCH" in
	macos/aarch64) echo macos-arm64 ;;
	macos/*) die "prebuilt macOS releases require Apple silicon" ;;
	*/x86_64) echo linux-amd64 ;;
	*/aarch64) echo linux-arm64 ;;
	*) die "no release binaries for $DISTRO/$ARCH" ;;
	esac
}

# The latest release is the newest stable vX.Y.Z tag, also before its
# binaries are published: the installer then builds it from source. On the
# dev channel it is the newest commit of main, always built here.
resolve_release() {
	local channel="$CHANNEL_FLAG" head
	[ -n "$channel" ] || channel="$(saved_channel)"
	if [ -n "$WANT_VERSION" ]; then
		WANT_VERSION="${WANT_VERSION#v}"
	elif [ "$channel" = dev ]; then
		head="$(git ls-remote "$REPO_URL" refs/heads/main)" || die "cannot reach $REPO_URL"
		WANT_VERSION="dev-$(printf '%s\n' "$head" | awk '$2 == "refs/heads/main" {print $1}')"
		dev_build "$WANT_VERSION" || die "$REPO_URL has no main branch"
	else
		local tags
		tags="$(git ls-remote --tags --refs "$REPO_URL" 'v*')" ||
			die "cannot reach $REPO_URL"
		WANT_VERSION="$(printf '%s\n' "$tags" |
			sed -n 's|.*refs/tags/v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$|\1|p' |
			sort -t. -k1,1n -k2,2n -k3,3n | tail -1)"
	fi
	[[ "$WANT_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || dev_build "$WANT_VERSION" ||
		die "cannot resolve a release version (expected X.Y.Z or dev-COMMIT)"
	if dev_build "$WANT_VERSION"; then FROM_SOURCE=1; fi
}

PREVIOUS_SOURCE=""
fetch_source() {
	local ref
	ref="$(source_ref "$WANT_VERSION")"
	PREVIOUS_SOURCE="$(git -C "$SRC" rev-parse -q --verify HEAD 2>/dev/null || true)"
	step "Source $(release_name "$WANT_VERSION") ($SRC)"
	mkdir -p "$ELSE_HOME"
	if [ -d "$SRC/.git" ]; then
		git -C "$SRC" remote set-url origin "$REPO_URL"
		git -C "$SRC" fetch -q --force --tags origin
	else
		rm -rf "$SRC"
		git clone -q --no-checkout "$REPO_URL" "$SRC"
	fi
	if dev_build "$WANT_VERSION"; then
		git -C "$SRC" cat-file -e "$ref^{commit}" 2>/dev/null || git -C "$SRC" fetch -q origin "$ref" ||
			die "commit $ref does not exist at $REPO_URL"
	else
		git -C "$SRC" rev-parse -q --verify "refs/tags/$ref" >/dev/null ||
			die "release $ref does not exist at $REPO_URL"
	fi
	git -C "$SRC" -c advice.detachedHead=false checkout -q --force "$ref"
	git -C "$SRC" clean -q -fdx
	ok "checked out $(release_name "$WANT_VERSION")"
}

installed_version() {
	local pointer="$APP_CURRENT" bin=else-desktop directory
	if want_server; then pointer="$SERVER_CURRENT"; bin=else-api; fi
	[ -x "$pointer/bin/$bin" ] || return 1
	directory="$(basename "$(readlink "$pointer")")"
	if dev_build "$directory"; then echo "$directory"; return 0; fi
	"$pointer/bin/$bin" --version 2>/dev/null | awk '{print $2}'
}

source_version() {
	sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$SRC/Cargo.toml" | head -1
}

# Downloads verified binaries into $output. When the release does not
# publish SHA256SUMS or a selected archive (yet), it sets RELEASE_MISSING and
# leaves none of the binaries in $output. Published checksums and archives
# that fail validation are fatal.
RELEASE_MISSING=""
download_release() {
	local version="$1" tmp="$2" output="$3" platform base pkg archive hash member actual status=0 i=0
	local hashes=()
	shift 3
	RELEASE_MISSING=""
	platform="$(release_platform)"
	base="$(releases_url)/download/v$version"
	mkdir -p "$tmp"
	fetch_release_file "$base/SHA256SUMS" "$tmp/SHA256SUMS" || status=$?
	case "$status" in
	0) ;;
	3)
		RELEASE_MISSING="v$version publishes no binaries yet"
		return 0
		;;
	*) die "cannot download checksums for v$version" ;;
	esac
	for pkg in "$@"; do
		hash="$(release_checksum "$tmp/SHA256SUMS" "$pkg-v$version-$platform.tar.gz")"
		hashes+=("$hash")
	done
	for pkg in "$@"; do
		archive="$pkg-v$version-$platform.tar.gz"
		info "Downloading $archive"
		status=0
		fetch_release_file "$base/$archive" "$tmp/$archive" || status=$?
		case "$status" in
		0) ;;
		3)
			RELEASE_MISSING="v$version publishes no $archive yet"
			for archive in "$@"; do rm -f "$output/$archive"; done
			return 0
			;;
		*) die "cannot download $archive" ;;
		esac
		if [ "$DISTRO" = macos ]; then
			(cd "$tmp" && printf '%s  %s\n' "${hashes[i]}" "$archive" | shasum -a 256 -c -) || die "checksum verification failed for $archive"
		else
			(cd "$tmp" && printf '%s  %s\n' "${hashes[i]}" "$archive" | sha256sum -c -) || die "checksum verification failed for $archive"
		fi
		member="$pkg"
		if [ "$DISTRO" = macos ] && [ "$pkg" = else-desktop ]; then
			member="else.bot.app/Contents/MacOS/else-desktop"
		fi
		tar -xOzf "$tmp/$archive" "$member" >"$output/$pkg" || die "the archive has no $member"
		chmod 755 "$output/$pkg"
		actual="$("$output/$pkg" --version)" || die "cannot run $pkg on this host"
		[ "$actual" = "$pkg $version" ] || die "$pkg reports '$actual', expected '$pkg $version'"
		i=$((i + 1))
	done
}

# Older tagged installers need --from-source before their prerequisites run.
# Probe for unpublished assets without downloading the archives twice.
release_unpublished() {
	local version="$1" components="$2" platform base tmp pkg archive code packages status=0
	RELEASE_MISSING=""
	platform="$(release_platform)"
	base="$(releases_url)/download/v$version"
	tmp="$(mktemp -d)"
	fetch_release_file "$base/SHA256SUMS" "$tmp/SHA256SUMS" || status=$?
	if [ "$status" = 3 ]; then
		RELEASE_MISSING="v$version publishes no binaries yet"
	elif [ "$status" = 0 ]; then
		packages="$([ "$components" = app ] || echo else-api) $([ "$components" = server ] || echo else-desktop)"
		for pkg in $packages; do
			if ! (release_checksum "$tmp/SHA256SUMS" "$pkg-v$version-$platform.tar.gz" >/dev/null); then
				rm -rf "$tmp"
				exit 1
			fi
		done
		for pkg in $packages; do
			archive="$pkg-v$version-$platform.tar.gz"
			code="$(curl --retry 3 --connect-timeout 15 -sSIL -o /dev/null -w '%{http_code}' "$base/$archive" || true)"
			if [ "$code" = 404 ]; then
				RELEASE_MISSING="v$version publishes no $archive yet"
				break
			fi
		done
	fi
	rm -rf "$tmp"
}

build_release() {
	local version="$1" destination="$2" pkg args="" output="$BUILD_PROFILE" log
	shift 2
	log="$LOGS/build-$version.log"
	mkdir -p "$LOGS"
	[ "$BUILD_PROFILE" != dev ] || output=debug
	ensure_rust
	info "Building $* ($BUILD_PROFILE); the first build takes several minutes"
	for pkg in "$@"; do args="$args -p $pkg"; done
	# Cargo can inherit wrappers such as sccache from ancestor config files.
	# shellcheck disable=SC2086
	if ! (cd "$SRC" && CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$BUILD" "$CARGO_HOME/bin/cargo" build --profile "$BUILD_PROFILE" --locked $args) >"$log" 2>&1; then
		tail -20 "$log" >&2
		die "the build failed; the full log is $log"
	fi
	for pkg in "$@"; do install -m 755 "$BUILD/$output/$pkg" "$destination/$pkg"; done
}

install_release() (
	migrate_pointers
	local version="$1" dest="$VERSIONS/$1" pkg missing=""
	local built_profile stage payload
	built_profile="$(cat "$dest/.build-profile" 2>/dev/null || echo release)"
	for pkg in $(want_server && echo else-api) $(want_app && echo else-desktop); do
		# A dev build reports the release it follows; its directory names its commit.
		if [ "$built_profile" != "$BUILD_PROFILE" ] || ! [ -x "$dest/bin/$pkg" ] ||
			{ ! dev_build "$version" && [ "$("$dest/bin/$pkg" --version 2>/dev/null | awk '{print $2}')" != "$version" ]; }; then
			missing="$missing $pkg"
		fi
	done
	step "Install else.bot $version"
	if [ -z "$missing" ]; then
		ok "already installed ($dest)"
		return
	fi
	mkdir -p "$VERSIONS"
	stage="$(mktemp -d "$VERSIONS/.install-$version.XXXXXX")"
	trap 'rm -rf "$stage"' EXIT
	payload="$stage/version"
	mkdir -p "$payload/bin"
	if [ "$FROM_SOURCE" = 1 ]; then
		# shellcheck disable=SC2086
		build_release "$version" "$payload/bin" $missing
	else
		# shellcheck disable=SC2086
		download_release "$version" "$stage/downloads" "$payload/bin" $missing
		if [ -n "$RELEASE_MISSING" ]; then
			info "$RELEASE_MISSING; building$missing from source"
			rm -rf "$stage/downloads"
			FROM_SOURCE=1
			if [ "$DISTRO" = macos ]; then macos_prerequisites; else linux_packages; fi
			# shellcheck disable=SC2086
			build_release "$version" "$payload/bin" $missing
		fi
	fi
	printf '%s\n' "$BUILD_PROFILE" >"$payload/.build-profile"
	if [ -d "$dest" ]; then
		mkdir -p "$dest/bin"
		for pkg in $missing; do mv -f "$payload/bin/$pkg" "$dest/bin/$pkg"; done
		mv -f "$payload/.build-profile" "$dest/.build-profile"
	else
		mv "$payload" "$dest"
	fi
	ok "installed into $dest"
)

point_to() {
	local pointer="$1" target="$2"
	ln -sfn "$target" "$pointer.tmp"
	mv -fT "$pointer.tmp" "$pointer" 2>/dev/null || mv -fh "$pointer.tmp" "$pointer"
}

sync_legacy_pointer() {
	local target
	target="$(readlink "$SERVER_CURRENT" 2>/dev/null || readlink "$APP_CURRENT" 2>/dev/null || true)"
	[ -z "$target" ] || point_to "$CURRENT" "$target"
}

migrate_pointers() {
	local target
	target="$(readlink "$CURRENT" 2>/dev/null || true)"
	[ -n "$target" ] || return 0
	if [ ! -L "$SERVER_CURRENT" ] && [ -x "$CURRENT/bin/else-api" ]; then point_to "$SERVER_CURRENT" "$target"; fi
	if [ ! -L "$APP_CURRENT" ] && [ -x "$CURRENT/bin/else-desktop" ]; then point_to "$APP_CURRENT" "$target"; fi
	if [ "$(readlink "$BIN/else-api" 2>/dev/null || true)" = ../current/bin/else-api ]; then
		ln -sfn ../server-current/bin/else-api "$BIN/else-api"
	fi
	if [ "$(readlink "$BIN/else-desktop" 2>/dev/null || true)" = ../current/bin/else-desktop ]; then
		ln -sfn ../app-current/bin/else-desktop "$BIN/else-desktop"
	fi
}

activate_component() {
	local pointer="$1" target="versions/$2" previous
	previous="$(readlink "$pointer" 2>/dev/null || true)"
	if [ -n "$previous" ] && [ "$previous" != "$target" ]; then
		point_to "${pointer%-current}-previous" "$previous"
	fi
	point_to "$pointer" "$target"
}

switch_to() {
	if want_server; then activate_component "$SERVER_CURRENT" "$1"; fi
	if want_app; then activate_component "$APP_CURRENT" "$1"; fi
	sync_legacy_pointer
}

restore_pointers() {
	local pointer target
	for pointer in app-current server-current app-previous server-previous; do
		case "$pointer" in
		app-*) want_app || continue ;;
		server-*) want_server || continue ;;
		esac
		case "$pointer" in
		app-current) target="$OLD_APP" ;;
		server-current) target="$OLD_SERVER" ;;
		app-previous) target="$OLD_APP_PREVIOUS" ;;
		server-previous) target="$OLD_SERVER_PREVIOUS" ;;
		esac
		if [ -n "$target" ]; then point_to "$ELSE_HOME/$pointer" "$target"; else rm -f "$ELSE_HOME/$pointer"; fi
	done
	sync_legacy_pointer
}

prune_versions() {
	local dir pointer referenced
	for dir in "$VERSIONS"/*; do
		[ -d "$dir" ] || continue
		referenced=0
		for pointer in current app-current server-current app-previous server-previous; do
			if [ -L "$ELSE_HOME/$pointer" ] && [ "$dir" -ef "$ELSE_HOME/$pointer" ]; then referenced=1; break; fi
		done
		[ "$referenced" = 1 ] || rm -rf "$dir"
	done
}

lock_installation() {
	local lock="$ELSE_HOME/run/install.lock"
	mkdir -p "$ELSE_HOME/run"
	if [ "${ELSE_INSTALL_LOCK_PID:-}" = "$PPID" ] && [ "$(cat "$lock/pid" 2>/dev/null || true)" = "$PPID" ]; then
		return
	fi
	if [ "${ELSE_INSTALL_LOCK_PID:-}" != "$$" ] || [ "$(cat "$lock/pid" 2>/dev/null || true)" != "$$" ]; then
		mkdir "$lock" 2>/dev/null || die "another installer holds $lock; if it was interrupted, remove this directory after checking that no installer is running"
		printf '%s\n' "$$" >"$lock/pid"
		export ELSE_INSTALL_LOCK_PID="$$"
	fi
	trap 'rm -rf "$ELSE_HOME/run/install.lock"' EXIT
	trap 'exit 130' INT
	trap 'exit 143' TERM
}

install_commands() {
	mkdir -p "$BIN" "$LINK_PREFIX/bin" "$CONFIG"
	chmod 700 "$CONFIG"
	install -m 755 "$SRC/scripts/elsebot" "$BIN/elsebot"
	local manager=process previous="" previous_autostart="" previous_app=""
	if want_server && [ -z "${ELSE_INSTALL_NO_SYSTEMD:-}" ]; then
		if [ "$DISTRO" = macos ]; then
			manager=launchd
		elif systemctl --user show-environment >/dev/null 2>&1; then
			manager=systemd
		fi
	fi
	if [ -f "$CONFIG/elsebot.env" ]; then
		previous="$(sed -n 's/^ELSEBOT_MANAGER=//p' "$CONFIG/elsebot.env")"
		previous_autostart="$(sed -n 's/^ELSEBOT_AUTOSTART=//p' "$CONFIG/elsebot.env")"
		previous_app="$(sed -n 's/^ELSEBOT_APP=//p' "$CONFIG/elsebot.env")"
		if ! want_server; then SERVICE="$(sed -n 's/^ELSEBOT_SERVICE=//p' "$CONFIG/elsebot.env")"; fi
	fi
	if ! want_server && [ -n "$previous" ]; then manager="$previous"; fi
	if want_server && [ -n "$previous" ] && [ "$previous" != "$manager" ] && [ -x "$BIN/elsebot" ]; then
		elsebot stop >/dev/null 2>&1 || true
	fi
	if ! want_server; then AUTOSTART="$previous_autostart"; fi
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
		"$SERVICE" "$manager" "$AUTOSTART" >"$CONFIG/elsebot.env.tmp"
	if [ "$DISTRO" = macos ] && want_app; then
		if [ -d "$HOME/.else" ] && [ "$(cd "$ELSE_HOME" && pwd -P)" = "$(cd "$HOME/.else" && pwd -P)" ]; then
			APP="$HOME/Applications/else.bot.app"
		fi
		publish_app
		printf 'ELSEBOT_APP=%q\n' "$APP" >>"$CONFIG/elsebot.env.tmp"
	elif [ -n "$previous_app" ]; then
		printf 'ELSEBOT_APP=%s\n' "$previous_app" >>"$CONFIG/elsebot.env.tmp"
	fi
	mv -f "$CONFIG/elsebot.env.tmp" "$CONFIG/elsebot.env"
	if want_server; then ln -sfn ../server-current/bin/else-api "$BIN/else-api"; fi
	if want_app; then ln -sfn ../app-current/bin/else-desktop "$BIN/else-desktop"; fi
	if [ ! -e "$LINK_PREFIX/bin/elsebot" ] || [ -L "$LINK_PREFIX/bin/elsebot" ]; then
		ln -sfn "$BIN/elsebot" "$LINK_PREFIX/bin/elsebot"
	fi
	case ":$PATH:" in *":$LINK_PREFIX/bin:"*) ;; *) next "add $LINK_PREFIX/bin to your PATH to use the elsebot command" ;; esac
	if [ "$DISTRO" != macos ] && want_app; then
		mkdir -p "$LINK_PREFIX/share/applications"
		mkdir -p "$LINK_PREFIX/share/icons/hicolor/scalable/apps"
		install -m 644 "$SRC/media/branding/else-bot.svg" "$LINK_PREFIX/share/icons/hicolor/scalable/apps/else-bot.svg"
		cat >"$LINK_PREFIX/share/applications/else-bot.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=else.bot
Icon=else-bot
Comment=Your personal AI assistant
Exec=$BIN/elsebot desktop
Terminal=false
Categories=Network;Chat;
EOF
	fi
}

publish_app() {
	local status=0
	python3 "$SRC/scripts/macos_app.py" --binary "$APP_CURRENT/bin/else-desktop" --app "$APP" \
		--home "$ELSE_HOME" --version "$("$APP_CURRENT/bin/else-desktop" --version | awk '{print $2}')" || status=$?
	case "$status" in
	0) ;;
	3)
		local fallback="$ELSE_HOME/Applications/else.bot.app"
		[ "$APP" != "$fallback" ] || die "$APP belongs to another installation; choose a different ELSE_HOME"
		warn "$APP belongs to another installation; publishing this app at $fallback"
		APP="$fallback"
		publish_app
		;;
	*) die "could not publish the app at $APP" ;;
	esac
}


dotenv_quote() {
	local v="$1"
	case "$v" in
	*$'\n'*) die "a setting cannot span lines" ;;
	*[!A-Za-z0-9_./:@%+,=-]*)
		v="${v//\\/\\\\}"
		v="${v//\"/\\\"}"
		v="${v//\$/\\\$}"
		printf '"%s"' "$v"
		;;
	*) printf '%s' "$v" ;;
	esac
}
dotenv_value() {
	local in="$1" out="" quote="" c i
	for ((i = 0; i < ${#in}; i++)); do
		c="${in:i:1}"
		if [ "$quote" = "'" ]; then
			if [ "$c" = "'" ]; then quote=""; else out+="$c"; fi
		elif [ "$c" = '\' ]; then
			i=$((i + 1))
			c="${in:i:1}"
			if [ "$c" = n ]; then out+=$'\n'; else out+="$c"; fi
		elif [ "$quote" = '"' ]; then
			if [ "$c" = '"' ]; then quote=""; else out+="$c"; fi
		elif [ "$c" = "'" ] || [ "$c" = '"' ]; then
			quote="$c"
		elif [ "$c" = ' ' ] || [ "$c" = $'\t' ]; then
			break
		else
			out+="$c"
		fi
	done
	printf '%s' "$out"
}
env_value() { dotenv_value "$(sed -n "s/^$1=//p" "$ENV_FILE" | tail -1)"; }
config_set() {
	local tmp value
	value="$(dotenv_quote "$2")"
	tmp="$(mktemp "$CONFIG/.env.XXXXXX")"
	if grep -q "^$1=" "$ENV_FILE"; then
		VALUE="$value" awk -v k="$1" 'index($0, k"=")==1{print k"="ENVIRON["VALUE"]; next} {print}' "$ENV_FILE" >"$tmp"
	else
		cat "$ENV_FILE" >"$tmp"
		printf '%s=%s\n' "$1" "$value" >>"$tmp"
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
	local url_value data_value msb_value
	url_value="$(dotenv_quote "$database_url")"
	data_value="$(dotenv_quote "$DATA")"
	msb_value="$(dotenv_quote "$MSB")"
	(
		umask 077
		cat >"$ENV_FILE" <<EOF
# else.bot settings, written by the installer. The API reads this file from
# its working directory ($CONFIG). SQLite needs no server.
DATABASE_URL=$url_value
ELSE_API_BIND=127.0.0.1:$API_PORT
ELSE_DATA_DIR=$data_value
ELSE_MSB_BIN=$msb_value
EOF

	)
	ok "written (mode 0600)"
}

ensure_bots_dir() {
	if ! grep -q '^ELSE_BOTS_DIR=' "$ENV_FILE"; then
		config_set ELSE_BOTS_DIR "$ELSE_HOME/bots"
	fi
	mkdir -p "$ELSE_HOME/bots"
	chmod 700 "$ELSE_HOME/bots"
}

refresh_provider_paths() {
	local codex claude grok
	codex="$(command -v codex 2>/dev/null || true)"
	claude="$(command -v claude 2>/dev/null || true)"
	if [ -n "$codex" ]; then config_set ELSE_CODEX_BIN "$codex"; fi
	if [ -n "$claude" ]; then config_set ELSE_CLAUDE_BIN "$claude"; fi
	grok="$(command -v grok 2>/dev/null || true)"
	if [ -n "$grok" ]; then config_set ELSE_GROK_BIN "$grok"; fi
}

# msb records a released maintenance lease with no holder and an expiry equal
# to its completion time, and blocks every command until that expiry passes.
# When the clock steps backwards after a release (a fast clock at boot that NTP
# corrects), that expiry lies hours ahead and msb fails with "install operation
# in progress" although nothing holds it. Move such released leases to now; the
# guard leaves a lease that a process took meanwhile untouched.
release_stale_msb_leases() {
	local db="$1/db/msb.db" released
	[ -f "$db" ] && have sqlite3 || return 0
	released="$(sqlite3 -cmd '.timeout 5000' "$db" "
		UPDATE maintenance_lease
		SET lease_expires_at = strftime('%Y-%m-%d %H:%M:%f', 'now'),
			last_completed_at = strftime('%Y-%m-%d %H:%M:%f', 'now')
		WHERE holder_pid IS NULL
			AND last_completed_at = lease_expires_at
			AND lease_expires_at > strftime('%Y-%m-%d %H:%M:%f', 'now')
		RETURNING name;" 2>/dev/null)" || return 0
	[ -z "$released" ] || info "Released Microsandbox locks dated in the future by a clock change: ${released//$'\n'/ }"
}

GUEST_IMAGE_STATE=missing
GUEST_BUILD=0
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
	local manifest="$SRC/infra/guest-image.json" schema published="" source
	[ -f "$manifest" ] || die "the release has no guest image manifest: $manifest"
	if [ "$DISTRO" = macos ]; then
		schema="$(plutil -extract schema raw -o - "$manifest")" || die "invalid guest image manifest: $manifest"
		published="$(plutil -extract source_sha256 raw -o - "$manifest" 2>/dev/null || true)"
		GUEST_IMAGE="$(plutil -extract image raw -o - "$manifest" 2>/dev/null || true)"
	else
		schema="$(jq -er '.schema' "$manifest")" || die "invalid guest image manifest: $manifest"
		published="$(jq -r '.source_sha256 | strings' "$manifest")"
		GUEST_IMAGE="$(jq -r '.image | strings' "$manifest")"
	fi
	[ "$schema" = 1 ] || die "unsupported guest image manifest schema: $schema"
	# A dev build whose guest inputs changed since the published image builds
	# its own; without Docker it keeps the published one.
	if dev_build "$WANT_VERSION"; then
		source="$(cd "$SRC" && python3 scripts/guest_release.py hash)" || die "cannot hash the guest image inputs in $SRC"
		if [ "$source" != "$published" ]; then
			# The app starts updates with the minimal PATH of a macOS login item.
			[ "$DISTRO" != macos ] || PATH="$PATH:/usr/local/bin:/opt/homebrew/bin:$HOME/.orbstack/bin"
			if have docker && docker info >/dev/null 2>&1; then
				GUEST_IMAGE="else-guest:dev-${source:0:12}"
				GUEST_BUILD=1
			else
				warn "the guest image changed on main, but Docker is not running to build it; bot computers keep the published image"
			fi
		fi
	fi
	if [ "$GUEST_BUILD" = 0 ]; then
		[ -n "$GUEST_IMAGE" ] || die "the release has no published guest image; use --skip-guest-image to install without bot desktops"
		[[ "$GUEST_IMAGE" =~ ^ghcr\.io/else-bot/else-guest@sha256:[0-9a-f]{64}$ ]] ||
			die "the guest image must be pinned to a GHCR digest in $manifest"
	fi
	GUEST_IMAGE_STATE=missing
	local home="$DATA/microsandbox" images
	[ -d "$home" ] || return 0
	[ -x "$MSB" ] || ensure_msb
	release_stale_msb_leases "$home"
	if ! images="$(MSB_HOME="$home" "$MSB" image list --format json)"; then
		ensure_msb
		images="$(MSB_HOME="$home" "$MSB" image list --format json)" ||
			die "could not list the guest images in $home with $MSB"
	fi
	case "$images" in *"\"$GUEST_IMAGE\""* | *"/$GUEST_IMAGE\""*) GUEST_IMAGE_STATE=cached ;; esac
}

prepare_guest_image() {
	step "Guest VM image"
	case "$GUEST_IMAGE_STATE" in
	skip)
		ok "not needed (no guest image)"
		return 0
		;;
	cached) ok "already in $DATA/microsandbox ($GUEST_IMAGE)" ;;
	missing)
		local log="$LOGS/guest-image.log"
		mkdir -p "$DATA/microsandbox"
		if [ "$GUEST_BUILD" = 1 ]; then
			info "Building $GUEST_IMAGE with Docker into $DATA/microsandbox"
			ELSE_GUEST_LOAD_TAG="$GUEST_IMAGE" ELSE_MSB_BIN="$MSB" bash "$SRC/scripts/build-guest-image.sh" "$DATA" >"$log" 2>&1 || {
				tail -20 "$log" >&2
				die "the guest image build failed; the full log is $log"
			}
			ok "built"
		else
			info "Downloading $GUEST_IMAGE into $DATA/microsandbox"
			MSB_HOME="$DATA/microsandbox" "$MSB" pull "$GUEST_IMAGE" >"$log" 2>&1 || {
				tail -20 "$log" >&2
				die "the guest image download failed; the full log is $log"
			}
			ok "downloaded"
		fi
		;;
	esac
	config_set ELSE_VM_IMAGE "$GUEST_IMAGE"
	config_set ELSE_VM_DESKTOP 1
}

run_setup() {
	step "else-api setup"
	local log="$LOGS/setup.log"
	if (cd "$CONFIG" && "$SERVER_CURRENT/bin/else-api" setup) >"$log" 2>&1; then
		grep -v ' INFO \| WARN ' "$log" | sed 's/^/    /'
		return 0
	fi
	tail -20 "$log" >&2
	return 1
}

elsebot() { ELSE_HOME="$ELSE_HOME" "$BIN/elsebot" "$@"; }

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

api_url() {
	local bind=""
	[ -f "$ENV_FILE" ] && bind="$(env_value ELSE_API_BIND)"
	bind="${bind:-127.0.0.1:$API_PORT}"
	case "$bind" in
	0.0.0.0:*) bind="127.0.0.1:${bind#0.0.0.0:}" ;;
	"[::]:"*) bind="[::1]:${bind#"[::]:"}" ;;
	esac
	printf 'http://%s' "$bind"
}
owner_token_file() {
	local file="" data=""
	if [ -f "$ENV_FILE" ]; then
		file="$(env_value OWNER_TOKEN_FILE)"
		data="$(env_value ELSE_DATA_DIR)"
	fi
	printf '%s' "${file:-${data:-$DATA}/owner-token}"
}

api_running() { curl -gfsS --max-time 3 "$(api_url)/healthz" >/dev/null 2>&1; }

show_api_urls() {
	ok "running on $(api_url) (local)"
	local cli=tailscale ip url port
	if ! have "$cli"; then
		cli=/Applications/Tailscale.app/Contents/MacOS/Tailscale
		[ -x "$cli" ] || return 0
	fi
	port="$(api_url)"
	port="${port##*:}"
	while IFS= read -r ip; do
		case "$ip" in
		*:*) url="http://[$ip]:$port" ;;
		*.*.*.*) url="http://$ip:$port" ;;
		*) continue ;;
		esac
		if curl --noproxy '*' -gfsS --max-time 2 "$url/readyz" >/dev/null 2>&1; then
			ok "running on $url (Tailscale)"
		fi
	done < <(TAILSCALE_BE_CLI=1 "$cli" ip 2>/dev/null || true)
}

start_api() {
	step "Start the API"
	elsebot start >/dev/null 2>&1 || true
	local _
	for _ in $(seq 1 60); do
		if curl -gfsS --max-time 2 "$(api_url)/readyz" >/dev/null 2>&1; then
			show_api_urls
			return 0
		fi
		sleep 1
	done
	return 1
}

prepare_running_api() {
	local token response status
	token="$(cat "$(owner_token_file)")"
	response="$(printf 'Authorization: Bearer %s\n' "$token" |
		curl -gsS --max-time 30 -X POST -H @- -w '\n%{http_code}' \
			"$(api_url)/v1/maintenance/update" 2>&1)" || response=$'\n000'
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

backup_if_migrating() {
	local from="$1" to="$2" added
	added="$(git -C "$SRC" diff --name-only --diff-filter=A "$(source_ref "$from")" "$(source_ref "$to")" -- apps/api/migrations 2>/dev/null || echo unknown)"
	[ -z "$added" ] && return 0
	step "Backup before the database changes"
	local log="$LOGS/backup-before-$to.log" scope=""
	# Releases without --database-only archive every bot computer here, which takes minutes.
	case "$("$SERVER_CURRENT/bin/else-api" help 2>/dev/null || true)" in
	*"backup --database-only"*) scope=--database-only ;;
	esac
	if (cd "$CONFIG" && "$SERVER_CURRENT/bin/else-api" backup $scope) >"$log" 2>&1; then
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
	if have grok; then
		ok "$(grok --version 2>/dev/null)"
	else
		info "Grok CLI not found (optional). Install Grok Build and run grok login."
	fi
}


macos_prerequisites() {
	if [ "$FROM_SOURCE" = 0 ]; then
		if want_app; then
			python3 --version >/dev/null 2>&1 || die "Python 3 is required to install the macOS app bundle"
		fi
		return
	fi
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
}

NO_MSB=0
stage2() {
	local target installed mode database_url=""
	if dev_build "$WANT_VERSION"; then
		target="$WANT_VERSION"
		[ "$(git -C "$SRC" rev-parse HEAD)" = "${target#dev-}" ] || die "the source is not at $(release_name "$target")"
	else
		target="$(source_version)"
		[ "$target" = "$WANT_VERSION" ] || die "the source at v$WANT_VERSION reports version $target"
	fi
	migrate_pointers
	choose_components
	OLD_APP="$(readlink "$APP_CURRENT" 2>/dev/null || true)"
	OLD_SERVER="$(readlink "$SERVER_CURRENT" 2>/dev/null || true)"
	OLD_APP_PREVIOUS="$(readlink "$ELSE_HOME/app-previous" 2>/dev/null || true)"
	OLD_SERVER_PREVIOUS="$(readlink "$ELSE_HOME/server-previous" 2>/dev/null || true)"
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

	if [ "$DISTRO" = macos ]; then
		macos_prerequisites
	else
		linux_packages
		if want_server; then linux_access; fi
	fi
	if want_server; then
		decide_guest_image
		if [ "$NO_MSB" != 1 ]; then ensure_msb; fi
	else
		GUEST_IMAGE_STATE=skip
	fi
	install_release "$target"
	if want_server; then
		install_server "$mode" "$installed" "$target"
	else
		step "Switch to $target"
		switch_to "$target"
		install_commands
		if [ -n "$installed" ] && [ "$installed" != "$target" ]; then prune_versions; fi
		ok "the app is installed"
	fi
	if want_app; then
		if [ "$DISTRO" = macos ]; then
			next "open the app: else.bot in $(dirname "$APP"), or elsebot desktop"
		else
			next "open the app: elsebot desktop (or else.bot in your launcher)"
		fi
	fi
	if ! want_server && [ ! -L "$SERVER_CURRENT" ]; then
		next "on first launch, enter your server's address and owner token (on the server: cat ~/.else/data/owner-token)"
	fi
	remember_components
	remember_channel
	return 0
}

install_server() {
	local mode="$1" installed="$2" target="$3" database_url
	if [ "$mode" = reset ]; then
		database_url="${ELSE_DATABASE_URL:-sqlite://$DATA/else.sqlite3}"
		case "$database_url" in sqlite:*) ;; *) die "ELSE_DATABASE_URL must be a SQLite URL" ;; esac
		step "Set up SQLite"
		elsebot stop || die "could not stop the API before setting up SQLite"
		switch_to "$target"
		config_set DATABASE_URL "$database_url"
	fi
	write_config
	ensure_bots_dir
	refresh_provider_paths
	prepare_guest_image

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
			restore_pointers
			install_commands
			elsebot start >/dev/null 2>&1 || true
			die "the update to $target failed and $installed runs again (logs: elsebot logs). If the database was changed, restore the backup above."
		fi
		prune_versions
		ok "updated to $target (kept $installed in $VERSIONS)"
	else
		switch_to "$target"
		install_commands
		setup_service
		if [ "$mode" != current ]; then
			run_setup || die "else-api setup failed (log: $LOGS/setup.log)"
		fi
		if api_running; then
			show_api_urls
		else
			if [ "$mode" = current ]; then
				run_setup || die "else-api setup failed (log: $LOGS/setup.log)"
			fi
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
		--from-source) FROM_SOURCE=1 ;;
		--channel)
			case "${2:-}" in
			stable | dev) CHANNEL_FLAG="$2" ;;
			*) die "--channel needs stable or dev" ;;
			esac
			shift
			;;
		--autostart) AUTOSTART=1 ;;
		--no-autostart) AUTOSTART=0 ;;
		--skip-guest-image) SKIP_GUEST=1 ;;
		-h | --help) usage; exit 0 ;;
		*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
		esac
		shift
	done
	detect_platform
	if dev_build "${WANT_VERSION#v}"; then FROM_SOURCE=1; fi
	if [ "$FROM_SOURCE" = 0 ]; then
		release_platform >/dev/null
		[ "$BUILD_PROFILE" = release ] || die "ELSE_INSTALL_TEST_PROFILE requires --from-source"
	fi
	printf '%selse.bot installer%s\n' "$BOLD" "$RESET"
	info "System: $DISTRO_NAME ($ARCH)"
	if [ "${ELSE_INSTALL_STAGE:-1}" != 2 ]; then
		ensure_bootstrap_tools
		lock_installation
		resolve_release
		fetch_source
		local installer_help inventory remember_here=0
		installer_help="$(bash "$SRC/install.sh" --help 2>/dev/null)"
		inventory="$(installed_components)"
		if ! printf '%s' "$installer_help" | grep -q -- '--channel'; then
			# Releases before the dev channel mistake a dev build of the same
			# version for themselves and skip the server's restart and rollback.
			if installed_dev_build; then
				[ -z "$PREVIOUS_SOURCE" ] || git -C "$SRC" -c advice.detachedHead=false checkout -q --force "$PREVIOUS_SOURCE"
				die "v$WANT_VERSION predates the dev channel and cannot replace a dev build; after the next release, run: elsebot update --channel stable"
			fi
			local argument skip=0 forwarded=()
			for argument in ${ARGS[@]+"${ARGS[@]}"}; do
				if [ "$skip" = 1 ]; then skip=0; continue; fi
				if [ "$argument" = --channel ]; then skip=1; continue; fi
				forwarded+=("$argument")
			done
			ARGS=(${forwarded[@]+"${forwarded[@]}"})
			remember_here=1
		fi
		if ! printf '%s' "$installer_help" | grep -q -- 'app-current'; then
			if [ -L "$APP_CURRENT" ] || [ -L "$SERVER_CURRENT" ]; then
				die "this release's installer predates component pointers; choose a release that supports component updates"
			fi
			case "$COMPONENTS_FLAG/$inventory" in
			app/server | app/both | server/app | server/both)
				die "this release's installer cannot update components independently; choose a newer release or update both components explicitly with --both"
				;;
			esac
		fi
		if ! printf '%s' "$installer_help" | grep -q -- '--from-source'; then
			# This release predates release binaries: its installer always builds.
			[ "$FROM_SOURCE" = 1 ] || info "v$WANT_VERSION has no release binaries; its installer builds it from source"
			local argument forwarded=()
			for argument in ${ARGS[@]+"${ARGS[@]}"}; do
				if [ "$argument" != --from-source ]; then forwarded+=("$argument"); fi
			done
			ARGS=(${forwarded[@]+"${forwarded[@]}"})
		elif [ "$FROM_SOURCE" = 0 ] && ! printf '%s' "$installer_help" | grep -q -- 'without binaries yet'; then
			# This release's installer cannot fall back to a source build itself.
			choose_components
			case "$COMPONENTS" in
			both) ARGS+=(--both) ;;
			server) ARGS+=(--server-only) ;;
			app) ARGS+=(--app-only) ;;
			esac
			release_unpublished "$WANT_VERSION" "$COMPONENTS"
			if [ -n "$RELEASE_MISSING" ]; then
				info "$RELEASE_MISSING; building it from source"
				ARGS+=(--from-source)
			fi
		fi
		export ELSE_INSTALL_STAGE=2 ELSE_VERSION="$WANT_VERSION"
		bash "$SRC/install.sh" ${ARGS[@]+"${ARGS[@]}"}
		if [ "$remember_here" = 1 ]; then remember_channel; fi
		return
	fi

	lock_installation
	mkdir -p "$LOGS"
	stage2
	summary
}

if [ "${BASH_SOURCE[0]:-$0}" = "$0" ]; then main "$@"; fi
