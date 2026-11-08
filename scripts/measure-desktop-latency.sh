#!/usr/bin/env bash
# Measure takeover latency against a real guest desktop in Docker: how long a
# pointer move takes to reach the viewer's frames, and frames per second while
# it moves. Writes the JSON report to the absolute path in $1.
set -euo pipefail
cd "$(dirname "$0")/.."
out="${1:-$PWD/desktop-latency.json}"
name="else-desktop-latency-$$"
port="${ELSE_DESKTOP_LATENCY_PORT:-46080}"
password="latency1"
base="${ELSE_GUEST_BASE:-$(python3 scripts/guest_release.py --base pinned)}"
docker build --pull=false --build-arg BASE="$base" --build-arg IMAGE_TAG=else-guest:latency -t else-guest:latency -q infra/guest >/dev/null
trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT
docker run -d --name "$name" -p "127.0.0.1:$port:6080" --entrypoint sleep else-guest:latency infinity >/dev/null
docker exec "$name" sh -c "printf '%s\n' '$password' > /etc/elsebot/vnc.pass"
# Chromium cannot start in plain Docker; the screen and VNC servers can.
docker exec "$name" /usr/local/bin/else-desktop >/dev/null 2>&1 || true
docker exec "$name" supervisorctl -c /etc/elsebot/supervisord.conf status x11vnc x11vnc-watch novnc
ELSE_DESKTOP_LATENCY_GUEST="127.0.0.1:$port" \
ELSE_DESKTOP_LATENCY_PASSWORD="$password" \
ELSE_DESKTOP_LATENCY_OUT="$out" \
    cargo test --release --locked -p else-api --test desktop_latency_live -- --nocapture
echo "Report: $out"
