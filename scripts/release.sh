#!/usr/bin/env bash
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
echo "Fetching main from $remote..."
git fetch --no-tags "$remote" "+refs/heads/main:refs/remotes/$remote/main" ||
	die "cannot fetch main from $remote"

workspace_version() {
	git show "$1:Cargo.toml" | sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' | head -1
}
current="$(workspace_version "$remote/main")"
[[ "$current" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] || die "cannot read the version in main's Cargo.toml (found '$current')"
major="${BASH_REMATCH[1]}" minor="${BASH_REMATCH[2]}" patch="${BASH_REMATCH[3]}"
case "$bump" in
patch) version="$major.$minor.$((patch + 1))" ;;
minor) version="$major.$((minor + 1)).0" ;;
major) version="$((major + 1)).0.0" ;;
esac
tag="v$version"
branch="release/$tag"
remote_tag="$(git ls-remote --tags "$remote" "refs/tags/$tag")" ||
	die "cannot check tag $tag on $remote"
[ -z "$remote_tag" ] || die "tag $tag already exists on $remote"

ensure_pr() {
	local url body
	echo "Looking up the pull request for $branch..."
	url="$(gh pr list --base main --head "$branch" --state open --json url --jq '.[0].url // empty')" ||
		die "gh cannot list pull requests; run gh auth login"
	if [ -z "$url" ]; then
		echo "Opening the release pull request..."
		body="$(mktemp)"
		cat >"$body" <<EOF
Releases $tag from main.

CI pins the guest image and reports release-ready. Squash and merge as chore(release): $tag.
CI then builds desktop and API binaries for Linux AMD64, Linux ARM64, and macOS ARM64 and verifies the pinned image.
It publishes the tag and GitHub release with archives and checksums without adding commits to main.
EOF
		if ! url="$(gh pr create --base main --head "$branch" --title "chore(release): $tag" --body-file "$body")"; then
			rm -f "$body"
			die "gh cannot open the pull request; the branch $branch is pushed, so run this again"
		fi
		rm -f "$body"
	fi
	echo "Release pull request: $url"
}

if git ls-remote --exit-code -q --heads "$remote" "$branch" >/dev/null; then
	echo "$branch already exists on $remote."
	ensure_pr
	exit 0
fi

echo "Cut $branch ($bump, was $current) from $remote/main $(git rev-parse --short "$remote/main"), push it, and open a pull request to main."
if [ "$yes" = 0 ]; then
	[ -t 0 ] || die "no terminal to confirm; pass -y"
	printf 'Continue? [y/N] '
	read -r answer
	case "$answer" in y | Y | yes) ;; *) echo "Nothing changed."; exit 1 ;; esac
fi

work="$(mktemp -d)"
trap 'git worktree remove --force "$work" >/dev/null 2>&1 || rm -rf "$work"' EXIT
echo "Preparing $branch in a temporary worktree..."
git worktree add -q --detach "$work" "$remote/main"
(
	cd "$work"
	echo "Bumping workspace and lockfile versions to $version..."
	sed -i.bak "/^\[workspace.package\]/,/^\[/s/^version = \".*\"/version = \"$version\"/" Cargo.toml
	rm -f Cargo.toml.bak
	python3 - "$current" "$version" <<'PY'
from pathlib import Path
import re
import sys

current, version = sys.argv[1:]
lock = Path("Cargo.lock")
contents = lock.read_text()
for name in ("else-api", "else-desktop"):
    contents, count = re.subn(
        rf'(?m)^(name = "{name}"\nversion = )"{re.escape(current)}"$',
        lambda match: match[1] + f'"{version}"', contents)
    if count != 1:
        sys.exit(f"release: expected one {name} {current} entry in Cargo.lock, found {count}")
lock.write_text(contents)
PY
	[ "$(git diff --name-only | sort | tr '\n' ' ')" = "Cargo.lock Cargo.toml " ] ||
		die "unexpected changes after the bump: $(git diff --name-only | tr '\n' ' ')"
	echo "Committing $tag (Git hooks may run)..."
	git commit -am "chore(release): $tag"
	echo "Pushing $branch to $remote..."
	git push "$remote" "HEAD:refs/heads/$branch" || die "cannot push $branch to $remote"
)
ensure_pr
