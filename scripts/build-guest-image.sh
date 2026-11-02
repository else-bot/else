#!/usr/bin/env bash
# Build the else.bot guest image and load it into the API's private
# Microsandbox home. Usage: scripts/build-guest-image.sh [DATA_DIR]
# DATA_DIR defaults to $ELSE_DATA_DIR or ~/.local/share/else-bot.
set -euo pipefail
cd "$(dirname "$0")/.."
TAG="else-guest:1"
DATA_DIR="${1:-${ELSE_DATA_DIR:-$HOME/.local/share/else-bot}}"
MSB="${ELSE_MSB_BIN:-$(command -v msb || echo "$HOME/.microsandbox/bin/msb")}"
docker build --pull=false -t "$TAG" infra/guest
ARCHIVE="$(mktemp -d)/else-guest.tar"
docker save "$TAG" -o "$ARCHIVE"
mkdir -p "$DATA_DIR/microsandbox"
MSB_HOME="$DATA_DIR/microsandbox" "$MSB" load --input "$ARCHIVE" --tag "$TAG"
rm -f "$ARCHIVE"
echo "Image ID: $(docker image inspect --format '{{.Id}}' "$TAG")"
echo "Loaded $TAG into $DATA_DIR/microsandbox (set ELSE_VM_IMAGE=$TAG)."
