#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
exec cargo clippy --workspace --all-targets --locked -- -D warnings
