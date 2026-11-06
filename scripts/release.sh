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
title="chore(release): $tag"
description="$(cat <<EOF
Releases $tag from main.

Bumps the workspace version from $current to $version and pins the verified guest image.
CI builds desktop and API binaries for Linux AMD64, Linux ARM64, and macOS ARM64.
It publishes the tag and GitHub release with archives and checksums without adding commits to main.
EOF
)"
remote_tag="$(git ls-remote --tags "$remote" "refs/tags/$tag")" ||
	die "cannot check tag $tag on $remote"
[ -z "$remote_tag" ] || die "tag $tag already exists on $remote"

ensure_pr() {
	echo "Looking up the pull request for $branch..."
	pr_url="$(gh pr list --base main --head "$branch" --state open --json url --jq '.[0].url // empty')" ||
		die "gh cannot list pull requests; run gh auth login"
	if [ -z "$pr_url" ]; then
		echo "Opening the release pull request..."
		if ! pr_url="$(gh pr create --base main --head "$branch" --title "$title" --body-file - <<<"$description")"; then
			die "gh cannot open the pull request; the branch $branch is pushed, so run this again"
		fi
	fi
	echo "Release pull request: $pr_url"
}

merge_pr() {
	local snapshot head state
	echo "Waiting for release-ready and cargo-test on $pr_url..."
	while :; do
		snapshot="$(gh pr view "$pr_url" --json headRefOid,statusCheckRollup --jq '
			[.headRefOid, ((.statusCheckRollup // [])
			| map({name: (.name // .context), state: (.conclusion // .state)})
			| . as $checks
			| map(select(.name == "release-ready" or .name == "cargo-test"))
			| if any($checks[]; .state == "FAILURE" or .state == "ERROR" or .state == "CANCELLED"
				or .state == "TIMED_OUT" or .state == "ACTION_REQUIRED" or .state == "STARTUP_FAILURE")
				or any(.[]; .state == "SKIPPED" or .state == "NEUTRAL" or .state == "STALE") then "failed"
			  elif any(.[]; .name == "release-ready" and .state == "SUCCESS")
				and any(.[]; .name == "cargo-test" and .state == "SUCCESS")
				and all(.[]; .state == "SUCCESS") then "ready"
			  else "pending" end)] | @tsv')" || die "cannot read CI checks for $pr_url"
		IFS=$'\t' read -r head state <<<"$snapshot"
		case "$state" in
		ready) break ;;
		failed) die "CI checks failed for $pr_url; fix them and run this again" ;;
		pending) sleep 10 ;;
		*) die "cannot read CI checks for $pr_url" ;;
		esac
	done
	echo "Squash-merging $pr_url as $title..."
	gh pr merge "$pr_url" --squash --match-head-commit "$head" --subject "$title" \
		--body-file - <<<"$description" || die "cannot merge $pr_url; check the PR and run this again"
	echo "Merged $tag. CI will publish the release binaries."
}

if git ls-remote --exit-code -q --heads "$remote" "$branch" >/dev/null; then
	echo "$branch already exists on $remote."
	ensure_pr
	merge_pr
	exit 0
fi

echo "Cut $branch ($bump, was $current) from $remote/main $(git rev-parse --short "$remote/main"), push it, open a pull request to main, wait for CI, and squash-merge it."
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
for name in ("else-api", "else-desktop", "else-tailnet"):
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
	git commit -am "$title"
	echo "Pushing $branch to $remote..."
	git push "$remote" "HEAD:refs/heads/$branch" || die "cannot push $branch to $remote"
)
ensure_pr
merge_pr
