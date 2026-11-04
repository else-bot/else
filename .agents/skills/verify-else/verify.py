#!/usr/bin/env python3
"""Fast native Linux amd64 verification. Build on host, drive in a private omabox."""

import argparse
import importlib.util
from concurrent.futures import ThreadPoolExecutor, as_completed
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[3]
SCRIPT = Path(__file__).resolve()
# scripts/dev.py owns the client side of the API's api.json contract.
_spec = importlib.util.spec_from_file_location("else_dev", ROOT / "scripts/dev.py")
dev = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(dev)
SEPARATE_CHECKS = "Native desktop input, doctests, provider subscriptions, real VMs, S3 replication, installer updates, arm64 and macOS"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def clean_env():
    return {k: v for k, v in os.environ.items()
            if not k.startswith(("ELSE_", "PG")) and k not in {
                "DATABASE_URL", "TEST_DATABASE_URL", "SECRET_KEY", "SECRET_KEY_FILE",
                "OWNER_TOKEN_FILE", "CODEX_HOME", "CLAUDE_CONFIG_DIR"}}


def command(argv, *, timeout=600, **kwargs):
    """Terminate only our process group, including child processes, on failure or interruption."""
    with subprocess.Popen(argv, start_new_session=True, **kwargs) as child:
        try:
            code = child.wait(timeout=timeout)
            if code:
                raise subprocess.CalledProcessError(code, argv)
        finally:
            # Descendants may outlive a test process even after its normal exit.
            try:
                os.killpg(child.pid, signal.SIGTERM)
                child.wait(timeout=5)
            except ProcessLookupError:
                pass
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()


def capture(argv):
    return subprocess.check_output(argv, cwd=ROOT, text=True, timeout=60).strip()


def until(probe, check, description, timeout=15):
    deadline = time.monotonic() + timeout
    while True:
        value = probe()
        if check(value):
            return value
        if time.monotonic() >= deadline:
            raise RuntimeError(f"timed out: {description}; last state: {value}")
        time.sleep(0.025)


class Instance:
    def __init__(self, binary, evidence):
        self.binary = binary
        self.evidence = evidence
        self.child = None
        self.log = None
        self.scratch = tempfile.TemporaryDirectory(prefix="else-verify-")
        self.work = Path(self.scratch.name)
        self.data = self.work / "data"
        self.env = clean_env()
        self.env.update(ELSE_DATA_DIR=str(self.data), ELSE_BOTS_DIR=str(self.work / "projects"),
                        ELSE_SYNTHETIC_PROVIDER="1", ELSE_BACKUP_INTERVAL_SECS="0",
                        ELSE_CODEX_BIN="/nonexistent/verify-codex",
                        ELSE_CLAUDE_BIN="/nonexistent/verify-claude", ELSE_MSB_BIN="/nonexistent/verify-msb",
                        TOKIO_WORKER_THREADS="2")
        self.token = None
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def __enter__(self):
        try:
            with (self.evidence / "setup.log").open("w") as log:
                command([self.binary, "setup"], cwd=self.work, env=self.env, stdout=log,
                        stderr=subprocess.STDOUT, timeout=30)
            self.token = (self.data / "owner-token").read_text().strip()
            self.start()
            require(len(self.request("GET", "/v1/bots")["bots"]) == 1, "setup must seed exactly one main bot")
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def start(self):
        require(self.child is None, "the owned API is already running")
        # The previous child of this instance is reaped, so a leftover manifest is ours and stale.
        (self.data / "api.json").unlink(missing_ok=True)
        self.env["ELSE_API_BIND"] = "127.0.0.1:0"
        self.log = (self.evidence / "api.log").open("a")
        self.child = subprocess.Popen([self.binary, "serve"], cwd=self.work, env=self.env,
                                      stdout=self.log, stderr=subprocess.STDOUT, start_new_session=True)

        def published():
            require(self.child.poll() is None, "API exited before publishing api.json; see api.log")
            return dev.published_url(self.data, self.child.pid)

        self.url = until(published, bool, f"api.json from pid {self.child.pid}")

        def ready():
            require(self.child.poll() is None, "API exited before readiness; see api.log")
            try:
                return self.request("GET", "/readyz", record=False)
            except (OSError, urllib.error.URLError):
                return None

        until(ready, lambda v: v and v.get("status") == "ready", "database readiness")
        self.doctor()

    def doctor(self):
        require(self.child.poll() is None, "owned API process is dead")
        version = capture([self.binary, "version"]).split()[-1]
        health = self.request("GET", "/healthz")
        require(health["version"] == version, "API version differs from built binary")
        listing = self.request("GET", "/v1/bots")
        require(any(b["id"] == listing["default_bot_id"] for b in listing["bots"]), "main bot missing")
        caps = self.request("GET", "/v1/capabilities")
        require(any(p["id"] == "synthetic" and p["available"] for p in caps["providers"]),
                "Synthetic provider unavailable")
        with sqlite3.connect(f"file:{self.data / 'else.sqlite3'}?mode=ro", uri=True) as db:
            main = db.execute("SELECT default_bot_id FROM installation").fetchone()
        main_id = str(uuid.UUID(bytes=main[0])) if main and main[0] else None
        require(main_id == listing["default_bot_id"], "listener differs from owned database")
        (self.evidence / "doctor.json").write_text(json.dumps({
            "pid": self.child.pid, "url": self.url, "version": version,
            "database": str(self.data / "else.sqlite3"), "main_bot_id": main_id,
            "authenticated": True, "synthetic_available": True}, indent=2) + "\n")

    def request(self, method, path, body=None, expected=200, auth=True, record=True):
        headers = {"Content-Type": "application/json"}
        if auth:
            headers["Authorization"] = f"Bearer {self.token}"
        request = urllib.request.Request(self.url + path, method=method, headers=headers,
                                         data=json.dumps(body).encode() if body is not None else None)
        try:
            response = self.opener.open(request, timeout=3)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            status, payload = response.code, json.load(response)
        if record:
            with (self.evidence / "http.jsonl").open("a") as log:
                log.write(json.dumps({"method": method, "path": path, "request": body,
                                      "authenticated": auth, "status": status, "response": payload}) + "\n")
        codes = expected if isinstance(expected, tuple) else (expected,)
        require(status in codes, f"{method} {path}: expected {codes}, got {status}: {payload}")
        return payload

    def stop(self):
        if self.child is not None:
            try:
                os.killpg(self.child.pid, signal.SIGTERM)
                self.child.wait(timeout=15)
            except ProcessLookupError:
                pass
            except subprocess.TimeoutExpired:
                os.killpg(self.child.pid, signal.SIGKILL)
                self.child.wait()
            self.child = None
        if self.log is not None:
            self.log.close()
            self.log = None

    def __exit__(self, *_):
        try:
            self.stop()
        finally:
            self.scratch.cleanup()


def create_bot(api):
    body = {"request_id": str(uuid.uuid4()), "name": "Verification", "role": "Synthetic verification"}
    bot = api.request("POST", "/v1/bots", body, expected=201)["bot"]
    retry = api.request("POST", "/v1/bots", body)["bot"]
    require(bot["id"] == retry["id"], "retry created a second bot")
    return bot


def bots(api):
    listing = api.request("GET", "/v1/bots")
    api.request("GET", "/v1/bots", expected=401, auth=False)
    main = listing["default_bot_id"]
    require(api.request("DELETE", f"/v1/bots/{main}", expected=409)["error"]["code"] == "main_bot",
            "main bot deletion must be refused")
    bot = create_bot(api)
    api.request("DELETE", f"/v1/bots/{bot['id']}", expected=(200, 202))
    until(lambda: api.request("GET", f"/v1/bots/{bot['id']}"),
          lambda v: v["bot"]["cleanup"] and v["bot"]["cleanup"]["state"] == "done", "bot cleanup finished")
    until(lambda: api.request("GET", "/v1/bots"),
          lambda v: all(b["id"] != bot["id"] for b in v["bots"]), "deleted bot hidden")
    with sqlite3.connect(f"file:{api.data / 'else.sqlite3'}?mode=ro", uri=True) as db:
        row = db.execute("SELECT status FROM bots WHERE id = ?", (uuid.UUID(bot["id"]).bytes,)).fetchone()
    require(row and row[0] == "deleted", "soft deletion did not retain the bot row")
    (api.evidence / "retained-row.json").write_text(json.dumps({"id": bot["id"], "status": row[0]}))


def messaging(api):
    bot = create_bot(api)
    path = f"/v1/bots/{bot['id']}"
    body = {"request_id": str(uuid.uuid4()), "text": "amd64 café ✓", "provider_id": "synthetic"}
    sent = api.request("POST", path + "/turns", body, expected=202)
    retry = api.request("POST", path + "/turns", body)
    require(sent["turn"]["id"] == retry["turn"]["id"], "retry duplicated a turn")
    snapshot = until(lambda: api.request("GET", path + "/conversation"),
                     lambda v: v["turns"] and v["turns"][-1]["status"] == "completed", "synthetic reply")
    expected = f"Synthetic reply ✓ from {bot['name']} (turn 1): {body['text']}"
    require([i["text"] for i in snapshot["items"]] == [body["text"], expected], "reply or retry history differs")
    hanging = api.request("POST", path + "/turns", {
        "request_id": str(uuid.uuid4()), "text": "[synthetic:hang]", "provider_id": "synthetic"}, expected=202)
    until(lambda: api.request("GET", path + "/conversation"),
          lambda v: any(t["id"] == hanging["turn"]["id"] and t["status"] == "running" for t in v["turns"]),
          "hanging turn started")
    cancel = {"request_id": str(uuid.uuid4())}
    until(lambda: api.request("POST", path + "/cancel", cancel, expected=(200, 202)),
          lambda v: v["state"] == "done", "stop work receipt")
    before = api.request("GET", path + "/conversation")
    require(before["turns"][-1]["status"] == "interrupted", "stop did not interrupt the turn")
    api.stop()
    api.start()
    after = api.request("GET", path + "/conversation")
    require(before["items"] == after["items"] and before["turns"] == after["turns"],
            "restart changed persisted conversation")
    require(before["event_cursor"] != after["event_cursor"], "restart reused an old event cursor")


def categories(api):
    bot = create_bot(api)
    body = {"request_id": str(uuid.uuid4()), "name": "Family"}
    category = api.request("POST", "/v1/categories", body, expected=201)["category"]
    path = f"/v1/categories/{category['id']}"
    api.request("POST", "/v1/categories", {**body, "request_id": str(uuid.uuid4()), "name": "FAMILY"}, expected=409)
    api.request("PATCH", f"/v1/bots/{bot['id']}/category", {"category_id": category["id"]})
    api.request("PATCH", path, {"name": "Home"})
    listing = api.request("GET", "/v1/bots")
    require(any(c["name"] == "Home" for c in listing["categories"]), "rename missing from sidebar data")
    require(next(b for b in listing["bots"] if b["id"] == bot["id"])["category_id"] == category["id"],
            "assignment missing from sidebar data")
    api.request("DELETE", path)
    after = api.request("GET", "/v1/bots")
    require(after["categories"] == [], "deleted category still listed")
    require(next(b for b in after["bots"] if b["id"] == bot["id"])["category_id"] is None,
            "category deletion removed its bot or left assignment")


def worker(spec):
    output = Path.home() / "else-verification" / spec["run_id"]
    output.mkdir(parents=True)
    results = {}
    jobs = [("bots", bots), ("messaging", messaging), ("categories", categories)]
    jobs += [(f"test-{item['name']}", item) for item in spec["tests"]]
    # Start the longest existing suites early so their waits overlap the short journeys.
    long = ["test-delegation", "test-backup", "test-task_secrets"]
    jobs.sort(key=lambda job: long.index(job[0]) if job[0] in long else len(long))

    def execute(name, feature):
        started = time.monotonic()
        evidence = output / name
        evidence.mkdir()
        result = {"status": "failed"}
        try:
            if callable(feature):
                with Instance(spec["api"], evidence) as api:
                    feature(api)
            else:
                with tempfile.TemporaryDirectory(prefix="else-test-") as work, (evidence / "test.log").open("w") as log:
                    command([feature["executable"], "--test-threads", str(spec["test_threads"])],
                            cwd=work, env={**clean_env(), "TOKIO_WORKER_THREADS": "2"}, stdout=log, stderr=subprocess.STDOUT,
                            timeout=spec["timeout"])
                summaries = re.findall(r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored", (evidence / "test.log").read_text())
                require(summaries, "test binary produced no libtest result")
                result["counts"] = [dict(zip(["passed", "failed", "ignored"], map(int, row))) for row in summaries]
            result["status"] = "passed"
        except Exception as error:
            result["error"] = str(error)
        result["seconds"] = round(time.monotonic() - started, 3)
        return result

    with ThreadPoolExecutor(max_workers=spec["jobs"]) as pool:
        pending = {pool.submit(execute, name, feature): name for name, feature in jobs}
        for future in as_completed(pending):
            name = pending[future]
            results[name] = future.result()
            print(f"{name}: {results[name]['status']} ({results[name]['seconds']}s)", flush=True)
    report = {"status": "passed" if all(r["status"] == "passed" for r in results.values()) else "failed",
              "lanes": results, "not_covered": SEPARATE_CHECKS, "excluded_targets": spec["excluded_targets"]}
    (output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    return 0 if report["status"] == "passed" else 1


def positive(value):
    value = int(value)
    if value < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return value


def native_session(name, binary):
    """Keep a seeded API alive in the box while an agent drives the real desktop."""
    evidence = Path.home() / "else-verification" / name
    evidence.mkdir(parents=True, exist_ok=False)
    with Instance(str(binary), evidence) as api:
        api.request("POST", "/v1/onboarding", {"name": "Verification Assistant", "profile": {
            "provider_id": "codex", "model": "gpt-6.1-sol", "reasoning_effort": "medium", "service_tier": "priority"}})
        require(not api.request("GET", "/v1/capabilities")["onboarding_required"], "welcome screen incomplete")
        session = {"pid": api.child.pid, "url": api.url, "data": str(api.data)}
        (evidence / "session.json").write_text(json.dumps(session) + "\n")
        (evidence / "desktop.env").write_text(
            f"ELSE_API_URL={api.url}\nELSE_OWNER_TOKEN_FILE={api.data / 'owner-token'}\n"
            f"ELSE_DESKTOP_CONFIG_FILE={evidence / 'desktop' / 'desktop.json'}\n")
        print(f"Ready: {api.url}; session {name}", flush=True)
        try:
            code = api.child.wait(timeout=3600)
            raise RuntimeError(f"native verification API exited: {code}")
        except subprocess.TimeoutExpired:
            raise RuntimeError("native verification session expired after one hour")


def native_probe(name, args):
    evidence = Path.home() / "else-verification" / name
    state = json.loads((evidence / "session.json").read_text())
    require(dev.published_url(state["data"], state["pid"]) == state["url"] and
            dev.holds(state["pid"], Path(state["data"]) / "api.lock"),
            f"session {name}'s API (pid {state['pid']}) no longer owns {state['data']}")
    api = object.__new__(Instance)
    api.evidence = evidence
    api.url = state["url"]
    api.token = (Path(state["data"]) / "owner-token").read_text().strip()
    api.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    payload = api.request(args.method, args.path, json.loads(args.body) if args.body else None,
                          expected=tuple(args.expected))
    print(json.dumps(payload, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--full", action="store_true", help="also build workspace and run all offline test binaries")
    parser.add_argument("--tests", nargs="+", help="run selected existing integration targets in parallel")
    cpus = len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count() or 1
    parser.add_argument("--jobs", type=positive, default=min(4, cpus))
    parser.add_argument("--test-threads", type=positive, default=2)
    parser.add_argument("--timeout", type=positive, default=180, help="deadline per test binary in seconds")
    parser.add_argument("--output", type=Path, help="new evidence directory, default target/verification/<unique run>")
    parser.add_argument("--worker", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--session", help="inside omabox only: hold a seeded API for native desktop verification")
    parser.add_argument("--api-binary", type=Path, default=ROOT / "target/debug/else-api", help="built API binary for --session")
    parser.add_argument("--probe", help="inside omabox only: read a session or call its public API")
    parser.add_argument("--method", default="GET", choices=["GET", "POST", "PATCH", "PUT", "DELETE"])
    parser.add_argument("--path", default="/v1/bots")
    parser.add_argument("--body", help="JSON request body for --probe")
    parser.add_argument("--expected", type=int, nargs="+", default=[200], help="expected HTTP statuses for --probe")
    args = parser.parse_args()

    def interrupted(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    if args.worker:
        require(os.environ.get("WAYLAND_DISPLAY") not in {None, "omabox-guard"}, "run workers inside omabox")
        return worker(json.loads(args.worker.read_text()))
    if args.session or args.probe:
        require(os.environ.get("WAYLAND_DISPLAY") not in {None, "omabox-guard"}, "run session/probe inside omabox")
        require(re.fullmatch(r"[a-zA-Z0-9_-]+", args.session or args.probe), "session name must be a simple directory name")
        if args.session:
            native_session(args.session, args.api_binary)
        else:
            native_probe(args.probe, args)
        return 0
    require(platform.system() == "Linux" and platform.machine() == "x86_64", "this pass requires native Linux amd64")
    require(shutil.which("omabox"), "omabox is required; read the omabox skill before running")
    run_id = time.strftime("%Y%m%d-%H%M%S") + "-" + uuid.uuid4().hex[:8]
    output = (args.output or ROOT / "target/verification" / run_id).resolve()
    output.mkdir(parents=True, exist_ok=False)
    metadata = json.loads(capture(["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"]))
    targets = sorted(t["name"] for p in metadata["packages"] for t in p["targets"] if "test" in t["kind"])
    offline = [name for name in targets if not name.endswith("_live") and name != "replication"]
    selected = offline if args.full else args.tests or []
    require(all(t in offline for t in selected), "--tests must name offline targets: " + ", ".join(offline))
    started = time.monotonic()
    report = {"revision": capture(["git", "rev-parse", "HEAD"]), "working_tree": capture(["git", "status", "--porcelain"]),
              "rustc": capture(["rustc", "-Vv"]), "architecture": platform.machine(),
              "jobs": args.jobs, "test_threads": args.test_threads, "status": "failed", "not_covered": SEPARATE_CHECKS}
    box = None
    try:
        build_start = time.monotonic()
        with (output / "build.jsonl").open("w") as artifacts, (output / "build.log").open("w") as log:
            package = ["--workspace"] if args.full else ["-p", "else-api"]
            command(["cargo", "build", *package, "--locked", "--bins", "--message-format=json"],
                    cwd=ROOT, stdout=artifacts, stderr=log, timeout=1800)
            if args.full or selected:
                command(["cargo", "test", *package, "--locked", "--no-run", "--message-format=json", "--lib", "--bins",
                         *[flag for name in selected for flag in ["--test", name]]],
                        cwd=ROOT, stdout=artifacts, stderr=log, timeout=1800)
        report["build_seconds"] = round(time.monotonic() - build_start, 3)
        frames = [json.loads(line) for line in (output / "build.jsonl").read_text().splitlines()]
        built = [f for f in frames if f.get("reason") == "compiler-artifact" and f.get("executable")]
        api = next(f["executable"] for f in built if f["target"]["name"] == "else-api" and not f["profile"]["test"])
        tests = [{"name": f["target"]["name"], "executable": f["executable"]} for f in built if f["profile"]["test"]]
        spec = {"run_id": run_id, "api": api, "tests": tests, "jobs": args.jobs,
                "test_threads": args.test_threads, "timeout": args.timeout,
                "excluded_targets": sorted(set(targets) - set(selected))}
        spec_file = output / "spec.json"
        spec_file.write_text(json.dumps(spec, indent=2) + "\n")
        # Input must be visible read-only even when --output or Cargo's cache is outside this checkout.
        mounts = {str(output), metadata["target_directory"]}
        # rust-embed reads assets from dependency sources in dev builds, including age's error strings.
        host = next(line.split(": ", 1)[1] for line in report["rustc"].splitlines() if line.startswith("host: "))
        dependencies = json.loads(capture(["cargo", "metadata", "--locked", "--offline", "--filter-platform", host,
                                           "--format-version", "1"]))
        age = next(p for p in dependencies["packages"] if p["name"] == "age")
        mounts.add(str(Path(age["manifest_path"]).parent / "i18n"))
        box_info = json.loads(capture(["omabox", "up", "--new", "--net", "isolated", "--no-shell", "--json",
                                      *[arg for path in sorted(mounts) for arg in ["--ro-bind", path]]]))
        box = box_info["name"]
        box_home = Path(capture(["omabox", "path", "-b", box, "--home"]))
        worker_start = time.monotonic()
        try:
            with (output / "worker.log").open("w") as log:
                command(["omabox", "run", "-b", box, "--", "python3", str(SCRIPT), "--worker", str(spec_file)],
                        stdout=log, stderr=subprocess.STDOUT, timeout=max(300, len(tests) * args.timeout))
        finally:
            evidence = box_home / "else-verification" / run_id
            if evidence.exists():
                shutil.copytree(evidence, output / "evidence")
                if (output / "evidence/results.json").exists():
                    report.update(json.loads((output / "evidence/results.json").read_text()))
            report["drive_seconds"] = round(time.monotonic() - worker_start, 3)
    except Exception as error:
        report.update(status="failed", error=str(error))
    finally:
        if box:
            try:
                command(["omabox", "down", "-b", box], timeout=45)
                report["cleanup"] = "owned box removed; worker scratch directories removed"
            except Exception as error:
                report.update(status="failed", cleanup_error=str(error))
        report["total_seconds"] = round(time.monotonic() - started, 3)
        (output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2))
        print(f"Evidence: {output}")
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
