#!/usr/bin/env bash
# Release else.bot: bump the version, commit, tag, and publish.
#
#   scripts/release.sh [-y] patch|minor|major
#
# From a clean `main` that matches origin/main, it bumps the workspace
# version in Cargo.toml (0.0.1 -> patch 0.0.2, minor 0.1.0, major 1.0.0),
# refreshes Cargo.lock, commits `chore(release): vX.Y.Z`, tags vX.Y.Z
# (annotated), and pushes main and the tag together (atomically: both or
# neither). It asks before pushing; -y does not ask. Installations pick the
# release up on their next `elsebot update`.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() { echo "usage: scripts/release.sh [-y] patch|minor|major" >&2; exit 2; }
die() { echo "release: $1" >&2; exit 1; }

yes=0
bump=""
for arg in "$@"; do
	case "$arg" in
	-y | --yes) yes=1 ;;
	patch | minor | major) [ -z "$bump" ] || usage; bump="$arg" ;;
	*) usage ;;
	esac
done
[ -n "$bump" ] || usage

remote=origin
branch=main
[ "$(git rev-parse --abbrev-ref HEAD)" = "$branch" ] || die "check out $branch first"
[ -z "$(git status --porcelain)" ] || die "the working tree has changes; commit or stash them first"
git fetch -q --tags "$remote" "$branch" || die "cannot fetch $remote/$branch"
[ "$(git rev-parse HEAD)" = "$(git rev-parse "$remote/$branch")" ] ||
	die "$branch differs from $remote/$branch; pull or push first"

current="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
[[ "$current" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] || die "cannot read the version in Cargo.toml (found '$current')"
major="${BASH_REMATCH[1]}" minor="${BASH_REMATCH[2]}" patch="${BASH_REMATCH[3]}"
case "$bump" in
patch) version="$major.$minor.$((patch + 1))" ;;
minor) version="$major.$((minor + 1)).0" ;;
major) version="$((major + 1)).0.0" ;;
esac
tag="v$version"
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "tag $tag already exists"

echo "Release $tag ($bump, was $current) from $(git rev-parse --short HEAD) and push $branch and $tag to $remote."
if [ "$yes" = 0 ]; then
	[ -t 0 ] || die "no terminal to confirm; pass -y"
	printf 'Continue? [y/N] '
	read -r answer
	case "$answer" in y | Y | yes) ;; *) echo "Nothing changed."; exit 1 ;; esac
fi

sed -i.bak "/^\[workspace.package\]/,/^\[/s/^version = \".*\"/version = \"$version\"/" Cargo.toml
rm -f Cargo.toml.bak
cargo update --workspace --offline -q
[ "$(git diff --name-only | sort | tr '\n' ' ')" = "Cargo.lock Cargo.toml " ] ||
	die "unexpected changes after the bump: $(git diff --name-only | tr '\n' ' ')"
git add Cargo.toml Cargo.lock
git commit -q -m "chore(release): $tag"
git tag -a "$tag" -m "else.bot $version"
if ! git push -q --atomic "$remote" "$branch" "$tag"; then
	echo "release: the push failed; the commit and tag are local. Push them with: git push --atomic $remote $branch $tag" >&2
	exit 1
fi
echo "Released $tag ($(git rev-parse --short HEAD))."
