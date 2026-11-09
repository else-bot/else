#!/usr/bin/env python3
"""Plan, prepare, and publish native binary releases from a commit on main.

A release never adds commits to main. The release workflow picks a commit
on main, and `prepare` adds one release commit on top of it that bumps the
workspace version and pins the verified guest image. That commit is pushed
to a candidate branch while it is built and tested, and `finalize` tags it
and publishes its binaries, and tags the main commit it was cut from as
main/vX.Y.Z so builds from main report that version. Main keeps the version
it had, so the next version comes from the newest release tag.

    release.py plan --bump patch|minor|major [--sha SHA]
    release.py prepare --version X.Y.Z --image IMAGE [--push]
    release.py finalize --version X.Y.Z --rev REV --assets-dir DIR
"""
import argparse
import json
from pathlib import Path
import re
import subprocess
import sys

from guest_release import (Fail, GUEST, VERSION, cargo_version, cmd_check, cmd_plan as guest_plan, git, git_out,
                           is_ancestor, parse_manifest, remote_tags, source_sha256, verify, version_key, write_manifest)
from release_binaries import BINARIES, PLATFORMS as BINARY_PLATFORMS, archive_name, release_assets

BUMPS = ("patch", "minor", "major")
RELEASE_FILES = ("Cargo.lock", "Cargo.toml", GUEST.manifest)


def candidate_branch(version):
    return f"release/v{version}"


def bumped(version, part):
    major, minor, patch = version_key(version)
    if part == "major":
        return f"{major + 1}.0.0"
    if part == "minor":
        return f"{major}.{minor + 1}.0"
    return f"{major}.{minor}.{patch + 1}"


def fetch_main(remote, main):
    git("fetch", "-q", "--tags", "--force", remote, f"+refs/heads/{main}:refs/remotes/{remote}/{main}")
    return f"{remote}/{main}"


def newest(tags):
    return max(tags, key=lambda t: version_key(t[1:])) if tags else None


def require_release_commit(rev, main_ref):
    """REV is on main, or it is one commit on top of main whose Cargo files
    differ from its parent only by the version bump and whose only other
    change is the guest pin, so a release ships reviewed code."""
    if is_ancestor(rev, main_ref):
        return
    commit, *parents = git_out("rev-list", "--parents", "-n", "1", rev).split()
    if len(parents) != 1 or not is_ancestor(parents[0], main_ref):
        raise Fail(f"{commit} is neither on {main_ref} nor a release commit on top of it")
    parent = parents[0]
    changed = set(git_out("diff-tree", "--no-commit-id", "--name-only", "-r", parent, commit).splitlines())
    extra = sorted(changed - set(RELEASE_FILES))
    if extra:
        raise Fail(f"release commit {commit} changes more than the version and guest pin: {', '.join(extra)}")
    current, version = cargo_version(parent), cargo_version(commit)
    if version_key(version) <= version_key(current):
        raise Fail(f"release commit {commit} does not raise the version above {current}")
    show = lambda r, path: git("show", f"{r}:{path}").stdout.decode()
    expected = {"Cargo.toml": bump_manifest(show(parent, "Cargo.toml"), version),
                "Cargo.lock": bump_lock(show(parent, "Cargo.lock"), current, version)}
    for path, text in expected.items():
        if show(commit, path) != text:
            raise Fail(f"release commit {commit} changes {path} beyond the version bump")
    parse_manifest(GUEST, show(commit, GUEST.manifest), f"{GUEST.manifest} at {commit}")


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


def cmd_release_plan(remote, main, bump, sha=None):
    """Print the workflow outputs: a new release from SHA, or the resumption
    of the newest tag when its binaries were never fully published."""
    main_ref = fetch_main(remote, main)
    tags = remote_tags(remote)
    latest = newest(tags)
    if latest and not complete_release(github_release(latest), latest[1:]):
        rev = tags[latest]
        require_release_commit(rev, main_ref)
        print(f"{latest} is tagged but its binaries are not all published; finishing it instead "
              f"of cutting a new release. Run the release again afterwards.", file=sys.stderr)
        return {"state": "resume", "version": latest[1:], "sha": rev, "candidate": rev,
                "guest_state": "pinned", "guest_hash": "", "guest_image": ""}
    sha = git_out("rev-parse", "--verify", f"{sha or main_ref}^{{commit}}")
    if not is_ancestor(sha, main_ref):
        raise Fail(f"{sha} is not on {main_ref}")
    base = cargo_version(sha)
    if latest and version_key(latest[1:]) > version_key(base):
        base = latest[1:]
    version = bumped(base, bump)
    guest_state, guest_hash, guest_image = guest_plan(sha, None)
    return {"state": "release", "version": version, "sha": sha, "candidate": "",
            "guest_state": guest_state, "guest_hash": guest_hash, "guest_image": guest_image}


def bump_manifest(text, version):
    text, count = re.subn(r'(?ms)^(\[workspace\.package\]\n(?:(?!^\[).)*?^version = )"[^"]*"',
                          lambda m: m[1] + f'"{version}"', text, count=1)
    if count != 1:
        raise Fail("cannot find the [workspace.package] version in Cargo.toml")
    return text


def bump_lock(text, current, version):
    """Bump every workspace package in Cargo.lock: the entries at CURRENT
    without a `source`, so crates added to the workspace need no list."""
    text, count = re.subn(rf'(?m)^(name = "[^"]+"\nversion = )"{re.escape(current)}"$(?!\nsource = )',
                          lambda m: m[1] + f'"{version}"', text)
    if not count:
        raise Fail(f"expected workspace packages at {current} in Cargo.lock, found none")
    return text


def bump_files(root, current, version):
    for name, bump in (("Cargo.toml", lambda t: bump_manifest(t, version)),
                       ("Cargo.lock", lambda t: bump_lock(t, current, version))):
        path = root / name
        path.write_text(bump(path.read_text()))


def cmd_prepare(version, image, remote=None):
    """Commit the version bump and guest pin on top of HEAD and print it."""
    root = Path(git_out("rev-parse", "--show-toplevel"))
    if git_out("status", "--porcelain", "--untracked-files=no"):
        raise Fail("the checkout has uncommitted changes")
    current = cargo_version("HEAD")
    if version_key(version) <= version_key(current):
        raise Fail(f"version {version} is not newer than {current} at HEAD")
    source = source_sha256(GUEST, "HEAD")
    verify(GUEST, image, source)
    bump_files(root, current, version)
    write_manifest(GUEST, source, image)
    git("add", "--", *RELEASE_FILES)
    git("commit", "-q", "-m", f"chore(release): v{version}", "-m", f"Guest image: {image}")
    head = git_out("rev-parse", "HEAD")
    if remote:
        git("push", "-q", "--force", remote, f"{head}:refs/heads/{candidate_branch(version)}")
    return head


def cmd_finalize(remote, main, version, rev, assets_dir):
    main_ref = fetch_main(remote, main)
    rev = git_out("rev-parse", "--verify", f"{rev}^{{commit}}")
    if cargo_version(rev) != version:
        raise Fail(f"{rev} carries version {cargo_version(rev)}, not {version}")
    require_release_commit(rev, main_ref)
    tag = f"v{version}"
    tags = remote_tags(remote)
    if tag in tags and tags[tag] != rev:
        raise Fail(f"{tag} already exists at {tags[tag]}, not {rev}")
    assets = release_assets(version, assets_dir)
    if tag not in tags:
        newer = [t for t in tags if version_key(t[1:]) >= version_key(version)]
        if newer:
            raise Fail(f"{tag} is not newer than the existing {newest(newer)}")
        data = cmd_check(GUEST, rev, allow_unpinned=False)
        git("tag", "-f", "-a", tag, "-m", f"else.bot {version}\n\nGuest image: {data['image']}", rev)
        git("push", "-q", remote, f"refs/tags/{tag}")
        print(f"tagged {tag} at {rev} with guest image {data['image']}")
    tag_release_base(remote, main_ref, tag, rev)
    release = github_release(tag)
    if not complete_release(release, version):
        publish(tag, version, release, assets)
    else:
        print(f"{tag} already has all release binaries")
    branch = candidate_branch(version)
    listed = git_out("ls-remote", "--heads", remote, f"refs/heads/{branch}").split()
    if listed and listed[0] == rev:
        git("push", "-q", remote, "--delete", f"refs/heads/{branch}")


def tag_release_base(remote, main_ref, tag, rev):
    """Tag the main commit TAG was cut from as main/TAG. Fetches follow tags
    into the fetched history only, so this is the tag a checkout of main
    receives, and builds read their version from it."""
    base = rev if is_ancestor(rev, main_ref) else git_out("rev-parse", f"{rev}^")
    ref = f"refs/tags/main/{tag}"
    listed = git_out("ls-remote", remote, ref).split()
    if not listed:
        git("push", "-q", remote, f"{base}:{ref}")
    elif listed[0] != base:
        raise Fail(f"main/{tag} already exists at {listed[0]}, not {base}")


def publish(tag, version, release, assets):
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
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("plan")
    p.add_argument("--bump", required=True, choices=BUMPS)
    p.add_argument("--sha")
    p = sub.add_parser("prepare")
    p.add_argument("--version", required=True)
    p.add_argument("--image", required=True)
    p.add_argument("--push", action="store_true", help="push the release commit to its candidate branch")
    p = sub.add_parser("finalize")
    p.add_argument("--version", required=True)
    p.add_argument("--rev", required=True)
    p.add_argument("--assets-dir", required=True, type=Path)
    for p in sub.choices.values():
        p.add_argument("--remote", default="origin")
        p.add_argument("--main", default="main")
    args = parser.parse_args()
    try:
        if getattr(args, "version", None) and not VERSION.fullmatch(args.version):
            raise Fail(f"{args.version} is not X.Y.Z")
        if args.command == "plan":
            for key, value in cmd_release_plan(args.remote, args.main, args.bump, args.sha).items():
                print(f"{key}={value}")
        elif args.command == "prepare":
            print(f"candidate={cmd_prepare(args.version, args.image, args.remote if args.push else None)}")
        else:
            cmd_finalize(args.remote, args.main, args.version, args.rev, args.assets_dir)
    except (Fail, ValueError) as error:
        print(f"release: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
