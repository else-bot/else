#!/usr/bin/env python3
"""Behavior tests for the release lifecycle: scripts/release.sh,
scripts/guest-release.py, and scripts/build-guest-base.sh against disposable
git repositories, a fake OCI registry, and fake gh, cargo, and docker
commands. Nothing here builds or publishes a real image.

    python3 scripts/test-release.py
"""

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import textwrap
import threading
import unittest
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LABEL = "bot.else.guest.source_sha256"
BASE_LABEL = "bot.else.guest-base.source_sha256"
INDEX = "application/vnd.oci.image.index.v1+json"
MANIFEST = "application/vnd.oci.image.manifest.v1+json"

FAKE_GH = r'''#!/usr/bin/env python3
import json, os, sys
path = os.environ["FAKE_GH_STATE"]
state = json.load(open(path)) if os.path.exists(path) else {"prs": [], "calls": []}
args = sys.argv[1:]
state["calls"].append(args)
def opt(name):
    return args[args.index(name) + 1]
out = ""
status = 0
if args[:2] == ["pr", "list"]:
    found = [p for p in state["prs"] if p["base"] == opt("--base") and p["head"] == opt("--head")]
    out = found[0]["url"] if found else ""
elif args[:2] == ["pr", "create"]:
    url = "https://github.com/else-bot/else/pull/%d" % (100 + len(state["prs"]))
    state["prs"].append({"base": opt("--base"), "head": opt("--head"), "title": opt("--title"), "url": url})
    out = url
elif args[:2] == ["workflow", "run"]:
    pass
elif args[:1] == ["api"]:
    status = int(os.environ.get("FAKE_GH_API_STATUS", "0"))
elif args[:2] == ["release", "view"]:
    status = 0 if args[2] in state.get("releases", []) else 1
elif args[:2] == ["release", "create"]:
    if os.environ.get("FAKE_GH_RELEASE_FAIL") == "1":
        status = 1
    else:
        state.setdefault("releases", []).append(args[2])
else:
    sys.exit("fake gh: unsupported " + " ".join(args))
json.dump(state, open(path, "w"))
if out:
    print(out)
sys.exit(status)
'''

FAKE_DOCKER = r"""#!/usr/bin/env python3
import hashlib, json, os, sys, urllib.error, urllib.request
args = sys.argv[1:]
with open(os.environ["FAKE_DOCKER_LOG"], "a") as log:
    log.write(json.dumps(args) + "\n")
MANIFEST = "application/vnd.oci.image.manifest.v1+json"
INDEX = "application/vnd.oci.image.index.v1+json"
LOGIN = {"X-Fake-Login": "1"}

def split(ref):
    host, _, rest = ref.partition("/")
    repo, _, reference = rest.partition("@") if "@" in rest else rest.rpartition(":")
    return f"http://{host}/v2/{repo}", reference

def get(base, kind, reference):
    try:
        return urllib.request.urlopen(urllib.request.Request(f"{base}/{kind}/{reference}", headers=LOGIN)).read()
    except urllib.error.HTTPError:
        return None

def put(base, kind, reference, body):
    urllib.request.urlopen(urllib.request.Request(f"{base}/{kind}/{reference}", data=body, method="PUT", headers=LOGIN))
    return "sha256:" + hashlib.sha256(body).hexdigest()

def opt(name):
    return args[args.index(name) + 1]

if args[:1] == ["version"]:
    print(os.environ.get("FAKE_DOCKER_ARCH", "amd64"))
    sys.exit(0)
assert args[0] == "buildx", args
args = args[1:]
if args[0] == "--builder":
    args = args[2:]
if args[0] == "inspect":
    print("Name: fake\nDriver: docker\nPlatforms: " + os.environ.get("FAKE_DOCKER_PLATFORMS", "linux/amd64*, linux/arm64"))
elif args[0] == "build":
    assert "--push" in args and args[-1] == "infra/guest", args
    if os.environ.get("FAKE_DOCKER_CONTEXT_LOG"):
        from pathlib import Path
        dockerfile = next(a.split("=", 1)[1] for a in args if a.startswith("--file="))
        with open(os.environ["FAKE_DOCKER_CONTEXT_LOG"], "a") as log:
            log.write(json.dumps(Path(dockerfile).read_text()) + "\n")
        Path(os.environ["FAKE_DOCKER_EDIT_INPUT"]).write_text("FROM scratch\n# edited during the build\n")
    labels = dict(a.split("=", 1)[1].split("=", 1) for a in args if a.startswith("--label="))
    os_, arch = opt("--platform").split("/")
    base, tag = split(opt("--tag"))
    config = json.dumps({"os": os_, "architecture": arch, "config": {"Labels": labels}}).encode()
    digest = put(base, "blobs", "sha256:" + hashlib.sha256(config).hexdigest(), config)
    put(base, "manifests", tag, json.dumps({"schemaVersion": 2, "mediaType": MANIFEST,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": digest}, "layers": []}).encode())
elif args[:2] == ["imagetools", "create"]:
    entries = []
    for source in args[4:]:
        base, reference = split(source)
        body = get(base, "manifests", reference)
        config = json.loads(get(base, "blobs", json.loads(body)["config"]["digest"]))
        entries.append({"mediaType": MANIFEST, "digest": "sha256:" + hashlib.sha256(body).hexdigest(), "size": len(body),
                        "platform": {"os": config["os"], "architecture": config["architecture"]}})
    base, tag = split(opt("--tag"))
    put(base, "manifests", tag, json.dumps({"schemaVersion": 2, "mediaType": INDEX, "manifests": entries}).encode())
elif args[:2] == ["imagetools", "inspect"]:
    body = get(*split(args[2])[:1], "manifests", split(args[2])[1])
    if body is None:
        sys.exit(1)
    if "--format" in args:
        print(json.dumps({"mediaType": json.loads(body)["mediaType"], "digest": "sha256:" + hashlib.sha256(body).hexdigest()}))
else:
    sys.exit("fake docker: unsupported " + " ".join(args))
"""

FAKE_CARGO = r'''#!/usr/bin/env python3
import re, sys
assert sys.argv[1:3] == ["update", "--workspace"], sys.argv
version = re.search(r'^version = "(.*)"', open("Cargo.toml").read(), re.M).group(1)
lock = open("Cargo.lock").read()
open("Cargo.lock", "w").write(re.sub(r'(name = "else-fixture"\nversion = )"[^"]*"', r'\1"%s"' % version, lock))
'''


class FakeRegistry:
    """An OCI distribution endpoint with Bearer token challenges."""

    def __init__(self):
        self.objects = {}
        self.tags = {}
        self.private = False
        self.refused = set()
        self.transient_failures = 0
        self.auth_requests = []
        self.tamper = set()
        registry = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                if self.path.startswith("/token"):
                    registry.auth_requests.append(self.headers.get("Authorization"))
                    scope = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)["scope"][0]
                    status = 403 if scope.split(":")[1] in registry.refused else 200
                    return self.reply(status, json.dumps({"token": "anon"}).encode())
                logged_in = self.headers.get("X-Fake-Login") == "1"
                if not logged_in and self.headers.get("Authorization") != "Bearer anon":
                    repo = "/".join(self.path.split("/")[2:-2])
                    return self.reply(401, b"{}", {
                        "WWW-Authenticate": f'Bearer realm="http://{registry.host}/token",service="fake",scope="repository:{repo}:pull"'
                    })
                if registry.private and not logged_in:
                    return self.reply(403, b"{}")
                if registry.transient_failures:
                    registry.transient_failures -= 1
                    return self.reply(503, b"{}")
                parts = self.path.split("/")
                reference = parts[-1]
                digest = registry.tags.get(reference, reference)
                body = registry.objects.get(digest)
                if body is None:
                    return self.reply(404, b"{}")
                if digest in registry.tamper:
                    body = body + b" "
                self.reply(200, body)

            def do_PUT(self):
                assert self.headers.get("X-Fake-Login") == "1", "pushes come from a logged-in docker"
                body = self.rfile.read(int(self.headers["Content-Length"]))
                digest = registry.put_bytes(body)
                reference = self.path.split("/")[-1]
                if "/manifests/" in self.path and reference != digest:
                    registry.tags[reference] = digest
                self.reply(201, b"")

            def reply(self, status, body, headers=()):
                self.send_response(status)
                for k, v in dict(headers).items():
                    self.send_header(k, v)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.host = f"127.0.0.1:{self.server.server_port}"
        self.repo = f"{self.host}/else-bot/else-guest"
        self.base_repo = f"{self.host}/else-bot/else-guest-base"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def put_bytes(self, body):
        digest = "sha256:" + hashlib.sha256(body).hexdigest()
        self.objects[digest] = body
        return digest

    def put(self, document):
        return self.put_bytes(json.dumps(document).encode())

    def publish(self, source, platforms=("linux/amd64", "linux/arm64"), labels=None, tag=None, base=False):
        entries = []
        for platform in platforms:
            os_, arch = platform.split("/")
            label = (labels or {}).get(platform, source)
            config = self.put({"os": os_, "architecture": arch,
                               "config": {"Labels": {BASE_LABEL if base else LABEL: label}}})
            manifest = self.put({"schemaVersion": 2, "mediaType": MANIFEST,
                                 "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config},
                                 "layers": []})
            entries.append({"mediaType": MANIFEST, "digest": manifest, "platform": {"os": os_, "architecture": arch}})
        entries.append({"mediaType": MANIFEST, "digest": self.put({"attestation": True}),
                        "platform": {"os": "unknown", "architecture": "unknown"}})
        index = self.put({"schemaVersion": 2, "mediaType": INDEX, "manifests": entries})
        if tag:
            self.tags[tag] = index
        return f"{self.base_repo if base else self.repo}@{index}"


class Lifecycle(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.registry = FakeRegistry()

    @classmethod
    def tearDownClass(cls):
        cls.registry.server.shutdown()

    def setUp(self):
        self.registry.objects.clear()
        self.registry.tags.clear()
        self.registry.private = False
        self.registry.refused.clear()
        self.registry.transient_failures = 0
        self.registry.auth_requests.clear()
        self.registry.tamper.clear()
        self.tmp = Path(tempfile.mkdtemp(prefix="else-release-test-"))
        self.addCleanup(shutil.rmtree, self.tmp)
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        for name, text in (("gh", FAKE_GH), ("cargo", FAKE_CARGO), ("docker", FAKE_DOCKER)):
            (bin_dir / name).write_text(text)
            (bin_dir / name).chmod(0o755)
        self.gh_state = self.tmp / "gh.json"
        self.env = {
            **os.environ,
            "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
            "FAKE_GH_STATE": str(self.gh_state),
            "FAKE_DOCKER_LOG": str(self.tmp / "docker.log"),
            "ELSE_GUEST_IMAGE_REPO": self.registry.repo,
            "ELSE_GUEST_BASE_REPO": self.registry.base_repo,
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_AUTHOR_NAME": "Test", "GIT_AUTHOR_EMAIL": "test@example.com",
            "GIT_COMMITTER_NAME": "Test", "GIT_COMMITTER_EMAIL": "test@example.com",
        }
        self.origin = self.tmp / "origin.git"
        self.git(self.tmp, "init", "-q", "--bare", "-b", "main", str(self.origin))
        self.work = self.clone("work")
        self.write("Cargo.toml", '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "0.0.21"\n')
        self.write("Cargo.lock", 'version = 4\n\n[[package]]\nname = "else-fixture"\nversion = "0.0.21"\n')
        self.write("infra/guest/Dockerfile", "FROM scratch\nCOPY else-init /\n")
        self.write("infra/guest/else-init", "#!/bin/sh\n", mode=0o755)
        self.write("infra/guest/vnc/else_vnc_targets.py", "TARGETS = 1\n")
        self.write("infra/guest/vnc/test_else_vnc_targets.py", "assert True\n")
        self.write("infra/guest/base.Dockerfile", "FROM scratch\nCOPY apt-downloads.conf x11vnc/keymap-settle.patch /\n")
        self.write("infra/guest/apt-downloads.conf", 'Acquire::Retries "5";\n')
        self.write("infra/guest/x11vnc/keymap-settle.patch", "--- a\n+++ b\n")
        (self.work / "scripts").mkdir()
        for name in ("release.sh", "guest-release.py", "build-guest-base.sh"):
            shutil.copy2(ROOT / "scripts" / name, self.work / "scripts" / name)
        self.write_base_manifest("0" * 64, None)
        self.write("infra/guest-image.json", json.dumps({"schema": 1, "source_sha256": "0" * 64, "image": None}))
        self.commit("initial")
        self.write_base_manifest(self.base_source(), self.publish_base())
        self.commit("pin the guest base")
        self.write_manifest(self.helper("hash").strip(), None)
        self.commit("bootstrap the guest image manifest")
        self.git(self.work, "tag", "-a", "v0.0.21", "-m", "else.bot 0.0.21")
        self.git(self.work, "push", "-q", "origin", "main", "main:dev", "v0.0.21")


    def git(self, cwd, *args, check=True):
        result = subprocess.run(["git", *args], cwd=cwd, env=self.env, capture_output=True, text=True)
        if check and result.returncode:
            self.fail(f"git {' '.join(args)}: {result.stderr}")
        return result.stdout.strip()

    def clone(self, name, branch=None):
        path = self.tmp / name
        self.git(self.tmp, "clone", "-q", *(["-b", branch] if branch else []), str(self.origin), str(path))
        return path

    def write(self, path, text, mode=None, repo=None):
        target = (repo or self.work) / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)
        if mode is not None:
            target.chmod(mode)

    def write_manifest(self, source, image, repo=None):
        self.write("infra/guest-image.json",
                   json.dumps({"schema": 1, "source_sha256": source, "image": image}, indent=2) + "\n", repo=repo)

    def write_base_manifest(self, source, image, repo=None):
        self.write("infra/guest-base.json",
                   json.dumps({"schema": 1, "source_sha256": source, "image": image}, indent=2) + "\n", repo=repo)

    def base_source(self, repo=None):
        return self.helper("--base", "hash", cwd=repo).strip()

    def publish_base(self, **kwargs):
        return self.registry.publish(self.base_source(), base=True, **kwargs)

    def base_manifest(self, repo=None):
        return json.loads(((repo or self.work) / "infra/guest-base.json").read_text())

    def publish_script(self, *args, ok=True, **env):
        self.env.update(env)
        try:
            return self.run_tool(["bash", "scripts/build-guest-base.sh", *args], self.work, ok)
        finally:
            for key in env:
                del self.env[key]

    def docker_calls(self):
        log = self.tmp / "docker.log"
        calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        log.unlink(missing_ok=True)
        return calls

    def commit(self, message, repo=None):
        repo = repo or self.work
        self.git(repo, "add", "-A")
        self.git(repo, "commit", "-q", "--allow-empty", "-m", message)
        return self.git(repo, "rev-parse", "HEAD")

    def run_tool(self, args, cwd, ok=True):
        result = subprocess.run(args, cwd=cwd, env=self.env, capture_output=True, text=True)
        if ok is not None and (result.returncode == 0) != ok:
            self.fail(f"{args} exited {result.returncode}\nstdout: {result.stdout}\nstderr: {result.stderr}")
        return result

    def helper(self, *args, cwd=None, ok=True):
        result = self.run_tool([sys.executable, "scripts/guest-release.py", *args], cwd or self.work, ok)
        return result.stdout if ok else result.stderr

    def release(self, *args, ok=True, cwd=None):
        return self.run_tool(["bash", "scripts/release.sh", *args], cwd or self.work, ok)

    def gh(self):
        return json.loads(self.gh_state.read_text()) if self.gh_state.exists() else {"prs": [], "calls": []}

    def remote_ref(self, ref):
        return self.git(self.tmp, "--git-dir", str(self.origin), "rev-parse", "-q", "--verify", ref, check=False)

    def source(self, repo=None):
        return self.helper("hash", cwd=repo).strip()

    def cut_release(self):
        """Run release.sh, pin a published image the way guest-image.yml
        does, and return a clone of the release branch."""
        self.release("-y", "patch")
        branch = self.clone("release", "release/v0.0.22")
        image = self.registry.publish(self.source(branch))
        self.helper("pin", image, cwd=branch)
        self.commit("chore(release): pin guest image", repo=branch)
        self.git(branch, "push", "-q", "origin", "HEAD:refs/heads/release/v0.0.22")
        return branch, image

    def merge_to_main(self, branch_repo):
        main = self.clone("main-ci", "main")
        self.git(main, "fetch", "-q", "origin", "release/v0.0.22")
        self.git(main, "merge", "-q", "--no-ff", "-m", "Merge release/v0.0.22", "FETCH_HEAD")
        self.git(main, "push", "-q", "origin", "main")
        return main


    def test_hash_covers_build_inputs_but_not_tests_or_manifest(self):
        base = self.source()
        self.write("infra/guest/vnc/test_else_vnc_targets.py", "assert False\n")
        self.write_manifest("1" * 64, None)
        self.commit("tests and manifest only")
        self.assertEqual(self.source(), base, "tests and the manifest are not build inputs")
        self.write("infra/guest/vnc/else_vnc_targets.py", "TARGETS = 2\n")
        self.commit("change a copied file")
        changed = self.source()
        self.assertNotEqual(changed, base, "a copied file changes the hash")
        (self.work / "infra/guest/else-init").chmod(0o644)
        self.commit("drop the executable bit")
        self.assertNotEqual(self.source(), changed, "file modes are build inputs")
        self.assertEqual(self.helper("hash", "--rev", "HEAD~2").strip(), base, "hashing a revision ignores the worktree")
        before_recipe = self.source()
        self.write(".github/workflows/guest-image.yml", "name: changed-build-recipe\n")
        self.commit("change the build recipe")
        self.assertNotEqual(self.source(), before_recipe, "the build workflow changes the hash")

    def test_plan_builds_when_the_registry_refuses_a_token_for_a_new_package(self):
        self.registry.refused.add("else-bot/else-guest")
        plan = self.helper("plan", "--main-ref", "origin/main")
        self.assertIn("state=build", plan)


    def test_registry_digest_commands_select_native_manifests(self):
        image = self.registry.publish(self.source(), tag="candidate")
        reference = self.registry.repo + ":candidate"
        self.assertEqual(self.helper("digest", reference).strip(), image.split("@")[1])
        index = json.loads(self.registry.objects[image.split("@")[1]])
        for entry in index["manifests"][:2]:
            platform = entry["platform"]["os"] + "/" + entry["platform"]["architecture"]
            self.assertEqual(self.helper("platform-digest", reference, "--platform", platform).strip(), entry["digest"])
            plain = self.registry.repo + "@" + entry["digest"]
            self.assertEqual(self.helper("platform-digest", plain, "--platform", platform).strip(), entry["digest"])

    def test_publish_commands_authenticate_but_release_verification_is_anonymous(self):
        source = self.source()
        image = self.registry.publish(source, tag="candidate")
        self.env["ELSE_GUEST_REGISTRY_USER"] = "fixture"
        self.env["ELSE_GUEST_REGISTRY_PASSWORD"] = "fixture-password"
        self.helper("digest", self.registry.repo + ":candidate")
        self.assertTrue(self.registry.auth_requests[-1].startswith("Basic "))
        self.registry.auth_requests.clear()
        self.write_manifest(source, image)
        self.commit("pin the published image")
        self.helper("check")
        self.assertTrue(self.registry.auth_requests)
        self.assertEqual(set(self.registry.auth_requests), {None})

    def test_registry_retries_a_temporary_server_failure(self):
        image = self.registry.publish(self.source(), tag="candidate")
        self.registry.transient_failures = 1
        self.assertEqual(self.helper("digest", self.registry.repo + ":candidate").strip(), image.split("@")[1])
        self.assertEqual(self.registry.transient_failures, 0)

    def test_check_rejects_invalid_and_mismatched_manifests(self):
        source = self.source()
        image = self.registry.publish(source)
        cases = {
            "not JSON": "{",
            "wrong schema": json.dumps({"schema": 2, "source_sha256": source, "image": None}),
            "short hash": json.dumps({"schema": 1, "source_sha256": "abc", "image": None}),
            "tag instead of digest": json.dumps({"schema": 1, "source_sha256": source, "image": self.registry.repo + ":latest"}),
            "other repository": json.dumps({"schema": 1, "source_sha256": source, "image": "docker.io/x/y@sha256:" + "a" * 64}),
            "stale hash": json.dumps({"schema": 1, "source_sha256": "f" * 64, "image": image}),
        }
        for name, text in cases.items():
            with self.subTest(name):
                self.write("infra/guest-image.json", text)
                self.commit(name)
                self.helper("check", ok=False)
        self.write_manifest(source, None)
        self.commit("unpinned")
        self.assertIn("no published image", self.helper("check", ok=False))
        self.helper("check", "--allow-unpinned")
        self.write_manifest(source, image)
        self.commit("pinned")
        self.helper("check")

    def test_verify_rejects_images_that_do_not_match(self):
        source = self.source()
        broken = {
            "missing arm64": self.registry.publish(source, platforms=("linux/amd64",)),
            "arm64 built elsewhere": self.registry.publish(source, labels={"linux/arm64": "e" * 64}),
            "unpublished digest": f"{self.registry.repo}@sha256:{'d' * 64}",
        }
        tampered = self.registry.publish(source)
        self.registry.tamper.add(tampered.split("@")[1])
        broken["content not matching digest"] = tampered
        for name, image in broken.items():
            with self.subTest(name):
                self.write_manifest(source, image)
                self.commit(name)
                self.helper("check", ok=False)
        self.registry.tamper.clear()
        good = self.registry.publish(source)
        self.registry.private = True
        self.write_manifest(source, good)
        self.commit("private package")
        self.assertIn("public", self.helper("check", ok=False))


    def test_plan_reuses_published_images_before_building(self):
        source = self.source()
        self.assertIn("state=build", self.helper("plan", "--main-ref", "origin/main"))
        tagged = self.registry.publish(source, tag=f"src-{source}")
        self.assertIn(f"state=reuse\nhash={source}\nimage={tagged}", self.helper("plan"))
        self.registry.tags.clear()
        on_main = self.registry.publish(source)
        main = self.clone("main-pin", "main")
        self.write_manifest(source, on_main, repo=main)
        self.commit("pin on main", repo=main)
        self.git(main, "push", "-q", "origin", "main")
        self.git(self.work, "fetch", "-q", "origin")
        self.assertIn(f"image={on_main}", self.helper("plan", "--main-ref", "origin/main"))
        self.helper("pin", on_main)
        first = (self.work / "infra/guest-image.json").read_bytes()
        self.helper("pin", on_main)
        self.assertEqual((self.work / "infra/guest-image.json").read_bytes(), first, "pinning again changes nothing")
        self.commit("pin")
        self.assertIn("state=pinned", self.helper("plan"))
        self.registry.publish(source, labels={"linux/amd64": "c" * 64}, tag=f"src-{source}")
        self.registry.objects.clear()
        self.publish_base()
        self.assertIn("state=build", self.helper("plan", "--main-ref", "origin/main"), "unverifiable images are rebuilt")

    def test_pin_refuses_an_image_built_from_other_inputs(self):
        other = self.registry.publish("b" * 64)
        self.helper("pin", other, ok=False)
        self.assertEqual(json.loads((self.work / "infra/guest-image.json").read_text())["image"], None)


    def test_release_cuts_a_branch_and_pull_request_without_tagging(self):
        out = self.release("-y", "patch").stdout
        self.assertIn("https://github.com/else-bot/else/pull/100", out)
        self.assertTrue(self.remote_ref("refs/heads/release/v0.0.22"))
        self.assertFalse(self.remote_ref("refs/tags/v0.0.22"), "no tag before the merge")
        branch = self.clone("release", "release/v0.0.22")
        self.assertIn('version = "0.0.22"', (branch / "Cargo.toml").read_text())
        self.assertIn('version = "0.0.22"', (branch / "Cargo.lock").read_text())
        self.assertEqual(self.git(branch, "log", "-1", "--format=%s"), "chore(release): v0.0.22")
        self.assertEqual(self.git(branch, "rev-parse", "HEAD~1"), self.remote_ref("refs/heads/dev"))
        self.assertEqual([p["base"] for p in self.gh()["prs"]], ["main"])
        self.assertEqual(self.git(self.work, "status", "--porcelain"), "", "the caller's checkout is untouched")
        head = self.remote_ref("refs/heads/release/v0.0.22")
        again = self.release("-y", "patch").stdout
        self.assertIn("already exists", again)
        self.assertEqual(self.remote_ref("refs/heads/release/v0.0.22"), head, "a rerun pushes nothing")
        self.assertEqual(len(self.gh()["prs"]), 1, "a rerun opens no second pull request")
        self.assertEqual(self.git(self.work, "worktree", "list", "--porcelain").count("worktree "), 1)

    def test_release_refuses_dev_without_main(self):
        main = self.clone("main-hotfix", "main")
        self.write("hotfix", "x", repo=main)
        self.commit("hotfix", repo=main)
        self.git(main, "push", "-q", "origin", "main")
        self.assertIn("dev does not contain main", self.release("-y", "patch", ok=False).stderr)
        self.assertFalse(self.remote_ref("refs/heads/release/v0.0.22"))

    def test_release_refuses_an_existing_tag(self):
        self.git(self.work, "tag", "v0.0.22")
        self.git(self.work, "push", "-q", "origin", "v0.0.22")
        self.assertIn("already exists", self.release("-y", "patch", ok=False).stderr)
        self.assertEqual(self.gh()["prs"], [])


    def test_gate_passes_only_a_pinned_matching_release(self):
        branch, _ = self.cut_release()
        self.helper("gate", "--branch", "release/v0.0.22", cwd=branch)
        self.assertIn("not a release branch", self.helper("gate", "--branch", "feature/x", cwd=branch, ok=False))
        self.assertIn("not 0.0.23", self.helper("gate", "--branch", "release/v0.0.23", cwd=branch, ok=False))
        self.write("infra/guest/else-init", "#!/bin/sh\necho changed\n", repo=branch)
        self.commit("change the guest after pinning", repo=branch)
        self.assertIn("hash to", self.helper("gate", "--branch", "release/v0.0.22", cwd=branch, ok=False))

    def test_gate_requires_main_and_an_unused_tag(self):
        branch, _ = self.cut_release()
        main = self.clone("main-hotfix", "main")
        self.write("hotfix", "x", repo=main)
        self.commit("hotfix", repo=main)
        self.git(main, "push", "-q", "origin", "main")
        self.git(branch, "fetch", "-q", "origin")
        self.assertIn("does not contain", self.helper("gate", "--branch", "release/v0.0.22", cwd=branch, ok=False))
        self.git(branch, "merge", "-q", "origin/main")
        self.helper("gate", "--branch", "release/v0.0.22", cwd=branch)
        self.git(branch, "tag", "v0.0.22")
        self.assertIn("already exists", self.helper("gate", "--branch", "release/v0.0.22", cwd=branch, ok=False))


    def test_finalize_tags_last_and_merges_main_back_into_dev(self):
        branch, image = self.cut_release()
        main = self.merge_to_main(branch)
        out = self.helper("finalize", cwd=main)
        head = self.git(main, "rev-parse", "HEAD")
        self.assertIn(f"tagged v0.0.22 at {head}", out)
        self.assertEqual(self.remote_ref("refs/tags/v0.0.22^{commit}"), head)
        message = self.git(self.tmp, "--git-dir", str(self.origin), "tag", "-l", "--format=%(contents)", "v0.0.22")
        self.assertIn(image, message)
        dev = self.remote_ref("refs/heads/dev")
        self.assertEqual(self.git(main, "merge-base", "--is-ancestor", head, dev, check=False), "")
        self.assertEqual(self.git(main, "rev-list", "--count", f"{head}..{dev}"), "1", "dev gets one merge commit")
        again = self.helper("finalize", cwd=main)
        self.assertIn("already released", again)
        self.assertIn("already contains", again)
        self.assertEqual(self.remote_ref("refs/heads/dev"), dev, "a rerun changes nothing")
        self.assertEqual(self.gh()["releases"], ["v0.0.22"], "a rerun publishes no second GitHub release")

    def test_finalize_retries_github_publication_without_replacing_the_tag(self):
        branch, _ = self.cut_release()
        main = self.merge_to_main(branch)
        self.env["FAKE_GH_RELEASE_FAIL"] = "1"
        self.assertIn("publishing its GitHub release failed", self.helper("finalize", cwd=main, ok=False))
        tag = self.remote_ref("refs/tags/v0.0.22")
        self.assertTrue(tag)
        self.assertFalse(self.gh().get("releases"))
        del self.env["FAKE_GH_RELEASE_FAIL"]
        self.helper("finalize", cwd=main)
        self.assertEqual(self.remote_ref("refs/tags/v0.0.22"), tag)
        self.assertEqual(self.gh()["releases"], ["v0.0.22"])

    def test_finalize_never_tags_an_unverified_release(self):
        branch, image = self.cut_release()
        main = self.merge_to_main(branch)
        self.registry.objects.clear()
        self.helper("finalize", cwd=main, ok=False)
        self.assertFalse(self.remote_ref("refs/tags/v0.0.22"), "a missing image leaves no tag")
        self.write("infra/guest/else-init", "#!/bin/sh\necho unpinned\n", repo=main)
        self.commit("guest change without a pin", repo=main)
        self.git(main, "push", "-q", "origin", "main")
        self.registry.publish(json.loads((main / "infra/guest-image.json").read_text())["source_sha256"])
        self.assertIn("hash to", self.helper("finalize", cwd=main, ok=False))
        self.assertFalse(self.remote_ref("refs/tags/v0.0.22"), "stale inputs leave no tag")

    def test_finalize_opens_one_pull_request_when_dev_conflicts(self):
        branch, _ = self.cut_release()
        dev = self.clone("dev-work", "dev")
        self.write("Cargo.toml", '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "0.1.0"\n', repo=dev)
        self.commit("conflicting dev work", repo=dev)
        self.git(dev, "push", "-q", "origin", "dev")
        dev_head = self.remote_ref("refs/heads/dev")
        main = self.merge_to_main(branch)
        self.assertIn("could not be merged", self.helper("finalize", cwd=main))
        self.assertTrue(self.remote_ref("refs/tags/v0.0.22"), "the release is tagged before the sync")
        self.assertEqual(self.remote_ref("refs/heads/dev"), dev_head)
        self.helper("finalize", cwd=main)
        prs = [p for p in self.gh()["prs"] if p["base"] == "dev"]
        self.assertEqual([(p["head"], p["title"]) for p in prs], [("main", "chore(release): merge v0.0.22 into dev")])

    def test_finalize_ignores_main_pushes_without_a_new_version(self):
        out = self.helper("finalize", cwd=self.work)
        self.assertIn("v0.0.21 is already released", out)
        self.assertEqual(self.remote_ref("refs/tags/v0.0.21^{commit}"), self.git(self.work, "rev-parse", "HEAD"))


    def test_base_hash_covers_only_heavy_inputs(self):
        base, guest = self.base_source(), self.source()
        for path, text in (("infra/guest/else-init", "#!/bin/sh\necho thin\n"),
                           ("infra/guest/Dockerfile", "FROM scratch\nCOPY else-init /usr/local/bin/\n"),
                           ("scripts/guest-release.py", (self.work / "scripts/guest-release.py").read_text() + "\n"),
                           (".github/workflows/guest-image.yml", "name: changed-guest-recipe\n")):
            with self.subTest(path):
                self.write(path, text)
                self.commit(f"thin change to {path}")
                self.assertEqual(self.base_source(), base, f"{path} is not a base input")
                self.assertNotEqual(self.source(), guest, f"{path} is a guest input")
                guest = self.source()
        for path in ("infra/guest/base.Dockerfile", "infra/guest/apt-downloads.conf", "infra/guest/x11vnc/keymap-settle.patch"):
            with self.subTest(path):
                self.write(path, (self.work / path).read_text() + "# changed\n")
                self.commit(f"heavy change to {path}")
                self.assertNotEqual(self.base_source(), base, f"{path} is a base input")
                self.assertNotEqual(self.source(), guest, "a heavy change also changes the guest")
                base, guest = self.base_source(), self.source()
        self.write_base_manifest(base, self.publish_base())
        self.commit("pin the new base")
        self.assertEqual(self.base_source(), base, "pinning the base is not a base input")
        self.assertNotEqual(self.source(), guest, "the guest is rebuilt on a newly pinned base")

    def test_guest_build_args_select_the_pinned_base(self):
        base = self.base_manifest()["image"]
        args = self.helper("build-args").splitlines()
        self.assertIn(f"--build-arg=BASE={base}", args)
        self.assertIn("--file=infra/guest/Dockerfile", args)
        self.assertIn(f"--label={LABEL}={self.source()}", args)
        base_args = self.helper("--base", "build-args").splitlines()
        self.assertIn("--file=infra/guest/base.Dockerfile", base_args)
        self.assertIn(f"--label={BASE_LABEL}={self.base_source()}", base_args)
        self.assertFalse([a for a in base_args if a.startswith("--build-arg=BASE=")], "the base is built from Debian")
        self.assertIn(base, self.helper("--base", "pinned"))
        self.helper("--base", "check")

    def test_guest_builds_refuse_a_missing_or_stale_base(self):
        self.write_base_manifest(self.base_source(), None)
        self.commit("base never published")
        for args in (("build-args",), ("plan", "--main-ref", "origin/main"), ("check",), ("--base", "pinned")):
            with self.subTest(state="unpublished", command=args[0]):
                error = self.helper(*args, ok=False)
                self.assertIn("infra/guest-base.json has no published image", error)
                self.assertIn("scripts/build-guest-base.sh", error)
        self.write_base_manifest(self.base_source(), self.publish_base())
        self.commit("publish the base")
        self.helper("build-args")
        self.write("infra/guest/apt-downloads.conf", 'Acquire::Retries "9";\n')
        self.commit("change a heavy input without publishing")
        for args in (("build-args",), ("plan", "--main-ref", "origin/main"), ("check", "--allow-unpinned")):
            with self.subTest(state="stale", command=args[0]):
                error = self.helper(*args, ok=False)
                self.assertIn("guest base build inputs at HEAD hash to", error)
                self.assertIn("scripts/build-guest-base.sh", error)
        self.write("infra/guest/apt-downloads.conf", 'Acquire::Retries "5";\n')
        self.commit("restore the heavy input")
        self.registry.objects.clear()
        self.assertIn("cannot be read anonymously", self.helper("build-args", ok=False), "a vanished base is refused")

    def test_release_gate_refuses_a_stale_base(self):
        branch, _ = self.cut_release()
        self.write("infra/guest/base.Dockerfile", "FROM scratch\n", repo=branch)
        self.commit("heavy change after pinning", repo=branch)
        self.assertIn("guest base build inputs", self.helper("gate", "--branch", "release/v0.0.22", cwd=branch, ok=False))

    def test_base_pin_requires_both_platforms_public_and_base_labels(self):
        source = self.base_source()
        before = self.base_manifest()
        broken = {
            "missing arm64": self.publish_base(platforms=("linux/amd64",)),
            "arm64 built elsewhere": self.publish_base(labels={"linux/arm64": "e" * 64}),
            "guest label instead of base label": self.registry.publish(source).replace(self.registry.repo, self.registry.base_repo),
            "guest repository": self.registry.publish(source),
        }
        for name, image in broken.items():
            with self.subTest(name):
                self.helper("--base", "pin", image, ok=False)
                self.assertEqual(self.base_manifest(), before)
        good = self.publish_base()
        self.registry.private = True
        self.assertIn("public", self.helper("--base", "pin", good, ok=False))
        self.registry.private = False
        self.helper("--base", "pin", good)
        self.assertEqual(self.base_manifest(), {"schema": 1, "source_sha256": source, "image": good})

    def test_base_publisher_builds_both_platforms_and_pins_without_committing(self):
        self.write("infra/guest/base.Dockerfile", "FROM scratch\n# new packages\n")
        head = self.commit("change the base")
        source = self.base_source()
        out = self.publish_script().stdout
        calls = self.docker_calls()
        builds = [c for c in calls if c[1:2] == ["build"]]
        self.assertEqual([c[c.index("--platform") + 1] for c in builds], ["linux/amd64", "linux/arm64"])
        for call in builds:
            self.assertIn(f"--label={BASE_LABEL}={source}", call)
            self.assertIn("--file=infra/guest/base.Dockerfile", call)
        self.assertIn("under emulation", out, "an emulated build says so")
        self.assertFalse([c for c in calls if "--use" in c or c[1:2] in (["create"], ["use"])], "the global builder is untouched")
        image = self.base_manifest()["image"]
        self.assertIn(f"Pinned {image}", out)
        self.assertEqual(self.base_manifest()["source_sha256"], source)
        self.assertEqual(self.git(self.work, "rev-parse", "HEAD"), head, "nothing is committed")
        self.assertEqual(self.git(self.work, "status", "--porcelain"), "M infra/guest-base.json")
        self.assertEqual(self.git(self.work, "tag", "-l"), "v0.0.21", "nothing is tagged")
        self.commit("pin the base")
        self.helper("--base", "check")
        self.assertIn(f"--build-arg=BASE={image}", self.helper("build-args"))
        again = self.publish_script().stdout
        self.assertFalse([c for c in self.docker_calls() if c[1:2] == ["build"]], "a rerun builds nothing")
        self.assertIn(f"Pinned {image}", again)
        self.assertEqual(self.git(self.work, "status", "--porcelain"), "", "a rerun pins the same index")

    def test_base_publisher_joins_platforms_built_on_two_machines(self):
        self.write("infra/guest/base.Dockerfile", "FROM scratch\n# two machines\n")
        self.commit("change the base")
        before = self.base_manifest()
        out = self.publish_script("--platform", "linux/amd64").stdout
        self.assertIn("still needs: linux/arm64", out)
        self.assertIn("scripts/build-guest-base.sh --platform linux/arm64", out)
        self.assertEqual(self.base_manifest(), before, "one platform pins nothing")
        self.assertEqual([c[c.index("--platform") + 1] for c in self.docker_calls() if c[1:2] == ["build"]], ["linux/amd64"])
        out = self.publish_script("--platform", "linux/arm64", FAKE_DOCKER_ARCH="arm64",
                                  FAKE_DOCKER_PLATFORMS="linux/arm64*").stdout
        self.assertNotIn("under emulation", out)
        self.assertEqual([c[c.index("--platform") + 1] for c in self.docker_calls() if c[1:2] == ["build"]], ["linux/arm64"])
        image = self.base_manifest()["image"]
        self.assertIn(f"Pinned {image}", out)
        self.publish_script("--skip-build")
        self.assertFalse([c for c in self.docker_calls() if c[1:2] == ["build"]])
        self.assertEqual(self.base_manifest()["image"], image)

    def test_base_publisher_builds_committed_inputs_while_the_checkout_changes(self):
        dockerfile = "FROM scratch\n# committed base\n"
        self.write("infra/guest/base.Dockerfile", dockerfile)
        self.commit("freeze the base inputs")
        captured = self.work / "build-contexts.jsonl"
        self.publish_script(FAKE_DOCKER_CONTEXT_LOG=str(captured),
                            FAKE_DOCKER_EDIT_INPUT=str(self.work / "infra/guest/base.Dockerfile"))
        self.assertEqual([json.loads(line) for line in captured.read_text().splitlines()], [dockerfile, dockerfile])
        self.assertNotEqual((self.work / "infra/guest/base.Dockerfile").read_text(), dockerfile)
        self.assertEqual(self.base_manifest()["source_sha256"], self.base_source())

    def test_base_publisher_refuses_before_building_what_it_cannot_publish(self):
        self.write("infra/guest/base.Dockerfile", "FROM scratch\n# refused\n")
        self.commit("change the base")
        before = self.base_manifest()
        error = self.publish_script(ok=False, FAKE_DOCKER_PLATFORMS="linux/amd64*").stderr
        self.assertIn("cannot build linux/arm64", error)
        self.assertIn("--platform linux/arm64", error)
        self.assertFalse([c for c in self.docker_calls() if c[1:2] == ["build"]], "no partial build")
        self.write("infra/guest/apt-downloads.conf", "uncommitted\n")
        self.assertIn("differ from HEAD", self.publish_script(ok=False).stderr)
        self.assertEqual(self.docker_calls(), [], "dirty inputs never reach docker")
        self.git(self.work, "checkout", "--", "infra/guest/apt-downloads.conf")
        self.registry.private = True
        error = self.publish_script(ok=False).stderr
        self.assertIn("make the package public", error)
        self.assertEqual(self.base_manifest(), before, "a private base is not pinned")
        self.registry.private = False
        self.docker_calls()
        self.assertIn("Pinned", self.publish_script("--skip-build").stdout)
        self.assertFalse([c for c in self.docker_calls() if c[1:2] == ["build"]], "the pushed platforms are reused")


    def test_repository_manifest_and_workflows(self):
        manifest = json.loads((ROOT / "infra/guest-image.json").read_text())
        self.assertEqual(set(manifest), {"schema", "source_sha256", "image"})
        if manifest["image"] is not None:
            self.assertRegex(manifest["image"], r"^ghcr\.io/else-bot/else-guest@sha256:[0-9a-f]{64}$")
        self.assertRegex(manifest["source_sha256"], r"^[0-9a-f]{64}$")
        base = json.loads((ROOT / "infra/guest-base.json").read_text())
        self.assertEqual(set(base), {"schema", "source_sha256", "image"})
        self.assertRegex(base["source_sha256"], r"^[0-9a-f]{64}$")
        if base["image"] is not None:
            self.assertRegex(base["image"], r"^ghcr\.io/else-bot/else-guest-base@sha256:[0-9a-f]{64}$")
        thin = (ROOT / "infra/guest/Dockerfile").read_text()
        self.assertRegex(thin, r"(?m)^FROM \$BASE$", "the guest builds on the pinned base")
        for heavy in ("apt-get", "snapshot.debian.org", "dpkg-buildpackage"):
            self.assertNotIn(heavy, thin, "release CI never runs APT or compiles packages")
        for workflow in (ROOT / ".github/workflows").glob("*.yml"):
            for line in workflow.read_text().splitlines():
                action = re.match(r"\s*(?:-\s+)?uses:\s*(.*)", line)
                if action:
                    ref = action.group(1).split("#")[0].strip()
                    self.assertRegex(ref, r"^[\w.-]+/[\w.-]+@[0-9a-f]{40}$", f"{workflow.name} pins actions by commit")
            self.assertNotIn("pull_request_target", workflow.read_text())


class ReleaseReadinessWorkflow(unittest.TestCase):
    def status_step(self, name):
        workflow = (ROOT / ".github/workflows/release-ready.yml").read_text()
        step = workflow.split(f"      - name: {name}\n", 1)[1].split("      - ", 1)[0]
        block = re.search(r"(?m)^        run: \|\n((?:          .*\n|\n)+)", step)
        self.assertIsNotNone(block)
        return textwrap.dedent(block.group(1))

    def test_readiness_reports_status_on_the_verified_commit(self):
        with tempfile.TemporaryDirectory() as directory:
            work = Path(directory)
            (work / "gh").write_text(FAKE_GH)
            (work / "gh").chmod(0o755)
            env = {
                **os.environ, "PATH": f"{work}{os.pathsep}{os.environ['PATH']}",
                "FAKE_GH_STATE": str(work / "calls.json"),
                "GH_TOKEN": "test-token", "GITHUB_REPOSITORY": "else-bot/else",
                "GITHUB_SHA": "a" * 40, "GITHUB_SERVER_URL": "https://github.com",
                "GITHUB_RUN_ID": "123",
            }
            for step, result, expected in (
                ("Mark readiness pending", "", "pending"),
                ("Report readiness", "success", "success"),
                ("Report readiness", "failure", "failure"),
                ("Report readiness", "cancelled", "error"),
                ("Report readiness", "unknown", "failure"),
            ):
                with self.subTest(step=step, result=result):
                    run = subprocess.run(["bash", "-e", "-c", self.status_step(step)],
                                         env={**env, "RESULT": result}, capture_output=True, text=True)
                    self.assertEqual(run.returncode, 0, run.stderr)
                    args = json.loads((work / "calls.json").read_text())["calls"][-1]
                    self.assertEqual(args[:3], ["api", "--method", "POST"])
                    self.assertIn("repos/else-bot/else/statuses/" + "a" * 40, args)
                    self.assertIn(f"state={expected}", args)
                    self.assertIn("context=release-ready", args)
                    self.assertIn("target_url=https://github.com/else-bot/else/actions/runs/123", args)
            failed = subprocess.run(["bash", "-e", "-c", self.status_step("Report readiness")],
                                    env={**env, "RESULT": "success", "FAKE_GH_API_STATUS": "1"},
                                    capture_output=True, text=True)
            self.assertNotEqual(failed.returncode, 0, "a failed status publication must fail the workflow")

    def test_readiness_waits_for_the_post_pin_dispatch(self):
        workflow = (ROOT / ".github/workflows/release-ready.yml").read_text()
        triggers = re.search(r"(?m)^on:\n((?:[ \t].*\n|\n)*)", workflow)
        self.assertIsNotNone(triggers)
        events = re.findall(r"(?m)^  ([a-z_]+):", triggers.group(1))
        self.assertEqual(events, ["workflow_dispatch"],
                         "release-ready must wait for guest-image to dispatch it after pinning")
        guest = (ROOT / ".github/workflows/guest-image.yml").read_text()
        self.assertIn("gh workflow run release-ready.yml", guest,
                      "guest-image must dispatch readiness after pinning")
        self.assertIn("  statuses: write\n", workflow)
        self.assertIn("  verify-release:\n", workflow)
        self.assertNotIn("  release-ready:\n", workflow,
                         "the workflow job must not conflict with the required commit status name")
        self.assertIn("      - name: Report readiness\n        if: always()\n", workflow,
                      "a failed gate must also report its outcome")


if __name__ == "__main__":
    unittest.main()
