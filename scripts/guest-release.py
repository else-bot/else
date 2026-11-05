#!/usr/bin/env python3
"""Pin, verify, and release the prebuilt else.bot guest image.

The guest is built in two layers, each recorded by a manifest that names the
published image matching its build inputs at a commit:

    infra/guest-base.json   {"schema": 1, "source_sha256": "<64 hex>", "image": "ghcr.io/else-bot/else-guest-base@sha256:<64 hex>"}
    infra/guest-image.json  {"schema": 1, "source_sha256": "<64 hex>", "image": "ghcr.io/else-bot/else-guest@sha256:<64 hex>"}

The base holds the heavy packages and the patched x11vnc. An operator
publishes it once per change with scripts/build-guest-base.sh. The guest
image adds the helpers on top of the pinned base, and its inputs include the
base manifest, so release CI rebuilds it without touching APT. `image` is null
until one is published. --base selects the base manifest.

The release workflows in .github/workflows call this script;
scripts/test-release.py exercises it against disposable repositories and a
fake registry.

    guest-release.py [--base] hash [--rev REV]
    guest-release.py [--base] check [--rev REV] [--allow-unpinned]
    guest-release.py [--base] pinned [--rev REV]
    guest-release.py [--base] build-args
    guest-release.py [--base] pin IMAGE [--rev REV]
    guest-release.py plan [--rev REV] [--main-ref REF]
    guest-release.py platform-digest REF --platform OS/ARCH
    guest-release.py digest REF
    guest-release.py gate --branch BRANCH [--rev REV] [--main-ref REF]
    guest-release.py finalize [--remote NAME] [--main BRANCH] [--dev BRANCH]
"""

import argparse
import base64
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass

CONTEXT = "infra/guest"
PLATFORMS = ("linux/amd64", "linux/arm64")
SOURCE_URL = "https://github.com/else-bot/else"


@dataclass(frozen=True)
class Kind:
    name: str
    manifest: str
    repo: str
    label: str
    dockerfile: str
    inputs: tuple
    build_args: dict
    publisher: str
    parent: "Kind | None" = None

    def recipe(self):
        return {
            "version": 2,
            "kind": self.name,
            "context": CONTEXT,
            "dockerfile": self.dockerfile,
            "platforms": list(PLATFORMS),
            "build_args": self.build_args,
            "label": self.label,
        }


BASE = Kind(
    name="guest base",
    manifest="infra/guest-base.json",
    repo=os.environ.get("ELSE_GUEST_BASE_REPO", "ghcr.io/else-bot/else-guest-base"),
    label="bot.else.guest-base.source_sha256",
    dockerfile=f"{CONTEXT}/base.Dockerfile",
    inputs=(f"{CONTEXT}/base.Dockerfile", f"{CONTEXT}/apt-downloads.conf", f"{CONTEXT}/x11vnc/keymap-settle.patch"),
    build_args={},
    publisher="publish it once with scripts/build-guest-base.sh and commit the pin",
)
GUEST = Kind(
    name="guest image",
    manifest="infra/guest-image.json",
    repo=os.environ.get("ELSE_GUEST_IMAGE_REPO", "ghcr.io/else-bot/else-guest"),
    label="bot.else.guest.source_sha256",
    dockerfile=f"{CONTEXT}/Dockerfile",
    inputs=(CONTEXT, BASE.manifest, "scripts/guest-release.py", ".github/workflows/guest-image.yml"),
    build_args={"IMAGE_TAG": "else-guest:3"},
    publisher="the release workflow pins a new image",
    parent=BASE,
)
HEX64 = re.compile(r"^[0-9a-f]{64}$")
VERSION = re.compile(r"^([0-9]+)\.([0-9]+)\.([0-9]+)$")
INDEX_TYPES = (
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
)
IMAGE_TYPES = (
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
)


class Fail(Exception):
    pass


def git(*args, check=True, input=None):
    result = subprocess.run(["git", *args], capture_output=True, input=input)
    if check and result.returncode:
        raise Fail(f"git {' '.join(args)} failed: {result.stderr.decode().strip()}")
    return result


def git_out(*args):
    return git(*args).stdout.decode().strip()


def is_ancestor(a, b):
    return git("merge-base", "--is-ancestor", a, b, check=False).returncode == 0


def source_sha256(kind, rev="HEAD"):
    """Hash KIND's tracked build inputs and recipe at REV."""
    listing = git("ls-tree", "-r", "-z", "--full-tree", rev, "--", *kind.inputs).stdout
    entries = []
    for item in filter(None, listing.split(b"\0")):
        meta, path = item.split(b"\t", 1)
        mode, type_, oid = meta.decode().split(" ")
        path = path.decode()
        if type_ != "blob":
            raise Fail(f"{path} is a {type_}; guest build inputs must be files")
        name = path.rsplit("/", 1)[-1]
        if name.startswith("test_") and name.endswith(".py"):
            continue
        content = git("cat-file", "blob", oid).stdout
        entries.append((path, mode, hashlib.sha256(content).hexdigest()))
    if not entries:
        raise Fail(f"the {kind.name} build inputs have no files at {rev}")
    digest = hashlib.sha256()
    digest.update(b"else-guest-source\n")
    digest.update(json.dumps(kind.recipe(), sort_keys=True, separators=(",", ":")).encode() + b"\n")
    for path, mode, sha in sorted(entries, key=lambda e: e[0].encode()):
        digest.update(f"{mode} {sha} {path}\n".encode())
    return digest.hexdigest()


def image_pattern(kind):
    return re.compile("^" + re.escape(kind.repo) + r"@sha256:[0-9a-f]{64}$")


def parse_manifest(kind, text, where):
    try:
        data = json.loads(text)
    except json.JSONDecodeError as e:
        raise Fail(f"{where} is not JSON: {e}")
    if not isinstance(data, dict) or data.get("schema") != 1:
        raise Fail(f"{where} must be a JSON object with schema 1")
    if not isinstance(data.get("source_sha256"), str) or not HEX64.match(data["source_sha256"]):
        raise Fail(f"{where}: source_sha256 must be 64 lowercase hex characters")
    image = data.get("image", "missing")
    if image is not None and (not isinstance(image, str) or not image_pattern(kind).match(image)):
        raise Fail(f"{where}: image must be null or {kind.repo}@sha256:<64 hex>")
    return data


def manifest_at(kind, rev):
    shown = git("show", f"{rev}:{kind.manifest}", check=False)
    if shown.returncode:
        raise Fail(f"{kind.manifest} is missing at {rev}")
    return parse_manifest(kind, shown.stdout.decode(), f"{kind.manifest} at {rev}")


class NotFound(Fail):
    pass


def split_ref(ref):
    if "@" in ref:
        name, reference = ref.split("@", 1)
    else:
        name, _, reference = ref.rpartition(":")
        if not name or "/" in reference:
            raise Fail(f"{ref} names no tag or digest")
    host, _, repo = name.partition("/")
    return host, repo, reference


class Registry:
    def __init__(self, host, repo, credentials=None):
        local = host.split(":")[0] in ("127.0.0.1", "localhost")
        self.base = ("http://" if local else "https://") + host
        self.repo = repo
        self.credentials = credentials
        self.token = None

    def _open(self, url, accept, auth):
        headers = {"Accept": accept}
        if auth:
            headers["Authorization"] = auth
        request = urllib.request.Request(url, headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                return response.status, response.headers, response.read()
        except urllib.error.HTTPError as e:
            return e.code, e.headers, e.read()

    def _authenticate(self, challenge):
        fields = dict(re.findall(r'(\w+)="([^"]*)"', challenge or ""))
        if not (challenge or "").lower().startswith("bearer") or "realm" not in fields:
            raise Fail(f"{self.base} asked for unsupported authentication: {challenge}")
        query = {k: v for k, v in fields.items() if k in ("service", "scope")}
        headers = {}
        if self.credentials:
            user, password = self.credentials
            headers["Authorization"] = "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()
        url = fields["realm"] + "?" + urllib.parse.urlencode(query)
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=60) as r:
                body = json.loads(r.read())
        except urllib.error.HTTPError as e:
            if e.code in (401, 403, 404):
                raise NotFound(f"{self.repo}: registry token unavailable (HTTP {e.code})")
            raise Fail(f"cannot get a registry token from {fields['realm']}: {e}")
        except (urllib.error.URLError, ValueError) as e:
            raise Fail(f"cannot get a registry token from {fields['realm']}: {e}")
        self.token = body.get("token") or body.get("access_token")

    def get(self, kind, reference, accept):
        url = f"{self.base}/v2/{self.repo}/{kind}/{reference}"
        for attempt in range(4):
            try:
                auth = f"Bearer {self.token}" if self.token else None
                status, headers, body = self._open(url, accept, auth)
                if status == 401 and self.token is None:
                    self._authenticate(headers.get("WWW-Authenticate"))
                    status, headers, body = self._open(url, accept, f"Bearer {self.token}")
            except urllib.error.URLError as e:
                status, body = None, str(e).encode()
            if status == 200:
                return body
            if status in (401, 403, 404):
                raise NotFound(f"{self.repo} {kind}/{reference}: HTTP {status}")
            if attempt < 3:
                time.sleep(2**attempt)
        raise Fail(f"{url}: {status} {body[:200]!r}")

    def manifest(self, reference):
        return self.get("manifests", reference, ", ".join(INDEX_TYPES + IMAGE_TYPES))

    def blob(self, digest):
        return self.get("blobs", digest, "*/*")


def content(body, digest):
    if "sha256:" + hashlib.sha256(body).hexdigest() != digest:
        raise Fail(f"registry content does not match {digest}")
    return json.loads(body)


def verify(kind, image, source):
    """Confirm IMAGE is a public multi-platform index whose platform images
    were all built from KIND's inputs hashing to SOURCE. Returns
    {platform: manifest digest}."""
    if not image_pattern(kind).match(image):
        raise Fail(f"{image} is not {kind.repo}@sha256:<64 hex>")
    host, repo, digest = split_ref(image)
    registry = Registry(host, repo)
    try:
        index = content(registry.manifest(digest), digest)
        if index.get("mediaType") not in INDEX_TYPES:
            raise Fail(f"{image} is not a multi-platform index ({index.get('mediaType')})")
        found = {}
        for entry in index.get("manifests", []):
            platform = entry.get("platform", {})
            name = f"{platform.get('os')}/{platform.get('architecture')}"
            if name == "unknown/unknown":
                continue
            if name in found or name not in PLATFORMS:
                raise Fail(f"{image} has an unexpected or repeated platform {name}")
            found[name] = entry["digest"]
        if sorted(found) != sorted(PLATFORMS):
            raise Fail(f"{image} holds {sorted(found)}, not {list(PLATFORMS)}")
        for name, platform_digest in found.items():
            manifest = content(registry.manifest(platform_digest), platform_digest)
            config_digest = manifest["config"]["digest"]
            config = content(registry.blob(config_digest), config_digest)
            if f"{config.get('os')}/{config.get('architecture')}" != name:
                raise Fail(f"{image}: the {name} entry is a {config.get('os')}/{config.get('architecture')} image")
            labels = (config.get("config") or {}).get("Labels") or {}
            if labels.get(kind.label) != source:
                raise Fail(f"{image}: the {name} image was built from {labels.get(kind.label)}, not {source}")
    except NotFound as e:
        raise Fail(f"{image} cannot be read anonymously ({e}); the package must exist and be public")
    return found


def try_verify(image, source):
    try:
        verify(GUEST, image, source)
        return True
    except Fail as e:
        print(f"not reusing {image}: {e}", file=sys.stderr)
        return False


def cargo_version(rev):
    text = git("show", f"{rev}:Cargo.toml").stdout.decode()
    block = re.search(r"^\[workspace\.package\]\n(.*?)(?=^\[|\Z)", text, re.M | re.S)
    found = block and re.search(r'^version = "([^"]*)"', block.group(1), re.M)
    if not found or not VERSION.match(found.group(1)):
        raise Fail(f"cannot read [workspace.package] version in Cargo.toml at {rev}")
    return found.group(1)


def version_key(version):
    return tuple(int(x) for x in VERSION.match(version).groups())


def remote_tags(remote):
    out = git_out("ls-remote", "--tags", remote, "refs/tags/v*")
    tags = {}
    for line in out.splitlines():
        sha, ref = line.split("\t")
        name = ref.removeprefix("refs/tags/")
        if name.endswith("^{}"):
            tags[name[:-3]] = sha
        else:
            tags.setdefault(name, sha)
    return {k: v for k, v in tags.items() if VERSION.match(k[1:])}


def fresh(kind, rev, allow_unpinned=False):
    data = manifest_at(kind, rev)
    source = source_sha256(kind, rev)
    if data["source_sha256"] != source:
        raise Fail(
            f"{kind.manifest} records {kind.name} source {data['source_sha256']}, but the {kind.name} build inputs "
            f"at {rev} hash to {source}; {kind.publisher}"
        )
    if data["image"] is None and not allow_unpinned:
        raise Fail(f"{kind.manifest} has no published image yet; {kind.publisher}")
    return data


def cmd_check(kind, rev, allow_unpinned):
    if kind.parent:
        fresh(kind.parent, rev, allow_unpinned)
    data = fresh(kind, rev, allow_unpinned)
    if data["image"]:
        verify(kind, data["image"], data["source_sha256"])
    return data


def pinned(kind, rev="HEAD"):
    data = fresh(kind, rev)
    verify(kind, data["image"], data["source_sha256"])
    return data["image"]


def require_clean(kind):
    dirty = git_out("status", "--porcelain", "--untracked-files=all", "--", *kind.inputs)
    if dirty:
        raise Fail(f"the {kind.name} build inputs differ from HEAD; commit them first:\n{dirty}")


def build_args(kind):
    require_clean(kind)
    args = [f"--file={kind.dockerfile}"]
    if kind.parent:
        args.append(f"--build-arg=BASE={pinned(kind.parent)}")
    args += [f"--build-arg={key}={value}" for key, value in sorted(kind.build_args.items())]
    args += [f"--label={kind.label}={source_sha256(kind)}", f"--label=org.opencontainers.image.source={SOURCE_URL}"]
    return args


def cmd_plan(rev, main_ref):
    pinned(BASE, rev)
    source = source_sha256(GUEST, rev)
    data = manifest_at(GUEST, rev)
    candidates = []
    if data["source_sha256"] == source and data["image"]:
        candidates.append(("pinned", data["image"]))
    if main_ref:
        try:
            main = manifest_at(GUEST, main_ref)
            if main["source_sha256"] == source and main["image"]:
                candidates.append(("reuse", main["image"]))
        except Fail as e:
            print(f"ignoring {main_ref}: {e}", file=sys.stderr)
    host, repo, _ = split_ref(GUEST.repo + ":x")
    try:
        body = Registry(host, repo).manifest(f"src-{source}")
        candidates.append(("reuse", f"{GUEST.repo}@sha256:{hashlib.sha256(body).hexdigest()}"))
    except NotFound:
        pass
    for state, image in candidates:
        if try_verify(image, source):
            return state, source, image
    return "build", source, ""


def write_manifest(kind, source, image):
    path = os.path.join(git_out("rev-parse", "--show-toplevel"), kind.manifest)
    text = json.dumps({"schema": 1, "source_sha256": source, "image": image}, indent=2) + "\n"
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        f.write(text)
    os.replace(tmp, path)


def cmd_gate(branch, rev, main_ref):
    match = re.match(r"^release/v([0-9]+\.[0-9]+\.[0-9]+)$", branch)
    if not match:
        raise Fail(f"{branch} is not a release branch (release/vX.Y.Z); main only takes release pull requests")
    version = match.group(1)
    found = cargo_version(rev)
    if found != version:
        raise Fail(f"{branch} carries version {found} in Cargo.toml, not {version}")
    if not is_ancestor(main_ref, rev):
        raise Fail(f"{branch} does not contain {main_ref}; merge main into dev and cut the release again")
    main_version = cargo_version(main_ref)
    if version_key(version) <= version_key(main_version):
        raise Fail(f"version {version} is not newer than {main_ref}'s {main_version}")
    if git("rev-parse", "-q", "--verify", f"refs/tags/v{version}", check=False).returncode == 0:
        raise Fail(f"tag v{version} already exists")
    data = cmd_check(GUEST, rev, allow_unpinned=False)
    print(f"release v{version} is ready: guest image {data['image']}")


def ensure_pr(base, head, title, body):
    found = subprocess.run(
        ["gh", "pr", "list", "--base", base, "--head", head, "--state", "open", "--json", "url", "--jq", ".[0].url // empty"],
        capture_output=True, text=True,
    )
    if found.returncode:
        raise Fail(f"gh pr list failed: {found.stderr.strip()}")
    url = found.stdout.strip()
    if not url:
        with tempfile.NamedTemporaryFile(mode="w", suffix=".md") as description:
            description.write(body)
            description.flush()
            created = subprocess.run(
                ["gh", "pr", "create", "--base", base, "--head", head, "--title", title, "--body-file", description.name],
                capture_output=True, text=True,
            )
        if created.returncode:
            raise Fail(f"gh pr create failed: {created.stderr.strip()}")
        url = created.stdout.strip()
    return url


def sync_dev(remote, main, dev, head, tag):
    if git("fetch", "-q", remote, f"+refs/heads/{dev}:refs/remotes/{remote}/{dev}", check=False).returncode:
        git("push", "-q", remote, f"{head}:refs/heads/{dev}")
        print(f"created {dev} from {main}")
        return
    if is_ancestor(head, f"{remote}/{dev}"):
        print(f"{dev} already contains {main}")
        return
    work = tempfile.mkdtemp(prefix="else-sync-")
    try:
        git("worktree", "add", "-q", "--detach", work, f"{remote}/{dev}")
        merged = subprocess.run(
            ["git", "-C", work, "merge", "-q", "--no-ff", "-m", f"chore(release): merge {tag} into {dev}", head],
            capture_output=True,
        ).returncode == 0
        pushed = merged and subprocess.run(
            ["git", "-C", work, "push", "-q", remote, f"HEAD:refs/heads/{dev}"], capture_output=True
        ).returncode == 0
    finally:
        git("worktree", "remove", "--force", work, check=False)
    if pushed:
        print(f"merged {main} into {dev}")
        return
    url = ensure_pr(
        dev, main, f"chore(release): merge {tag} into {dev}",
        f"Brings {tag} back into `{dev}`. Merge with a merge commit so both branches keep one history.",
    )
    print(f"{main} could not be merged into {dev} automatically; merge {url}")


def cmd_finalize(remote, main, dev):
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
        print(f"{tag} is already released")
    else:
        newer = [t for t in tags if version_key(t[1:]) >= version_key(version)]
        if newer:
            raise Fail(f"{tag} is not newer than the existing {max(newer, key=lambda t: version_key(t[1:]))}")
        data = cmd_check(GUEST, head, allow_unpinned=False)
        git("tag", "-f", "-a", tag, "-m", f"else.bot {version}\n\nGuest image: {data['image']}", head)
        git("push", "-q", remote, f"refs/tags/{tag}")
        print(f"tagged {tag} at {head} with guest image {data['image']}")
    if tags.get(tag, head) == head:
        exists = subprocess.run(["gh", "release", "view", tag], capture_output=True).returncode == 0
        if not exists:
            result = subprocess.run(
                ["gh", "release", "create", tag, "--verify-tag", "--title", f"else.bot {version}", "--generate-notes"],
                capture_output=True, text=True,
            )
            if result.returncode:
                raise Fail(f"tag {tag} is ready, but publishing its GitHub release failed: {result.stderr.strip()}")
            print(f"published the GitHub release for {tag}")
    sync_dev(remote, main, dev, head, tag)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base", action="store_true", help="act on the guest base manifest")
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("hash")
    p.add_argument("--rev", default="HEAD")
    p = sub.add_parser("check")
    p.add_argument("--rev", default="HEAD")
    p.add_argument("--allow-unpinned", action="store_true")
    p = sub.add_parser("pinned")
    p.add_argument("--rev", default="HEAD")
    p = sub.add_parser("plan")
    p.add_argument("--rev", default="HEAD")
    p.add_argument("--main-ref")
    sub.add_parser("build-args")
    p = sub.add_parser("platform-digest")
    p.add_argument("ref")
    p.add_argument("--platform", required=True, choices=PLATFORMS)
    p = sub.add_parser("digest")
    p.add_argument("ref")
    p = sub.add_parser("pin")
    p.add_argument("image")
    p.add_argument("--rev", default="HEAD")
    p = sub.add_parser("gate")
    p.add_argument("--branch", required=True)
    p.add_argument("--rev", default="HEAD")
    p.add_argument("--main-ref", default="origin/main")
    p = sub.add_parser("finalize")
    p.add_argument("--remote", default="origin")
    p.add_argument("--main", default="main")
    p.add_argument("--dev", default="dev")
    args = parser.parse_args()
    if args.base and args.command not in ("hash", "check", "pinned", "build-args", "pin"):
        parser.error(f"--base does not apply to {args.command}")
    kind = BASE if args.base else GUEST
    try:
        if args.command == "hash":
            print(source_sha256(kind, args.rev))
        elif args.command == "check":
            data = cmd_check(kind, args.rev, args.allow_unpinned)
            print(f"{kind.manifest} matches the {kind.name} inputs; image {data['image']}")
        elif args.command == "pinned":
            print(pinned(kind, args.rev))
        elif args.command == "plan":
            state, source, image = cmd_plan(args.rev, args.main_ref)
            print(f"state={state}\nhash={source}\nimage={image}")
        elif args.command == "build-args":
            print("\n".join(build_args(kind)))
        elif args.command in ("platform-digest", "digest"):
            host, repo, reference = split_ref(args.ref)
            user = os.environ.get("ELSE_GUEST_REGISTRY_USER")
            password = os.environ.get("ELSE_GUEST_REGISTRY_PASSWORD")
            credentials = (user, password) if user and password else None
            registry = Registry(host, repo, credentials)
            body = registry.manifest(reference)
            digest = "sha256:" + hashlib.sha256(body).hexdigest()
            data = json.loads(body)
            if args.command == "platform-digest" and data.get("mediaType") in INDEX_TYPES:
                os_, arch = args.platform.split("/")
                entries = [m for m in data.get("manifests", [])
                           if m.get("platform", {}).get("os") == os_ and m.get("platform", {}).get("architecture") == arch]
                if len(entries) != 1:
                    raise Fail(f"{args.ref} holds {len(entries)} {args.platform} images")
                digest = entries[0]["digest"]
            print(digest)
        elif args.command == "pin":
            source = source_sha256(kind, args.rev)
            verify(kind, args.image, source)
            write_manifest(kind, source, args.image)
            print(f"pinned {args.image} for {kind.name} source {source}")
        elif args.command == "gate":
            cmd_gate(args.branch, args.rev, args.main_ref)
        elif args.command == "finalize":
            cmd_finalize(args.remote, args.main, args.dev)
    except Fail as e:
        print(f"guest-release: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
