#!/usr/bin/env bash
# Build the else.bot guest image and load it into the API's private
# Microsandbox home. Usage: scripts/build-guest-image.sh [DATA_DIR]
# DATA_DIR defaults to $ELSE_DATA_DIR or ~/.else. The exact image this run
# built is exported by ID, so concurrent builds never trade a shared tag.
set -euo pipefail
cd "$(dirname "$0")/.."
TAG="${ELSE_GUEST_TAG:-else-guest:3}"
BASE="${ELSE_GUEST_BASE:-$(python3 scripts/guest-release.py --base pinned)}"
DATA_DIR="${1:-${ELSE_DATA_DIR:-$HOME/.else}}"
MSB="${ELSE_MSB_BIN:-$(command -v msb || echo "$HOME/.microsandbox/bin/msb")}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
docker build --pull=false --build-arg BASE="$BASE" --build-arg IMAGE_TAG="$TAG" --iidfile "$work/image.id" infra/guest
image="$(cat "$work/image.id")"
docker save "$image" -o "$work/else-guest.tar"
mkdir -p "$DATA_DIR/microsandbox"
MSB_HOME="$DATA_DIR/microsandbox" "$MSB" load --input "$work/else-guest.tar" --tag "$TAG"
echo "Image ID: $image"
echo "Loaded $TAG into $DATA_DIR/microsandbox (set ELSE_VM_IMAGE=$TAG)."
