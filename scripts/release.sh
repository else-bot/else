#!/usr/bin/env bash
# Starts the release workflow for the head of main (or --sha). The workflow
# picks the next version from the newest release tag, so nothing is pushed
# from this checkout and main keeps moving while the release runs.
set -euo pipefail
cd "$(dirname "$0")/.."

usage() { echo "usage: scripts/release.sh [-y] [--wait] [--sha SHA] patch|minor|major" >&2; exit 2; }
die() { echo "release: $1" >&2; exit 1; }

yes=0
wait=0
bump=""
sha=""
while [ $# -gt 0 ]; do
	case "$1" in
	-y | --yes) yes=1 ;;
	--wait) wait=1 ;;
	--sha) [ $# -ge 2 ] || usage; sha="$2"; shift ;;
	patch | minor | major) [ -z "$bump" ] || usage; bump="$1" ;;
	*) usage ;;
	esac
	shift
done
[ -n "$bump" ] || usage

remote=origin
echo "Fetching main from $remote..."
git fetch -q --no-tags "$remote" "+refs/heads/main:refs/remotes/$remote/main" ||
	die "cannot fetch main from $remote"
if [ -n "$sha" ]; then
	sha="$(git rev-parse -q --verify "$sha^{commit}")" || die "unknown commit $sha"
	git merge-base --is-ancestor "$sha" "$remote/main" || die "$sha is not on $remote/main"
	target="$sha"
else
	target="$(git rev-parse "$remote/main")"
fi

echo "Release $(git log -1 --format='%h %s' "$target") as the next $bump version."
if [ "$yes" = 0 ]; then
	[ -t 0 ] || die "no terminal to confirm; pass -y"
	printf 'Continue? [y/N] '
	read -r answer
	case "$answer" in y | Y | yes) ;; *) echo "Nothing started."; exit 1 ;; esac
fi

request="$(date -u +%Y%m%dT%H%M%SZ)-$$-$RANDOM"
gh workflow run release.yml --ref main -f bump="$bump" -f sha="$target" -f request="$request" >/dev/null ||
	die "gh cannot start the release workflow; run gh auth login"
echo "Started the release workflow; looking up its run..."
run=""
for _ in $(seq 30); do
	run="$(gh run list --workflow release.yml --event workflow_dispatch --limit 20 \
		--json databaseId,displayTitle --jq ".[] | select(.displayTitle | endswith(\"[$request]\")) | .databaseId" |
		head -1)" || die "cannot list release workflow runs"
	[ -n "$run" ] && break
	sleep 2
done
[ -n "$run" ] || die "the release workflow started, but its run did not appear; check the Actions tab"
url="$(gh run view "$run" --json url --jq .url)" || die "cannot read release run $run"
echo "Release run: $url"
if [ "$wait" = 1 ]; then
	gh run watch "$run" --exit-status >/dev/null || die "the release failed; see $url"
	echo "Released. $url"
fi
