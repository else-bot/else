#!/usr/bin/env bash
#   scripts/build-guest-base.sh [--builder NAME] [--platform OS/ARCH]... [--skip-build]
set -euo pipefail
cd "$(dirname "$0")/.."

die() { echo "build-guest-base: $1" >&2; exit 1; }

REPO="${ELSE_GUEST_BASE_REPO:-ghcr.io/else-bot/else-guest-base}"
export ELSE_GUEST_BASE_REPO="$REPO"
required=(linux/amd64 linux/arm64)
platforms=()
builder=""
build=1
while [ $# -gt 0 ]; do
	case "$1" in
	--builder) [ $# -ge 2 ] || die "--builder needs a name"; builder="$2"; shift ;;
	--platform)
		[ $# -ge 2 ] || die "--platform needs linux/amd64 or linux/arm64"
		case " ${required[*]} " in *" $2 "*) ;; *) die "unsupported platform $2 (use linux/amd64 or linux/arm64)" ;; esac
		platforms+=("$2"); shift ;;
	--skip-build) build=0 ;;
	*) die "usage: scripts/build-guest-base.sh [--builder NAME] [--platform OS/ARCH]... [--skip-build]" ;;
	esac
	shift
done
[ ${#platforms[@]} -gt 0 ] || platforms=("${required[@]}")

helper() { python3 scripts/guest-release.py --base "$@"; }
buildx=(docker buildx)
[ -z "$builder" ] || buildx+=(--builder "$builder")

described="$(helper build-args)"
mapfile -t args <<<"$described"
hash="$(helper hash)"
commit="$(git rev-parse HEAD)"
context="$(mktemp -d)"
trap 'rm -rf "$context"' EXIT
git archive "$commit" -- infra/guest | tar -x -C "$context"

unpushed() {
	for platform in "$@"; do
		"${buildx[@]}" imagetools inspect "$REPO:src-$hash-${platform#*/}" >/dev/null 2>&1 || echo "$platform"
	done
}

if [ "$build" = 1 ]; then
	mapfile -t todo < <(unpushed "${platforms[@]}")
	if [ ${#todo[@]} -gt 0 ]; then
		supported="$("${buildx[@]}" inspect --bootstrap | sed -n 's/^Platforms:[[:space:]]*//p' | tr -d '*' | tr ',' '\n' | tr -d ' ')" ||
			die "cannot inspect the Buildx builder${builder:+ $builder}"
		for platform in "${todo[@]}"; do
			grep -qx "$platform" <<<"$supported" || die "the Buildx builder${builder:+ $builder} cannot build $platform (it supports: $(echo $supported)).
Build $platform natively on a $platform machine with: scripts/build-guest-base.sh --platform $platform
or pass --builder with a builder that supports it. Registering QEMU emulation is a privileged host change this script does not make."
		done
		native="linux/$(docker version --format '{{.Server.Arch}}')"
		for platform in "${todo[@]}"; do
			[ "$platform" = "$native" ] || echo "Building $platform under emulation on a $native host; this can take hours. A native $platform machine is much faster."
			echo "Building and pushing $REPO:src-$hash-${platform#*/}"
			(cd "$context" && "${buildx[@]}" build --platform "$platform" --provenance=false --sbom=false "${args[@]}" \
				--tag "$REPO:src-$hash-${platform#*/}" --push infra/guest)
		done
	fi
fi

mapfile -t missing < <(unpushed "${required[@]}")
if [ ${#missing[@]} -gt 0 ]; then
	echo "The guest base for commit $commit still needs: ${missing[*]}."
	for platform in "${missing[@]}"; do
		echo "  On a $platform machine at commit $commit, run: scripts/build-guest-base.sh --platform $platform"
	done
	echo "Then run here: scripts/build-guest-base.sh --skip-build"
	exit 0
fi

sources=()
for platform in "${required[@]}"; do sources+=("$REPO:src-$hash-${platform#*/}"); done
"${buildx[@]}" imagetools create --tag "$REPO:src-$hash" "${sources[@]}"
digest="$("${buildx[@]}" imagetools inspect "$REPO:src-$hash" --format '{{json .Manifest}}' |
	python3 -c 'import json, sys; print(json.load(sys.stdin)["digest"])')"
image="$REPO@$digest"
if ! helper pin "$image"; then
	die "published $image, but it cannot be pinned yet.
Resolve the verification error printed above. If the package is private, make the package public
(https://github.com/orgs/else-bot/packages/container/else-guest-base/settings, Danger Zone > Change visibility).
Then retry verification with: scripts/build-guest-base.sh --skip-build"
fi
cat <<EOF

Pinned $image in infra/guest-base.json.
Next steps (nothing has been committed or pushed):
  git add infra/guest-base.json
  git commit -m "chore(guest): publish guest base ${digest#sha256:}"
  Open a pull request to dev with that commit. Release CI then builds the guest image from this base.
EOF
