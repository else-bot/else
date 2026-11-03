#!/usr/bin/env bash
# else.bot host installer. Run from a checkout of the repository:
#
#   ./install.sh [options]
#
# Linux (Debian, Ubuntu, Arch Linux / Omarchy): a full installation. System
# packages (with sudo, only when missing), Rust, Microsandbox, release builds
# of else-api and else-desktop, PostgreSQL in Docker Compose, the guest VM
# image, `else-api setup`, and a systemd user service for the API.
#
# macOS: the desktop client only (the API needs Linux). Point it at an API on
# a Linux machine through an SSH tunnel. Not yet tested on a real Mac.
#
# Safe to run again: it installs only what is missing and never replaces an
# existing configuration, database, or installation.
set -euo pipefail

usage() {
	cat <<'EOF'
Usage: ./install.sh [options]

  -y, --yes            do not ask for confirmation (sudo may still ask for a password)
  --server-only        Linux: skip the desktop client and its libraries (headless servers)
  --skip-build         do not build or install the binaries
  --skip-database      do not start PostgreSQL or run `else-api setup`
  --skip-guest-image   do not build the guest VM image (about 15 minutes the first time)
  --skip-service       do not install or start the systemd user service
  --bin-dir DIR        where binaries go (default ~/.local/bin)
  --data-dir DIR       installation data (default ~/.local/share/else-bot)
  --config-dir DIR     configuration (default ~/.config/else-bot)
  -h, --help           show this help
EOF
}

# --- Settings ---------------------------------------------------------------

MIN_RUST="1.94.0"
MSB_VERSION="0.7.13"
COMPOSE_VERSION="5.6.1"
PG_MAJOR="18"
GUEST_TAG="else-guest:2"
API_BIND="127.0.0.1:7878"

ASSUME_YES=0
SERVER_ONLY=0
SKIP_BUILD=0
SKIP_DATABASE=0
SKIP_GUEST=0
SKIP_SERVICE=0
BIN_DIR="$HOME/.local/bin"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/else-bot"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/else-bot"

while [ $# -gt 0 ]; do
	case "$1" in
	-y | --yes) ASSUME_YES=1 ;;
	--server-only) SERVER_ONLY=1 ;;
	--skip-build) SKIP_BUILD=1 ;;
	--skip-database) SKIP_DATABASE=1 ;;
	--skip-guest-image) SKIP_GUEST=1 ;;
	--skip-service) SKIP_SERVICE=1 ;;
	--bin-dir) BIN_DIR="$2"; shift ;;
	--data-dir) DATA_DIR="$2"; shift ;;
	--config-dir) CONFIG_DIR="$2"; shift ;;
	-h | --help) usage; exit 0 ;;
	*) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
	esac
	shift
done

USER="${USER:-$(id -un)}"
REPO="$(cd "$(dirname "$0")" && pwd -P)"
if [ ! -f "$REPO/Cargo.toml" ] || [ ! -d "$REPO/apps/api" ]; then
	echo "install.sh must stay at the root of the else.bot repository" >&2
	exit 1
fi

# --- Output -----------------------------------------------------------------

if [ -t 1 ]; then
	BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; GREEN=$'\033[32m'; YELLOW=$'\033[33m'; RESET=$'\033[0m'
else
	BOLD=""; DIM=""; RED=""; GREEN=""; YELLOW=""; RESET=""
fi
step() { printf '\n%s==> %s%s\n' "$BOLD" "$1" "$RESET"; }
info() { printf '    %s\n' "$1"; }
ok() { printf '    %s✓%s %s\n' "$GREEN" "$RESET" "$1"; }
warn() { printf '    %s!%s %s\n' "$YELLOW" "$RESET" "$1"; WARNINGS="${WARNINGS}  - $1"$'\n'; }
die() { printf '%serror:%s %s\n' "$RED" "$RESET" "$1" >&2; exit 1; }
WARNINGS=""
NEXT=""
next() { NEXT="${NEXT}  - $1"$'\n'; }

confirm() {
	[ "$ASSUME_YES" = 1 ] && return 0
	if [ ! -t 0 ]; then
		die "$1 Re-run with --yes to proceed non-interactively."
	fi
	printf '    %s [y/N] ' "$1"
	local answer
	read -r answer
	case "$answer" in y | Y | yes | YES) return 0 ;; *) return 1 ;; esac
}

have() { command -v "$1" >/dev/null 2>&1; }

# version_ge A B: true when version A >= B (dotted numbers).
version_ge() {
	[ "$(printf '%s\n%s\n' "$2" "$1" | sort -t. -k1,1n -k2,2n -k3,3n | head -1)" = "$2" ]
}

SUDO=""
sudo_ready() {
	if [ "$(id -u)" = 0 ]; then
		SUDO=""
		return
	fi
	have sudo || die "sudo is required to install system packages (install it, or install the packages listed above as root)"
	SUDO="sudo"
	info "${DIM}sudo may ask for your password${RESET}"
	sudo -v || die "sudo was refused"
}

# --- Platform ---------------------------------------------------------------

OS="$(uname -s)"
ARCH="$(uname -m)"
DISTRO=""
DISTRO_NAME=""
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
	# Ubuntu derivatives list both; keep Ubuntu's package names for them.
	case " ${ID_LIKE:-} " in *" ubuntu "*) DISTRO=ubuntu ;; esac
	[ "$ID" = ubuntu ] && DISTRO=ubuntu
	;;
Darwin)
	DISTRO=macos
	DISTRO_NAME="macOS $(sw_vers -productVersion 2>/dev/null || echo)"
	;;
*) die "unsupported operating system: $OS (supported: Linux, macOS)" ;;
esac
case "$ARCH" in
x86_64 | amd64) ARCH=x86_64 ;;
aarch64 | arm64) ARCH=aarch64 ;;
*) die "unsupported architecture: $ARCH" ;;
esac
if [ "$DISTRO" != macos ] && [ "$ARCH" != x86_64 ]; then
	warn "$ARCH Linux has not been tested yet; continuing"
fi

if [ "$(id -u)" = 0 ]; then
	die "run install.sh as the user who will own else.bot, not as root; it asks for sudo when needed"
fi

printf '%selse.bot installer%s\n' "$BOLD" "$RESET"
info "System: $DISTRO_NAME ($ARCH)"
info "Repository: $REPO"
if [ "$DISTRO" = macos ]; then
	info "macOS gets the desktop client; the API runs on a Linux machine."
fi

# --- System packages --------------------------------------------------------

# Packages by role. The desktop libraries are only needed to build and run
# else-desktop; bubblewrap, Docker, and the PostgreSQL client tools are what
# the API needs at runtime.
apt_packages() {
	local list="build-essential pkg-config curl ca-certificates git jq tar gzip bubblewrap"
	if [ "$SERVER_ONLY" = 0 ]; then
		list="$list libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libxcb1-dev libx11-xcb-dev libx11-dev libfontconfig-dev libfreetype-dev libvulkan1 libzstd-dev"
	fi
	echo "$list"
}
pacman_packages() {
	local list="base-devel pkgconf curl ca-certificates git jq tar gzip bubblewrap postgresql-libs"
	if [ "$SERVER_ONLY" = 0 ]; then
		list="$list libxkbcommon libxkbcommon-x11 wayland libxcb libx11 fontconfig freetype2 vulkan-icd-loader zstd"
	fi
	echo "$list"
}

missing_packages() {
	local pkg missing=""
	for pkg in "$@"; do
		case "$DISTRO" in
		debian | ubuntu) [ "$(dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null || true)" = "install ok installed" ] || missing="$missing $pkg" ;;
		arch) pacman -Q "$pkg" >/dev/null 2>&1 || pacman -Qg "$pkg" >/dev/null 2>&1 || missing="$missing $pkg" ;;
		esac
	done
	echo "${missing# }"
}

install_packages() {
	[ $# -gt 0 ] || return 0
	case "$DISTRO" in
	debian | ubuntu)
		$SUDO env DEBIAN_FRONTEND=noninteractive apt-get update -qq
		$SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends "$@"
		;;
	arch)
		$SUDO pacman -Sy --needed --noconfirm "$@"
		;;
	esac
}

# Docker Engine with the Compose plugin. Kept if any Docker is present
# (Docker's own packages, Docker Desktop); otherwise the distribution's.
docker_packages() {
	if have docker; then
		docker compose version >/dev/null 2>&1 && return 0
		case "$DISTRO" in
		ubuntu) echo "docker-compose-v2" ;;
		debian) echo "docker-compose" ;;
		arch) echo "docker-compose" ;;
		esac
		return 0
	fi
	case "$DISTRO" in
	ubuntu) echo "docker.io docker-compose-v2" ;;
	# Debian 12's docker-compose is the old v1 tool: the plugin comes from
	# Docker's release instead (install_compose_plugin).
	debian) if [ "${VERSION_ID%%.*}" -ge 13 ] 2>/dev/null; then echo "docker.io docker-compose"; else echo "docker.io"; fi ;;
	arch) echo "docker docker-compose" ;;
	esac
}

# The Compose plugin from Docker's release, for distributions without a
# packaged one; per user, in ~/.docker/cli-plugins, checksum verified.
install_compose_plugin() {
	docker compose version >/dev/null 2>&1 && return 0
	local base="https://github.com/docker/compose/releases/download/v$COMPOSE_VERSION"
	local asset="docker-compose-linux-$ARCH" dir="$HOME/.docker/cli-plugins" tmp
	info "Installing the Docker Compose plugin $COMPOSE_VERSION into $dir"
	tmp="$(mktemp -d)"
	curl -fsSL "$base/$asset" -o "$tmp/$asset"
	curl -fsSL "$base/$asset.sha256" -o "$tmp/$asset.sha256"
	(cd "$tmp" && sha256sum -c --quiet "$asset.sha256") || die "checksum verification of the Compose plugin failed"
	mkdir -p "$dir"
	install -m 755 "$tmp/$asset" "$dir/docker-compose"
	rm -rf "$tmp"
	docker compose version >/dev/null 2>&1 || die "docker compose still does not work"
}

pg_dump_major() {
	local bin="$1"
	"$bin" --version 2>/dev/null | sed -n 's/.* \([0-9][0-9]*\)\..*/\1/p' | head -1
}

# The PostgreSQL client tools for backups must be at least the server's
# major version. Debian and Ubuntu ship older ones: use the PostgreSQL
# project's apt repository (PGDG) for them.
PG_DUMP=""
PG_RESTORE=""
find_pg_tools() {
	local dir
	for dir in "/usr/lib/postgresql/$PG_MAJOR/bin" /usr/bin /usr/local/bin; do
		if [ -x "$dir/pg_dump" ] && [ -x "$dir/pg_restore" ]; then
			local major
			major="$(pg_dump_major "$dir/pg_dump")"
			if [ -n "$major" ] && [ "$major" -ge "$PG_MAJOR" ]; then
				PG_DUMP="$dir/pg_dump"
				PG_RESTORE="$dir/pg_restore"
				return 0
			fi
		fi
	done
	return 1
}

setup_pgdg() {
	if [ -f /etc/apt/sources.list.d/pgdg.list ] || [ -f /etc/apt/sources.list.d/pgdg.sources ]; then
		return 0
	fi
	info "Adding the PostgreSQL apt repository (apt.postgresql.org) for postgresql-client-$PG_MAJOR"
	install_packages postgresql-common
	$SUDO /usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y >/dev/null
}

linux_packages() {
	step "System packages"
	local wanted missing
	case "$DISTRO" in
	debian | ubuntu) wanted="$(apt_packages) $(docker_packages)" ;;
	arch) wanted="$(pacman_packages) $(docker_packages)" ;;
	esac
	# shellcheck disable=SC2086
	missing="$(missing_packages $wanted)"
	local need_pg=0
	find_pg_tools || need_pg=1
	if [ -z "$missing" ] && [ "$need_pg" = 0 ]; then
		ok "all present"
	else
		[ -n "$missing" ] && info "Missing: $missing"
		if [ "$need_pg" = 1 ] && [ "$DISTRO" != arch ]; then
			info "Missing: postgresql-client-$PG_MAJOR (pg_dump and pg_restore $PG_MAJOR for backups)"
		fi
		confirm "Install them with sudo?" || die "cannot continue without the system packages"
		sudo_ready
		# shellcheck disable=SC2086
		install_packages $missing
		if [ "$need_pg" = 1 ] && [ "$DISTRO" != arch ]; then
			setup_pgdg
			install_packages "postgresql-client-$PG_MAJOR"
		fi
		# Debian 13 and newer Ubuntus ship the Docker CLI separately.
		if ! have docker && [ "$DISTRO" != arch ]; then
			install_packages docker-cli
		fi
		find_pg_tools || die "pg_dump $PG_MAJOR or newer is still missing"
		ok "installed"
	fi
	install_compose_plugin
	ok "$(docker compose version)"
	ok "pg_dump: $PG_DUMP ($("$PG_DUMP" --version | head -1))"
}

# --- Groups and services ----------------------------------------------------

DOCKER="docker"
# Whole-word match on captured output (a `| grep -q` pipeline can fail on
# SIGPIPE under pipefail).
in_group() { case " $(id -nG "$USER" 2>/dev/null || true) " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
active_in_session() { case " $(id -nG 2>/dev/null || true) " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
has_systemd() { [ -d /run/systemd/system ]; }

linux_access() {
	step "Docker and KVM access"
	if has_systemd && ! systemctl is-active --quiet docker 2>/dev/null; then
		if systemctl list-unit-files docker.service >/dev/null 2>&1; then
			sudo_ready
			if $SUDO systemctl enable --now docker >/dev/null 2>&1; then
				ok "Docker service started"
			else
				warn "could not start the Docker service"
			fi
		fi
	fi
	if ! in_group docker && getent group docker >/dev/null 2>&1; then
		confirm "Add $USER to the docker group (lets you run Docker without sudo)?" && {
			sudo_ready
			$SUDO usermod -aG docker "$USER"
			ok "added to the docker group"
			next "log out and back in so the docker group applies to new sessions"
		}
	fi
	# This run may predate the new membership: go through `sg` for it.
	if in_group docker && ! active_in_session docker; then
		DOCKER="sg docker -c docker"
	fi
	if docker_run info >/dev/null 2>&1; then
		ok "Docker works"
	else
		warn "Docker is not reachable from this session (no daemon running, or no permission)"
	fi

	if [ ! -e /dev/kvm ]; then
		warn "/dev/kvm is missing: bot computers need hardware virtualization (enable VT-x/AMD-V, or nested virtualization in a VM)"
	elif [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
		ok "/dev/kvm is usable"
	elif getent group kvm >/dev/null 2>&1 && ! in_group kvm; then
		confirm "Add $USER to the kvm group (needed to run bot computers)?" && {
			sudo_ready
			$SUDO usermod -aG kvm "$USER"
			ok "added to the kvm group"
			next "log out and back in (or reboot) so the kvm group applies, then restart the service: systemctl --user restart else-api"
		}
	else
		warn "/dev/kvm is not accessible to $USER"
	fi

	# Ubuntu 24.04+ restricts unprivileged user namespaces through AppArmor,
	# which bubblewrap needs to sandbox the provider CLIs.
	if have bwrap; then
		if bwrap --unshare-user --ro-bind / / true >/dev/null 2>&1; then
			ok "bubblewrap sandboxes work"
		else
			warn "bubblewrap cannot create a sandbox here (unprivileged user namespaces are restricted); provider CLIs will not run. On Ubuntu see: sysctl kernel.apparmor_restrict_unprivileged_userns"
		fi
	fi
}

# docker_run ARGS...: docker, through `sg docker` when the group is new.
docker_run() {
	if [ "$DOCKER" = docker ]; then
		docker "$@"
	else
		local quoted="" arg
		for arg in "$@"; do quoted="$quoted $(printf '%q' "$arg")"; done
		sg docker -c "docker$quoted"
	fi
}

# --- Rust -------------------------------------------------------------------

CARGO="cargo"
ensure_rust() {
	step "Rust (at least $MIN_RUST)"
	# shellcheck disable=SC1091
	[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
	local current=""
	if have rustc; then
		current="$(rustc --version | awk '{print $2}')"
	fi
	if [ -n "$current" ] && version_ge "$current" "$MIN_RUST"; then
		ok "rustc $current"
		return
	fi
	if have rustup; then
		info "rustc ${current:-is missing}; installing the stable toolchain with rustup"
		rustup toolchain install stable --profile minimal
		CARGO="cargo +stable"
	else
		[ -n "$current" ] && info "rustc $current is too old; installing rustup (your system Rust is kept)"
		confirm "Install Rust with rustup (https://rustup.rs) into ~/.cargo?" || die "Rust $MIN_RUST or newer is required"
		curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path
		# shellcheck disable=SC1091
		. "$HOME/.cargo/env"
		next "add ~/.cargo/bin to your PATH (rustup prints how) to use cargo yourself"
	fi
	ok "$($CARGO --version)"
}

# --- Microsandbox -----------------------------------------------------------

MSB=""
ensure_msb() {
	step "Microsandbox $MSB_VERSION"
	local home="$HOME/.microsandbox" found=""
	for found in "$(command -v msb 2>/dev/null || true)" "$home/bin/msb"; do
		[ -n "$found" ] && [ -x "$found" ] && break
		found=""
	done
	if [ -n "$found" ]; then
		local version
		version="$("$found" --version 2>/dev/null | awk '{print $2}')"
		MSB="$found"
		if [ "$version" = "$MSB_VERSION" ]; then
			ok "msb $version ($found)"
		else
			warn "msb $version is installed ($found); else.bot is verified with $MSB_VERSION. Kept as it is."
		fi
		return
	fi
	# The official installer always takes the latest release; install the
	# verified one from the same release bundle, checksum checked.
	local target="linux-$ARCH" base tmp lib abi
	base="https://github.com/superradcompany/microsandbox/releases/download/v$MSB_VERSION"
	tmp="$(mktemp -d)"
	info "Downloading microsandbox-$target.tar.gz"
	curl -fsSL "$base/microsandbox-$target.tar.gz" -o "$tmp/bundle.tar.gz"
	curl -fsSL "$base/checksums.sha256" -o "$tmp/checksums.sha256"
	(cd "$tmp" && grep -F " microsandbox-$target.tar.gz" checksums.sha256 | sed "s|microsandbox-$target.tar.gz|bundle.tar.gz|" | sha256sum -c --quiet -) ||
		die "checksum verification of the Microsandbox bundle failed"
	tar -xzf "$tmp/bundle.tar.gz" -C "$tmp"
	lib="$(cd "$tmp" && find . -maxdepth 1 -name 'libkrunfw.so.*.*.*' | sed 's|^\./||' | head -1)"
	[ -n "$lib" ] || die "the Microsandbox bundle has no libkrunfw library"
	abi="${lib#libkrunfw.so.}"
	abi="${abi%%.*}"
	mkdir -p "$home/bin" "$home/lib"
	install -m 755 "$tmp/msb" "$home/bin/msb"
	ln -sf msb "$home/bin/microsandbox"
	install -m 644 "$tmp/$lib" "$home/lib/$lib"
	ln -sf "$lib" "$home/lib/libkrunfw.so.$abi"
	ln -sf "libkrunfw.so.$abi" "$home/lib/libkrunfw.so"
	rm -rf "$tmp"
	MSB="$home/bin/msb"
	mkdir -p "$BIN_DIR"
	[ -e "$BIN_DIR/msb" ] || ln -s "$MSB" "$BIN_DIR/msb"
	ok "msb $("$MSB" --version | awk '{print $2}') ($MSB)"
}

# --- Build ------------------------------------------------------------------

build_binaries() {
	local packages="$1"
	step "Release build (${packages}); the first build takes several minutes"
	local args="" pkg
	for pkg in $packages; do args="$args -p $pkg"; done
	# shellcheck disable=SC2086
	(cd "$REPO" && $CARGO build --release --locked $args)
	mkdir -p "$BIN_DIR"
	for pkg in $packages; do
		install -m 755 "$REPO/target/release/$pkg" "$BIN_DIR/$pkg"
		ok "$BIN_DIR/$pkg"
	done
	case ":$PATH:" in
	*":$BIN_DIR:"*) ;;
	*) next "add $BIN_DIR to your PATH" ;;
	esac
}

# --- Configuration ----------------------------------------------------------

ENV_FILE="$CONFIG_DIR/.env"

env_value() { sed -n "s/^$1=//p" "$ENV_FILE" | tail -1; }

free_port() {
	local port
	for port in 55432 55433 55434 55435 55436; do
		if ! (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
			echo "$port"
			return
		fi
	done
	die "no free local port for PostgreSQL (tried 55432-55436)"
}

write_config() {
	step "Configuration ($ENV_FILE)"
	if [ -f "$ENV_FILE" ]; then
		ok "kept the existing configuration"
		return
	fi
	# Never adopt an installation this script did not configure (a
	# development checkout's, for example): its database is elsewhere.
	if [ -e "$DATA_DIR/secret-key" ] || [ -e "$DATA_DIR/owner-token" ]; then
		die "$DATA_DIR already holds an else.bot installation that has no configuration in $CONFIG_DIR. Choose another place with --data-dir and --config-dir, or move it away first."
	fi
	mkdir -p "$CONFIG_DIR"
	chmod 700 "$CONFIG_DIR"
	local password port codex claude
	# 48 hex characters; no pipe that could end on SIGPIPE under pipefail.
	password="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
	port="$(free_port)"
	codex="$(command -v codex 2>/dev/null || true)"
	claude="$(command -v claude 2>/dev/null || true)"
	umask 077
	cat >"$ENV_FILE" <<EOF
# else.bot installation, written by install.sh. The API reads this file from
# its working directory ($CONFIG_DIR); so does Docker Compose.

# PostgreSQL (Docker Compose project in this directory)
COMPOSE_PROJECT_NAME=else-bot
POSTGRES_DB=else
POSTGRES_USER=else
POSTGRES_PASSWORD=$password
POSTGRES_PORT=$port

# API
DATABASE_URL=postgresql://else:$password@127.0.0.1:$port/else
ELSE_API_BIND=$API_BIND
ELSE_DATA_DIR=$DATA_DIR
ELSE_MSB_BIN=$MSB
ELSE_PG_DUMP=$PG_DUMP
ELSE_PG_RESTORE=$PG_RESTORE
${codex:+ELSE_CODEX_BIN=$codex}
${claude:+ELSE_CLAUDE_BIN=$claude}
EOF
	umask 022
	cat >"$CONFIG_DIR/compose.yaml" <<'EOF'
# PostgreSQL for this else.bot installation, written by install.sh.
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
	ok "written (mode 0600); PostgreSQL on 127.0.0.1:$port"
}

# Append KEY=VALUE unless the file already sets KEY.
config_set() {
	grep -q "^$1=" "$ENV_FILE" || printf '%s=%s\n' "$1" "$2" >>"$ENV_FILE"
}

start_database() {
	step "PostgreSQL (Docker Compose project else-bot)"
	docker_run compose --project-directory "$CONFIG_DIR" up -d --wait >/dev/null ||
		die "PostgreSQL did not start; see: docker compose --project-directory $CONFIG_DIR logs"
	ok "running on 127.0.0.1:$(env_value POSTGRES_PORT)"
}

build_guest_image() {
	step "Guest VM image $GUEST_TAG"
	local home="$DATA_DIR/microsandbox"
	local images=""
	[ -d "$home" ] && images="$(MSB_HOME="$home" "$MSB" image list --format json 2>/dev/null || true)"
	if case "$images" in *"\"$GUEST_TAG\""*) true ;; *) false ;; esac; then
		ok "already loaded into $home"
	else
		info "Building the image and loading it into $home (about 15 minutes the first time)"
		if [ "$DOCKER" = docker ]; then
			ELSE_MSB_BIN="$MSB" "$REPO/scripts/build-guest-image.sh" "$DATA_DIR"
		else
			sg docker -c "ELSE_MSB_BIN=$(printf '%q' "$MSB") $(printf '%q' "$REPO/scripts/build-guest-image.sh") $(printf '%q' "$DATA_DIR")"
		fi
		ok "loaded"
	fi
	config_set ELSE_VM_IMAGE "$GUEST_TAG"
	config_set ELSE_VM_DESKTOP 1
}

run_setup() {
	step "else-api setup"
	mkdir -p "$DATA_DIR"
	chmod 700 "$DATA_DIR"
	(cd "$CONFIG_DIR" && "$BIN_DIR/else-api" setup) | sed 's/^/    /'
}

install_service() {
	step "systemd user service else-api"
	if ! systemctl --user show-environment >/dev/null 2>&1; then
		warn "no systemd user session here; start the API yourself: cd $CONFIG_DIR && else-api serve"
		return
	fi
	local unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
	mkdir -p "$unit_dir"
	cat >"$unit_dir/else-api.service" <<EOF
[Unit]
Description=else.bot API
After=network-online.target

[Service]
WorkingDirectory=$CONFIG_DIR
ExecStart=$BIN_DIR/else-api serve
Environment=PATH=$BIN_DIR:$HOME/.microsandbox/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF
	systemctl --user daemon-reload
	systemctl --user enable else-api.service >/dev/null 2>&1
	systemctl --user restart else-api.service
	local ready=0 _
	for _ in $(seq 1 30); do
		if curl -fsS "http://$API_BIND/readyz" >/dev/null 2>&1; then
			ready=1
			break
		fi
		sleep 1
	done
	if [ "$ready" = 1 ]; then
		ok "running on http://$API_BIND"
	else
		warn "the API did not become ready within 30 s; see: journalctl --user -u else-api"
	fi
	if have loginctl && [ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null)" != yes ]; then
		next "to keep the API running while you are logged out: sudo loginctl enable-linger $USER"
	fi
}

check_providers() {
	step "Provider CLIs"
	if have codex; then
		ok "codex $(codex --version 2>/dev/null | awk '{print $NF}')"
		if ! codex login status >/dev/null 2>&1; then
			next "log in to Codex once: codex login"
		fi
	else
		warn "Codex CLI not found: install it (https://github.com/openai/codex), run 'codex login', then re-run install.sh"
	fi
	if have claude; then
		ok "claude $(claude --version 2>/dev/null | awk '{print $1}')"
		next "if Claude Code is not logged in yet, run 'claude' once and log in"
	else
		info "Claude Code not found (optional): https://claude.com/claude-code"
	fi
}

# --- macOS ------------------------------------------------------------------

macos_install() {
	step "Xcode"
	if ! xcode-select -p >/dev/null 2>&1; then
		xcode-select --install >/dev/null 2>&1 || true
		die "install the Xcode command line tools (a dialog was opened), then re-run install.sh"
	fi
	# GPUI compiles its Metal shaders at build time: that needs Xcode itself.
	if ! xcrun -sdk macosx -f metal >/dev/null 2>&1; then
		die "the Metal compiler is missing: install Xcode from the App Store, then run 'sudo xcode-select -s /Applications/Xcode.app' and 'xcodebuild -downloadComponent MetalToolchain', and re-run install.sh"
	fi
	ok "Xcode with the Metal compiler"
	ensure_rust
	[ "$SKIP_BUILD" = 1 ] || build_binaries "else-desktop"
	next "on the Linux machine running the API, copy its owner token here: scp SERVER:~/.local/share/else-bot/owner-token ~/.local/share/else-bot/owner-token && chmod 600 ~/.local/share/else-bot/owner-token"
	next "open a tunnel to the API (keep it running): ssh -N -L 7878:127.0.0.1:7878 SERVER"
	next "start the desktop: else-desktop"
}

# --- Main -------------------------------------------------------------------

if [ "$DISTRO" = macos ]; then
	macos_install
else
	linux_packages
	linux_access
	ensure_rust
	ensure_msb
	if [ "$SKIP_BUILD" = 0 ]; then
		if [ "$SERVER_ONLY" = 1 ]; then build_binaries "else-api"; else build_binaries "else-api else-desktop"; fi
	fi
	write_config
	if [ "$SKIP_DATABASE" = 0 ]; then
		start_database
		[ "$SKIP_GUEST" = 1 ] || build_guest_image
		[ -x "$BIN_DIR/else-api" ] || die "else-api is not installed in $BIN_DIR (do not combine --skip-build with setup on a first install)"
		run_setup
		[ "$SKIP_SERVICE" = 1 ] || install_service
	fi
	check_providers
	if [ "$SERVER_ONLY" = 0 ]; then
		next "start the desktop: else-desktop"
	fi
fi

step "Done"
if [ -n "$WARNINGS" ]; then
	printf '%sWarnings:%s\n%s' "$YELLOW" "$RESET" "$WARNINGS"
fi
if [ -n "$NEXT" ]; then
	printf '%sNext:%s\n%s' "$BOLD" "$RESET" "$NEXT"
fi
