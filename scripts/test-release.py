#!/usr/bin/env python3
"""Behavior tests for the release lifecycle: scripts/release.sh and
scripts/guest-release.py against disposable git repositories, a fake OCI
registry, and fake gh and cargo commands.

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
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LABEL = "bot.else.guest.source_sha256"
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
        self.token_status = 200
        self.transient_failures = 0
        self.auth_requests = []
        self.tamper = set()
        registry = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                if self.path.startswith("/token"):
                    registry.auth_requests.append(self.headers.get("Authorization"))
                    return self.reply(registry.token_status, json.dumps({"token": "anon"}).encode())
                if self.headers.get("Authorization") != "Bearer anon":
                    return self.reply(401, b"{}", {
                        "WWW-Authenticate": f'Bearer realm="http://{registry.host}/token",service="fake",scope="repository:else-bot/else-guest:pull"'
                    })
                if registry.private:
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
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def put(self, document):
        body = json.dumps(document).encode()
        digest = "sha256:" + hashlib.sha256(body).hexdigest()
        self.objects[digest] = body
        return digest

    def publish(self, source, platforms=("linux/amd64", "linux/arm64"), labels=None, tag=None):
        entries = []
        for platform in platforms:
            os_, arch = platform.split("/")
            label = (labels or {}).get(platform, source)
            config = self.put({"os": os_, "architecture": arch, "config": {"Labels": {LABEL: label}}})
            manifest = self.put({"schemaVersion": 2, "mediaType": MANIFEST,
                                 "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config},
                                 "layers": []})
            entries.append({"mediaType": MANIFEST, "digest": manifest, "platform": {"os": os_, "architecture": arch}})
        entries.append({"mediaType": MANIFEST, "digest": self.put({"attestation": True}),
                        "platform": {"os": "unknown", "architecture": "unknown"}})
        index = self.put({"schemaVersion": 2, "mediaType": INDEX, "manifests": entries})
        if tag:
            self.tags[tag] = index
        return f"{self.repo}@{index}"


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
        self.registry.token_status = 200
        self.registry.transient_failures = 0
        self.registry.auth_requests.clear()
        self.registry.tamper.clear()
        self.tmp = Path(tempfile.mkdtemp(prefix="else-release-test-"))
        self.addCleanup(shutil.rmtree, self.tmp)
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        for name, text in (("gh", FAKE_GH), ("cargo", FAKE_CARGO)):
            (bin_dir / name).write_text(text)
            (bin_dir / name).chmod(0o755)
        self.gh_state = self.tmp / "gh.json"
        self.env = {
            **os.environ,
            "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
            "FAKE_GH_STATE": str(self.gh_state),
            "ELSE_GUEST_IMAGE_REPO": self.registry.repo,
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
        (self.work / "scripts").mkdir()
        for name in ("release.sh", "guest-release.py"):
            shutil.copy2(ROOT / "scripts" / name, self.work / "scripts" / name)
        self.write("infra/guest-image.json", json.dumps({"schema": 1, "source_sha256": "0" * 64, "image": None}))
        self.commit("initial")
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
        self.registry.token_status = 403
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


    def test_repository_manifest_and_workflows(self):
        manifest = json.loads((ROOT / "infra/guest-image.json").read_text())
        self.assertEqual(set(manifest), {"schema", "source_sha256", "image"})
        if manifest["image"] is not None:
            self.assertRegex(manifest["image"], r"^ghcr\.io/else-bot/else-guest@sha256:[0-9a-f]{64}$")
        self.assertRegex(manifest["source_sha256"], r"^[0-9a-f]{64}$")
        for workflow in (ROOT / ".github/workflows").glob("*.yml"):
            for line in workflow.read_text().splitlines():
                if "uses:" in line:
                    ref = line.split("uses:", 1)[1].split("#")[0].strip()
                    self.assertRegex(ref, r"^[\w.-]+/[\w.-]+@[0-9a-f]{40}$", f"{workflow.name} pins actions by commit")
            self.assertNotIn("pull_request_target", workflow.read_text())


class ReleaseReadinessWorkflow(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
