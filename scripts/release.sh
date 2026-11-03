#!/usr/bin/env bash
# Prepare an else.bot release: set the workspace version, refresh
# Cargo.lock, commit `chore: release vX.Y.Z`, and tag it vX.Y.Z (annotated).
# Nothing is pushed; the command to publish it is printed at the end.
#
#   scripts/release.sh 0.0.2
#
# Installations update to the newest vX.Y.Z tag on the default remote.
set -euo pipefail
cd "$(dirname "$0")/.."

version="${1:-}"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || {
	echo "usage: scripts/release.sh X.Y.Z" >&2
	exit 2
}
[ -z "$(git status --porcelain)" ] || { echo "the working tree has changes; commit or stash them first" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/v$version" >/dev/null && { echo "tag v$version already exists" >&2; exit 1; }

current="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
newest="$(printf '%s\n%s\n' "$current" "$version" | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)"
if [ "$version" != "$current" ] && [ "$newest" != "$version" ]; then
	echo "v$version is older than the current version $current" >&2
	exit 1
fi

if [ "$version" != "$current" ]; then
	sed -i.bak "/^\[workspace.package\]/,/^\[/s/^version = \".*\"/version = \"$version\"/" Cargo.toml
	rm -f Cargo.toml.bak
	cargo update --workspace --offline -q
	git add Cargo.toml Cargo.lock
	git commit -q -m "chore: release v$version"
fi
git tag -a "v$version" -m "else.bot $version"
echo "Tagged v$version at $(git rev-parse --short HEAD) (was $current)."
echo "Publish it with: git push origin HEAD v$version"
