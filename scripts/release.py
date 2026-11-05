#!/usr/bin/env python3
"""Plan and publish native binary releases after verifying the pinned guest image."""
import argparse
import json
from pathlib import Path
import subprocess
import sys

from guest_release import (Fail, GUEST, VERSION, cargo_version, cmd_check, git, git_out,
                           is_ancestor, remote_tags, version_key)
from release_binaries import BINARIES, PLATFORMS as BINARY_PLATFORMS, archive_name, release_assets


def release_context(remote, main):
    head = git_out("rev-parse", "HEAD")
    git("fetch", "-q", remote, f"+refs/heads/{main}:refs/remotes/{remote}/{main}")
    if not is_ancestor(head, f"{remote}/{main}"):
        raise Fail(f"HEAD {head} is not on {remote}/{main}")
    version = cargo_version(head)
    tag = f"v{version}"
    tags = remote_tags(remote)
    if tag in tags:
        if not is_ancestor(tags[tag], head):
            raise Fail(f"{tag} already exists at {tags[tag]}, which is not on {main}")
    else:
        newer = [t for t in tags if version_key(t[1:]) >= version_key(version)]
        if newer:
            raise Fail(f"{tag} is not newer than the existing {max(newer, key=lambda t: version_key(t[1:]))}")
    return head, version, tag, tags


def github_release(tag=None):
    target = [tag] if tag is not None else []
    result = subprocess.run(["gh", "release", "view", *target, "--json", "isDraft,assets,tagName"], capture_output=True, text=True)
    if result.returncode:
        if "release not found" in result.stderr.lower() or "HTTP 404" in result.stderr:
            return None
        raise Fail(f"cannot look up release {tag or 'latest'}: {result.stderr.strip()}")
    return json.loads(result.stdout)


def complete_release(release, version):
    expected = {archive_name(binary, version, platform) for platform in BINARY_PLATFORMS for binary in BINARIES}
    expected.add("SHA256SUMS")
    return (release is not None and not release["isDraft"]
            and expected <= {asset["name"] for asset in release["assets"]})


def cmd_release_plan(remote, main):
    head, version, tag, tags = release_context(remote, main)
    if tag in tags and (tags[tag] != head or complete_release(github_release(tag), version)):
        print("state=skip")
    else:
        print("state=build")


def cmd_finalize(remote, main, assets_dir):
    head, version, tag, tags = release_context(remote, main)
    if tag in tags and tags[tag] != head:
        print(f"{tag} is already released at an earlier commit; nothing to publish")
        return
    assets = release_assets(version, assets_dir)
    if tag not in tags:
        data = cmd_check(GUEST, head, allow_unpinned=False)
        git("tag", "-f", "-a", tag, "-m", f"else.bot {version}\n\nGuest image: {data['image']}", head)
        git("push", "-q", remote, f"refs/tags/{tag}")
        print(f"tagged {tag} at {head} with guest image {data['image']}")
    release = github_release(tag)
    if complete_release(release, version):
        print(f"{tag} already has all release binaries")
        return
    if release is None:
        command = ["gh", "release", "create", tag, *map(str, assets), "--draft", "--verify-tag",
                   "--title", f"else.bot {version}", "--generate-notes"]
    else:
        command = ["gh", "release", "upload", tag, *map(str, assets), "--clobber"]
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode:
        raise Fail(f"tag {tag} is ready, but uploading its release binaries failed: {result.stderr.strip()}")
    latest = github_release()
    promote = True
    if latest is not None:
        latest_version = latest["tagName"].removeprefix("v")
        if not VERSION.fullmatch(latest_version):
            raise Fail(f"cannot compare the latest release tag {latest['tagName']} with {tag}")
        promote = version_key(version) >= version_key(latest_version)
    result = subprocess.run(["gh", "release", "edit", tag, "--draft=false",
                             "--latest" if promote else "--latest=false"], capture_output=True, text=True)
    if result.returncode:
        raise Fail(f"tag {tag} has binaries, but publishing its GitHub release failed: {result.stderr.strip()}")
    print(f"published the GitHub release for {tag} with all release binaries")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    for command in ("plan", "finalize"):
        p = sub.add_parser(command)
        p.add_argument("--remote", default="origin")
        p.add_argument("--main", default="main")
        if command == "finalize":
            p.add_argument("--assets-dir", required=True, type=Path)
    args = parser.parse_args()
    try:
        if args.command == "plan":
            cmd_release_plan(args.remote, args.main)
        else:
            cmd_finalize(args.remote, args.main, args.assets_dir)
    except (Fail, ValueError) as error:
        print(f"release: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
