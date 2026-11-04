#!/usr/bin/env python3
"""Offline checks that concurrent runs and checkouts own distinct mutable resources."""
import fcntl
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]

# Each fake command appends {"tool", "argv", ...} to $CALLS, one JSON object per line.
RECORDER = r'''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
tool, argv = Path(sys.argv[0]).name, sys.argv[1:]
call = {"tool": tool, "argv": argv}
def record():
    with open(os.environ["CALLS"], "a") as calls:
        calls.write(json.dumps(call) + "\n")
def option(name):
    return argv[argv.index(name) + 1]
'''

FAKES = {
    "docker": RECORDER + r'''
if argv[:1] == ["compose"] and "port" in argv:
    project = option("--project-name")
    print("127.0.0.1:" + str(40000 + sum(project.encode()) % 1000))
elif argv[:1] == ["build"]:
    iid = "sha256:" + os.urandom(8).hex()
    Path(option("--iidfile")).write_text(iid + "\n")
    call["iid"] = iid
elif argv[:1] == ["save"]:
    Path(option("-o")).write_text("archive of " + argv[1])
elif argv[:1] == ["run"]:
    source = next(v.split(":")[0] for v in argv if v.endswith(":/src:ro"))
    call["src"] = sorted(str(p.relative_to(source)) for p in Path(source).rglob("*")
                         if p.is_file() and ".git" not in p.relative_to(source).parts)
record()
''',
    "cargo": RECORDER + r'''
call["env"] = {k: v for k, v in os.environ.items() if k.startswith(("ELSE_TEST_", "COMPOSE_"))}
env_file = Path(os.environ["ELSE_TEST_COMPOSE_ENV_FILE"])
call["env_file"] = env_file.read_text()
record()
sys.exit(int(os.environ.get("CARGO_STATUS", "0")))
''',
    "msb": RECORDER + r'''
call["msb_home"] = os.environ["MSB_HOME"]
call["input"] = option("--input")
call["archive"] = Path(call["input"]).read_text()
record()
sys.exit(int(os.environ.get("MSB_STATUS", "0")))
''',
}


class Fakes:
    def __init__(self, test):
        directory = tempfile.TemporaryDirectory()
        test.addCleanup(directory.cleanup)
        self.work = Path(directory.name)
        self.bin = self.work / "bin"
        self.bin.mkdir()
        for name, body in FAKES.items():
            (self.bin / name).write_text(body)
            (self.bin / name).chmod(0o755)
        self.calls = self.work / "calls.jsonl"

    def env(self, **extra):
        env = dict(os.environ, PATH=f"{self.bin}{os.pathsep}{os.environ['PATH']}", CALLS=str(self.calls))
        env.update(extra)
        return env

    def take(self):
        if not self.calls.exists():
            return []
        calls = [json.loads(line) for line in self.calls.read_text().splitlines()]
        self.calls.unlink()
        return calls


class ReplicationRunnerTests(unittest.TestCase):
    def run_once(self, fakes, cargo_status):
        env = fakes.env(CARGO_STATUS=str(cargo_status), COMPOSE_PROJECT_NAME="else-dev",
                        ELSE_TEST_S3_ENDPOINT="http://127.0.0.1:8333",
                        ELSE_TEST_COMPOSE_PROJECT="else-dev")
        result = subprocess.run([sys.executable, str(ROOT / "scripts/test-replication.py"), "--", "retention"],
                                env=env, capture_output=True, text=True, timeout=60)
        return result, fakes.take()

    def test_each_run_owns_its_emulator_endpoint_and_removes_exactly_it(self):
        fakes = Fakes(self)
        projects = []
        for cargo_status in [0, 3]:
            result, calls = self.run_once(fakes, cargo_status)
            self.assertEqual(result.returncode, cargo_status, result.stderr)
            self.assertEqual([c["tool"] for c in calls], ["docker", "docker", "cargo", "docker"])
            up, port, cargo, down = calls
            project = up["argv"][up["argv"].index("--project-name") + 1]
            projects.append(project)
            for call in [up, port, down]:
                self.assertEqual(call["argv"][call["argv"].index("--project-name") + 1], project)
            self.assertEqual(up["argv"][-3:], ["up", "-d", "--wait"])
            self.assertIn("--volumes", down["argv"])
            expected_port = 40000 + sum(project.encode()) % 1000
            self.assertEqual(cargo["env"]["ELSE_TEST_S3_ENDPOINT"], f"http://127.0.0.1:{expected_port}")
            self.assertEqual(cargo["env"]["ELSE_TEST_COMPOSE_PROJECT"], project)
            self.assertNotIn("COMPOSE_PROJECT_NAME", cargo["env"])
            self.assertIn("ELSE_TEST_S3_ACCESS_KEY_ID=elsetestaccess01", cargo["env_file"])
            self.assertEqual(cargo["argv"][-3:], ["--", "--ignored", "retention"])
            self.assertFalse(Path(cargo["env"]["ELSE_TEST_COMPOSE_ENV_FILE"]).exists())
        self.assertNotEqual(projects[0], projects[1])


class GuestImageExportTests(unittest.TestCase):
    def test_each_build_exports_its_own_image_id_and_removes_its_archive(self):
        fakes = Fakes(self)
        checkout = fakes.work / "checkout"
        (checkout / "scripts").mkdir(parents=True)
        (checkout / "infra/guest").mkdir(parents=True)
        shutil.copy2(ROOT / "scripts/build-guest-image.sh", checkout / "scripts")
        loaded = []
        for msb_status in [0, 4]:
            data = fakes.work / f"data-{msb_status}"
            tmp = fakes.work / f"tmp-{msb_status}"
            tmp.mkdir()
            result = subprocess.run(["bash", str(checkout / "scripts/build-guest-image.sh"), str(data)],
                                    env=fakes.env(ELSE_GUEST_TAG="else-guest:9", ELSE_MSB_BIN="msb",
                                                  MSB_STATUS=str(msb_status), TMPDIR=str(tmp)),
                                    capture_output=True, text=True, timeout=60)
            self.assertEqual(result.returncode, msb_status, result.stderr)
            build, save, load = fakes.take()
            self.assertNotIn("-t", build["argv"])
            self.assertNotIn("--tag", build["argv"])
            self.assertEqual(save["argv"][1], build["iid"])
            self.assertEqual(load["archive"], "archive of " + build["iid"])
            self.assertEqual(load["argv"][-2:], ["--tag", "else-guest:9"])
            self.assertEqual(load["msb_home"], str(data / "microsandbox"))
            self.assertEqual(list(tmp.iterdir()), [], "the run's temporary archive was removed")
            loaded.append(build["iid"])
        self.assertNotEqual(loaded[0], loaded[1])


@unittest.skipUnless(sys.platform.startswith("linux") and shutil.which("flock"), "the installer matrix is Linux-only")
class InstallMatrixIsolationTests(unittest.TestCase):
    def checkout(self, path):
        (path / "scripts").mkdir(parents=True)
        (path / "apps/api/migrations").mkdir(parents=True)
        shutil.copy2(ROOT / "scripts/test-install.sh", path / "scripts")
        (path / "install.sh").write_text("echo install\n")
        (path / "Cargo.toml").write_text('[workspace.package]\nversion = "0.0.5"\n')
        (path / "Cargo.lock").write_text('[[package]]\nname = "else-api"\nversion = "0.0.5"\n\n'
                                         '[[package]]\nname = "else-desktop"\nversion = "0.0.5"\n')
        (path / "apps/api/migrations/0001_base.sql").write_text("SELECT 1;\n")
        (path / "deleted").touch()
        (path / ".gitignore").write_text(".env\ntarget/\n.else-dev/\n")
        subprocess.run(["git", "init", "-q", str(path)], check=True)
        subprocess.run(["git", "-C", str(path), "add", "."], check=True)
        (path / "deleted").unlink()
        (path / ".env").write_text("SECRET=private\n")
        (path / "target").mkdir()
        (path / "target/artifact").write_text("other revision")
        (path / "untracked-edit").write_text("kept\n")
        return path

    def matrix(self, fakes, checkout):
        result = subprocess.run(["bash", str(checkout / "scripts/test-install.sh"), "debian-12"],
                                env=fakes.env(GIT_CONFIG_GLOBAL=os.devnull), capture_output=True,
                                text=True, timeout=120)
        runs = [c for c in fakes.take() if c["argv"][:1] == ["run"]]
        volumes = sorted(v.split(":")[0] for c in runs for v in c["argv"] if ":/home/tester/" in v)
        return result, runs, volumes

    def test_checkouts_own_distinct_caches_and_snapshots_exclude_ignored_state(self):
        fakes = Fakes(self)
        first = self.checkout(fakes.work / "first")
        second = self.checkout(fakes.work / "second")
        result, runs, first_volumes = self.matrix(fakes, first)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(len(first_volumes), 2)
        source = runs[0]["src"]
        self.assertIn("install.sh", source)
        self.assertIn("untracked-edit", source)
        for excluded in [".env", "target/artifact", "deleted"]:
            self.assertNotIn(excluded, source)
        _, _, again = self.matrix(fakes, first)
        self.assertEqual(again, first_volumes, "a checkout keeps its persistent caches")
        _, _, second_volumes = self.matrix(fakes, second)
        self.assertFalse(set(first_volumes) & set(second_volumes))
        self.assertEqual(len(list((first / "target/install-tests").glob("*/debian-12.log"))), 2)

    def test_a_second_run_in_the_same_checkout_is_refused(self):
        fakes = Fakes(self)
        checkout = self.checkout(fakes.work / "checkout")
        lock = checkout / ".else-dev/install-cache.lock"
        lock.parent.mkdir(parents=True)
        with lock.open("w") as held:
            fcntl.flock(held, fcntl.LOCK_EX)
            result, runs, _ = self.matrix(fakes, checkout)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("owns its install caches", result.stderr)
        self.assertEqual(runs, [])


if __name__ == "__main__":
    unittest.main()
