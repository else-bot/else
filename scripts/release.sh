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
git fetch -q --tags "$remote" "+refs/heads/main:refs/remotes/$remote/main" ||
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
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "tag $tag already exists"

ensure_pr() {
	local url body
	url="$(gh pr list --base main --head "$branch" --state open --json url --jq '.[0].url // empty')" ||
		die "gh cannot list pull requests; run gh auth login"
	if [ -z "$url" ]; then
		body="$(mktemp)"
		cat >"$body" <<EOF
Releases $tag from main.

CI pins the guest image and reports release-ready. Squash and merge as chore(release): $tag.
CI then verifies the pinned image, tags $tag, and publishes the GitHub release without adding commits to main.
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
git worktree add -q --detach "$work" "$remote/main"
(
	cd "$work"
	sed -i.bak "/^\[workspace.package\]/,/^\[/s/^version = \".*\"/version = \"$version\"/" Cargo.toml
	rm -f Cargo.toml.bak
	cargo update --workspace -q
	[ "$(git diff --name-only | sort | tr '\n' ' ')" = "Cargo.lock Cargo.toml " ] ||
		die "unexpected changes after the bump: $(git diff --name-only | tr '\n' ' ')"
	git commit -q -am "chore(release): $tag"
	git push -q "$remote" "HEAD:refs/heads/$branch" || die "cannot push $branch to $remote"
)
ensure_pr
