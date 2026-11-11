#!/usr/bin/env python3
"""Isolated else.bot development sessions, one state directory per checkout and session.

Host (manual development, real provider logins allowed):
  dev.py reset | api | desktop | status | curl ARGS... | sql QUERY | drop   [--session NAME] [--dir PATH]
Agents (disposable omabox on Linux, Synthetic only, foreground until Ctrl-C):
  dev.py run [--api-only]
  dev.py test [-p PACKAGE] [--test NAME] [--jobs N] [--serial] [-- FILTER]
"""

import argparse
from dataclasses import dataclass
import fcntl
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import signal
import sqlite3
import stat
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
ICON = ROOT / "media/branding/icons/else-bot.icns"
SCRIPT = Path(__file__).resolve()
MARKER = ".else-dev-session"
NAME = re.compile(r"(?!runs$)[A-Za-z0-9_-]{1,64}")
LOOPBACK = re.compile(r"http://127\.0\.0\.1:[1-9][0-9]{0,4}")
DROPPED = {"DATABASE_URL", "TEST_DATABASE_URL", "SECRET_KEY", "SECRET_KEY_FILE", "OWNER_TOKEN_FILE"}
INHERITED = {"ELSE_CODEX_BIN", "ELSE_CODEX_ALLOW_UNVERIFIED", "ELSE_CLAUDE_BIN", "ELSE_GROK_BIN", "ELSE_MSB_BIN",
             "ELSE_MSB_ALLOW_UNVERIFIED", "ELSE_VM_IMAGE", "ELSE_VM_DESKTOP", "ELSE_VM_NETWORK",
             "ELSE_MAX_ACTIVE_TURNS", "ELSE_SYNTHETIC_PROVIDER"}
DARWIN = sys.platform == "darwin"
MAC_RUN = ("dev.py run needs omabox, which is Linux only. On macOS run `scripts/dev.py api` and "
           "`scripts/dev.py desktop` in two terminals of this checkout. Agents verifying the native "
           "desktop use `python3 .agents/skills/verify-else/verify.py --session NAME --backend process "
           "--api-binary PATH --desktop-binary PATH`; see .agents/skills/verify-else/native-desktop.md")


class Refused(Exception):
    pass


def require(condition, message):
    if not condition:
        raise Refused(message)


def until(probe, description, timeout):
    deadline = time.monotonic() + timeout
    while (value := probe()) is None:
        require(time.monotonic() < deadline, f"timed out: {description}")
        time.sleep(0.05)
    return value


def clean_env(host_providers):
    env = {k: v for k, v in os.environ.items()
           if (host_providers and k in INHERITED)
           or not (k.startswith(("ELSE_", "PG", "AWS_")) or k in DROPPED)}
    if not host_providers:
        for key in ("CODEX_HOME", "CLAUDE_CONFIG_DIR", "GROK_HOME"):
            env.pop(key, None)
        env.update(ELSE_CODEX_BIN="/nonexistent/else-dev-codex", ELSE_CLAUDE_BIN="/nonexistent/else-dev-claude",
                   ELSE_GROK_BIN="/nonexistent/else-dev-grok",
                   ELSE_MSB_BIN="/nonexistent/else-dev-msb")
    return env


def published_url(data, pid):
    """The URL from the API's own manifest, or None until `pid` has published it."""
    path = Path(data) / "api.json"
    try:
        mode = os.lstat(path).st_mode
    except FileNotFoundError:
        return None
    require(stat.S_ISREG(mode), f"{path} is not a regular file")
    try:
        manifest = json.loads(path.read_text())
    except json.JSONDecodeError:
        return None
    if manifest.get("pid") != pid:
        return None
    require(isinstance(manifest.get("url"), str) and LOOPBACK.fullmatch(manifest["url"]),
            f"{path} does not name a loopback URL")
    return manifest["url"]


def publish_app(binary, root, identifier, name):
    """Publish a complete, immutable macOS app and return its executable."""
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    staged = root / f".staging-{uuid.uuid4().hex}"
    contents = staged / f"{name}.app/Contents"
    try:
        (contents / "MacOS").mkdir(parents=True)
        shutil.copy2(binary, contents / "MacOS/else-desktop")
        (contents / "Resources").mkdir()
        shutil.copy2(ICON, contents / "Resources" / ICON.name)
        (contents / "Info.plist").write_bytes(plistlib.dumps({
            "CFBundleIdentifier": identifier, "CFBundleName": name, "CFBundleDisplayName": name,
            "CFBundleExecutable": "else-desktop", "CFBundlePackageType": "APPL",
            "CFBundleIconFile": ICON.name,
            "NSHighResolutionCapable": True}))
        digest = hashlib.sha256()
        for part in ("MacOS/else-desktop", "Info.plist", f"Resources/{ICON.name}"):
            with (contents / part).open("rb") as source:
                digest.update(hashlib.file_digest(source, "sha256").digest())
        final = root / digest.hexdigest()[:16]
        try:
            staged.rename(final)
        except OSError:
            require(final.is_dir(), f"cannot publish {final}")
        return final / f"{name}.app/Contents/MacOS/else-desktop"
    finally:
        shutil.rmtree(staged, ignore_errors=True)


def try_lock(path, create):
    """An exclusively flocked descriptor, None when another process holds it, or 0 when absent."""
    flags = os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC | (os.O_CREAT if create else 0)
    try:
        fd = os.open(path, flags, 0o600)
    except FileNotFoundError:
        return 0
    except OSError as error:
        raise Refused(f"cannot open lock {path}: {error}") from error
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return fd
    except BlockingIOError:
        os.close(fd)
        return None


@dataclass(frozen=True)
class Session:
    checkout: Path
    name: str
    state: Path

    @classmethod
    def resolve(cls, name, directory=None):
        require(NAME.fullmatch(name), f"session name must match {NAME.pattern}")
        return cls(ROOT, name, Path(os.path.abspath(directory)) if directory else ROOT / ".else-dev" / name)

    data = property(lambda self: self.state / "data")
    database = property(lambda self: self.data / "else.sqlite3")
    token_file = property(lambda self: self.data / "owner-token")
    lock_path = property(lambda self: self.state.parent / f".{self.state.name}.lock")
    owner = property(lambda self: {"checkout": str(self.checkout), "session": self.name})
    label = property(lambda self: f"else.bot · {self.checkout.name} · {self.name} · {self.bundle_id[-6:]}")
    bundle_id = property(lambda self: "bot.else.dev." + hashlib.sha256(
        json.dumps([str(self.checkout), self.name, str(self.state)]).encode()).hexdigest()[:16])

    def app(self, binary):
        return publish_app(binary, self.state / "apps", self.bundle_id, self.label)

    def env(self, host_providers):
        env = clean_env(host_providers)
        env.update(ELSE_DATA_DIR=str(self.data), DATABASE_URL=f"sqlite://{self.database}",
                   OWNER_TOKEN_FILE=str(self.token_file), SECRET_KEY_FILE=str(self.data / "secret-key"),
                   ELSE_BOTS_DIR=str(self.state / "projects"), ELSE_BACKUP_DIR=str(self.data / "backups"),
                   ELSE_BACKUP_INTERVAL_SECS="0", ELSE_API_BIND="127.0.0.1:0",
                   ELSE_SYNTHETIC_PROVIDER=env.get("ELSE_SYNTHETIC_PROVIDER", "1"))
        return env

    def owned(self):
        """True when this session created the state, False when absent; refuses anything else."""
        try:
            mode = os.lstat(self.state).st_mode
        except FileNotFoundError:
            return False
        require(stat.S_ISDIR(mode), f"refusing: {self.state} is not a directory")
        marker = self.state / MARKER
        try:
            require(stat.S_ISREG(os.lstat(marker).st_mode), f"refusing: {marker} is not a regular file")
            owner = json.loads(marker.read_text())
        except (FileNotFoundError, json.JSONDecodeError):
            raise Refused(f"refusing: {self.state} was not created by scripts/dev.py") from None
        require(owner == self.owner, f"refusing: {self.state} belongs to {owner}, not {self.owner}")
        return True

    def lock(self):
        """Held by `api` for the API's lifetime and by reset/drop; never unlinked."""
        self.state.parent.mkdir(parents=True, exist_ok=True)
        fd = try_lock(self.lock_path, create=True)
        require(fd, f"refusing: session {self.name} is in use ({self.lock_path} is held)")
        return fd

    def runtime_locks(self):
        """Hold the API's own data and database locks so no independently started API is running."""
        self.data.mkdir(mode=0o700, exist_ok=True)
        paths = [self.data / "api.lock"]
        if os.path.lexists(self.database):
            paths.append(Path(f"{os.path.realpath(self.database)}.api.lock"))
        held = []
        for path in paths:
            fd = try_lock(path, create=True)
            if fd is None:
                for other in held:
                    os.close(other)
                raise Refused(f"refusing: an else.bot API is running on {self.data} ({path} is held)")
            if fd:
                held.append(fd)
        return held

    def remove(self):
        if not self.owned():
            return
        held = self.runtime_locks()
        try:
            removed = self.state.with_name(f".{self.state.name}.removed-{uuid.uuid4().hex}")
            self.state.rename(removed)
            shutil.rmtree(removed)
        finally:
            for fd in held:
                os.close(fd)

    def prepare(self, binary, host_providers, log_name=None):
        if not self.owned():
            self.state.mkdir(mode=0o700, parents=True)
            (self.state / MARKER).write_text(json.dumps(self.owner) + "\n")
        with (self.state / log_name).open("a") if log_name else open(os.dup(2), "w") as log:
            subprocess.run([binary, "setup"], cwd="/", env=self.env(host_providers), check=True,
                           stdout=log, stderr=subprocess.STDOUT)

    def serve(self, binary, host_providers, log=None):
        """Start the API on an ephemeral port and return it once its own manifest and doctor agree."""
        for fd in self.runtime_locks():
            os.close(fd)
        child = subprocess.Popen([binary, "serve"], cwd="/", env=self.env(host_providers),
                                 stdout=log, stderr=subprocess.STDOUT if log else None, start_new_session=True)
        try:
            def published():
                require(child.poll() is None, f"API exited with status {child.returncode} before publishing api.json")
                return published_url(self.data, child.pid)
            until(published, f"{self.data / 'api.json'} from pid {child.pid}", 60)
            until(lambda: self.doctor(child.pid) if self.ready() else None, "authenticated readiness", 30)
            return child
        except BaseException:
            stop(child)
            raise

    def manifest(self):
        try:
            return json.loads((self.data / "api.json").read_text())
        except FileNotFoundError:
            raise Refused(f"no API is published for session {self.name} ({self.data / 'api.json'})") from None

    def request(self, method, path, body=None, auth=True):
        url = published_url(self.data, self.manifest()["pid"])
        headers = {"Content-Type": "application/json"}
        if auth:
            headers["Authorization"] = f"Bearer {self.token_file.read_text().strip()}"
        request = urllib.request.Request(url + path, method=method, headers=headers,
                                         data=json.dumps(body).encode() if body is not None else None)
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(request, timeout=3) as response:
            return json.load(response)

    def ready(self):
        try:
            return self.request("GET", "/readyz", auth=False).get("status") == "ready"
        except (OSError, urllib.error.URLError, json.JSONDecodeError):
            return False

    def doctor(self, expected_pid=None):
        """Prove the published API is this session's process, authenticated, and on this database."""
        require(self.owned(), f"session {self.name} does not exist at {self.state}")
        pid = self.manifest()["pid"]
        require(expected_pid in (None, pid), f"api.json names pid {pid}, not the started pid {expected_pid}")
        require(holds(pid, self.data / "api.lock"), f"pid {pid} does not hold {self.data / 'api.lock'}")
        listing = self.request("GET", "/v1/bots")
        with sqlite3.connect(f"file:{self.database}?mode=ro", uri=True) as db:
            row = db.execute("SELECT default_bot_id FROM installation").fetchone()
        main = str(uuid.UUID(bytes=row[0])) if row and row[0] else None
        require(main == listing["default_bot_id"], "the published API does not serve this session's database")
        return {"session": self.name, "checkout": str(self.checkout), "state": str(self.state),
                "pid": pid, "url": published_url(self.data, pid), "main_bot_id": main}

    def desktop_profile(self, url):
        path = self.state / "desktop/desktop.json"
        path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        staged = path.with_name(f".desktop-{uuid.uuid4().hex}.tmp")
        try:
            fd = os.open(staged, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "w") as output:
                json.dump({"origin": url, "owner_token": self.token_file.read_text().strip()}, output)
                output.flush()
                os.fsync(output.fileno())
            os.replace(staged, path)
        finally:
            staged.unlink(missing_ok=True)
        return path


def holds(pid, lock):
    try:
        os.kill(pid, 0)
        target = os.stat(lock)
    except (ProcessLookupError, PermissionError, FileNotFoundError):
        return False
    fds = Path(f"/proc/{pid}/fd")
    if not fds.is_dir():
        fd = try_lock(lock, create=False)
        if fd:
            os.close(fd)
        if fd is not None:
            return False
        try:
            owned = subprocess.run(["lsof", "-a", "-p", str(pid), "-Ff", "--", str(lock)],
                                   capture_output=True, text=True, timeout=5)
            return owned.returncode == 0 and any(line.startswith("f") for line in owned.stdout.splitlines())
        except (OSError, subprocess.TimeoutExpired):
            return False
    for entry in fds.iterdir():
        try:
            found = os.stat(entry)
        except OSError:
            continue
        if (found.st_dev, found.st_ino) == (target.st_dev, target.st_ino):
            return True
    return False


def stop(child, grace=15):
    if child is None or child.poll() is not None:
        return
    try:
        os.killpg(child.pid, signal.SIGTERM)
        child.wait(timeout=grace)
    except ProcessLookupError:
        pass
    except subprocess.TimeoutExpired:
        os.killpg(child.pid, signal.SIGKILL)
        child.wait()


def cargo_env():
    env = {**os.environ, "CARGO_BUILD_JOBS": os.environ.get("CARGO_BUILD_JOBS", "4")}
    env.pop("CARGO_TARGET_DIR", None)
    return env


def build(package, explicit):
    if explicit:
        return str(Path(explicit).resolve())
    output = subprocess.run(["cargo", "build", "--locked", "-q", "-p", package, "--bins",
                             "--target-dir", str(ROOT / ".else-dev/.build"),
                             "--config", "build.incremental=false",
                             "--message-format=json-render-diagnostics"],
                            cwd=ROOT, env=cargo_env(),
                            stdout=subprocess.PIPE, text=True, check=True).stdout
    frames = [json.loads(line) for line in output.splitlines()]
    return next(f["executable"] for f in frames
                if f.get("reason") == "compiler-artifact" and f["target"]["name"] == package and f.get("executable"))


def desktop_env(session, url):
    env = clean_env(host_providers=False)
    env.update(ELSE_API_URL=url, ELSE_OWNER_TOKEN_FILE=str(session.token_file),
               ELSE_DESKTOP_CONFIG_FILE=str(session.desktop_profile(url)))
    if DARWIN:
        env["ELSE_DESKTOP_TITLE"] = session.label
    return env


def host_api(session, args):
    binary = build("else-api", args.api_binary)
    fd = session.lock()
    child = None
    try:
        session.prepare(binary, host_providers=True)
        child = session.serve(binary, host_providers=True)
        info = session.doctor(child.pid)
        print(f"ready: session {session.name} at {info['url']} (pid {info['pid']}, state {session.state})",
              file=sys.stderr, flush=True)
        return child.wait()
    finally:
        stop(child)
        os.close(fd)


def owned_window(box, pid, alive, timeout=60):
    def find():
        require(alive(), "the boxed session exited before its window appeared")
        windows = json.loads(subprocess.check_output(["omabox", "windows", "-b", box, "--json"],
                                                    cwd=ROOT, text=True))
        mine = [w["address"] for w in windows if w.get("pid") == pid]
        foreign = [w["address"] for w in windows if w.get("class") == "else-bot" and w.get("pid") != pid]
        require(len(mine) <= 1 and not foreign,
                f"refusing ambiguous windows in {box}: pid {pid} owns {mine}, others {foreign}")
        return mine[0] if mine else None
    return until(find, f"else-bot window of pid {pid} in {box}", timeout)


def run_box(args):
    """Agent GUI testing: a new isolated omabox owning its API, desktop, state, and evidence."""
    require(shutil.which("omabox"), MAC_RUN if DARWIN else "omabox is required; read the omabox skill first")
    api = build("else-api", args.api_binary)
    desktop = None if args.api_only else build("else-desktop", args.desktop_binary)
    mounts = {str(Path(api).parent)} | ({str(Path(desktop).parent)} if desktop else set())
    if not args.api_binary:
        # Dev builds read age's translations from the dependency source at runtime.
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"],
            cwd=ROOT, env=cargo_env(), text=True))
        age = next(p for p in metadata["packages"] if p["name"] == "age")
        mounts.add(str(Path(age["manifest_path"]).parent / "i18n"))
    run_id = time.strftime("%Y%m%d-%H%M%S-") + uuid.uuid4().hex[:8]
    evidence = ROOT / ".else-dev" / "runs" / run_id
    evidence.mkdir(parents=True)
    box = worker = home = None
    report = {"run": run_id, "evidence": str(evidence), "status": "failed"}
    try:
        up = subprocess.check_output(["omabox", "up", "--new", "--net", "isolated", "--json",
                                      *[a for m in sorted(mounts) for a in ("--ro-bind", m)]], cwd=ROOT, text=True)
        box = report["box"] = json.loads(up)["name"]
        home = Path(subprocess.check_output(["omabox", "path", "-b", box, "--home"], cwd=ROOT, text=True).strip())
        spec = json.dumps({"run": run_id, "api": api, "desktop": desktop})
        with (evidence / "worker.log").open("w") as log:
            worker = subprocess.Popen(["omabox", "run", "-b", box, "--", "python3", str(SCRIPT), "box-worker", spec],
                                      cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        host_state = home / "else-dev" / run_id

        def ready():
            require(worker.poll() is None, f"boxed session exited ({worker.returncode}); see {evidence / 'worker.log'}")
            path = host_state / "ready.json"
            return json.loads(path.read_text()) if path.exists() else None
        session = until(ready, "boxed API readiness", 180)
        inside = f"--session {run_id} --dir {session['state']}"
        report.update(status="running", api=session["url"], api_pid=session["api_pid"], state=session["state"],
                      host_state=str(host_state),
                      status_command=f"omabox run -b {box} -- python3 {SCRIPT} status {inside}",
                      curl_command=f"omabox run -b {box} -- python3 {SCRIPT} curl {inside} /v1/bots")
        if desktop:
            window = owned_window(box, session["desktop_pid"], lambda: worker.poll() is None)
            report.update(window=window, desktop_pid=session["desktop_pid"],
                          shot_command=f"omabox shot -b {box} --window {window}",
                          keys_command=f"omabox keys -b {box} --window {window} --wait KEYS")
        (evidence / "manifest.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report, indent=2), flush=True)
        print("Ctrl-C stops the session, saves evidence, and removes the box.", file=sys.stderr, flush=True)
        report["status"] = f"exited {worker.wait()}"
    except KeyboardInterrupt:
        report["status"] = "stopped"
    finally:
        stop(worker, grace=10)
        if box:
            try:
                source = home / "else-dev" / run_id if home else None
                if source and source.is_dir():
                    shutil.copytree(source, evidence / "box", symlinks=True,
                                    ignore=lambda d, names: {"data", "projects", "desktop"} if Path(d) == source else set())
            except OSError as error:
                report.update(status="failed", evidence_error=str(error))
            finally:
                try:
                    subprocess.run(["omabox", "down", "-b", box], cwd=ROOT, check=True, timeout=45)
                except (OSError, subprocess.SubprocessError) as error:
                    report.update(status="failed", cleanup_error=str(error))
        (evidence / "manifest.json").write_text(json.dumps(report, indent=2) + "\n")
        print(f"Evidence: {evidence}", file=sys.stderr)
    return 0 if report["status"] in ("stopped", "exited 0") else 1


def box_worker(spec):
    require(os.environ.get("WAYLAND_DISPLAY") not in (None, "omabox-guard"), "box-worker runs inside omabox only")
    session = Session.resolve(spec["run"], Path.home() / "else-dev" / spec["run"])
    api = desktop = None
    try:
        session.prepare(spec["api"], host_providers=False, log_name="setup.log")
        with (session.state / "api.log").open("a") as log:
            api = session.serve(spec["api"], host_providers=False, log=log)
        if spec["desktop"]:
            session.request("POST", "/v1/onboarding", {"name": "Development Assistant", "profile": {
                "provider_id": "codex", "model": "gpt-6.1-sol",
                "reasoning_effort": "medium", "service_tier": "priority"}})
        info = session.doctor(api.pid)
        (session.state / "doctor.json").write_text(json.dumps(info, indent=2) + "\n")
        ready = {"state": str(session.state), "url": info["url"], "api_pid": api.pid, "desktop_pid": None}
        if spec["desktop"]:
            with (session.state / "desktop.log").open("a") as log:
                desktop = subprocess.Popen([spec["desktop"]], cwd="/", env=desktop_env(session, info["url"]),
                                           stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            ready["desktop_pid"] = desktop.pid
        (session.state / "ready.tmp").write_text(json.dumps(ready) + "\n")
        os.replace(session.state / "ready.tmp", session.state / "ready.json")
        return api.wait()
    finally:
        stop(desktop, grace=5)
        stop(api)


def main(argv=None):
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--session", default="default", help="session name (default: default)")
    common.add_argument("--dir", help="explicit state directory; still refuses one another session owns")
    common.add_argument("--api-binary", help="use this else-api instead of building it")
    common.add_argument("--desktop-binary", help="use this else-desktop instead of building it")
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    for name, text in [("reset", "recreate this session's state and run setup"),
                       ("api", "run the host API in the foreground on an ephemeral port"),
                       ("desktop", "run the desktop against this session's verified API"),
                       ("status", "print this session's verified API as JSON"),
                       ("curl", "curl the session's API with its owner token; /PATH arguments get its URL"),
                       ("drop", "remove this session's state")]:
        commands.add_parser(name, parents=[common], help=text)
    commands.add_parser("sql", parents=[common], help="read-only SQL on the session database").add_argument("query")
    commands.add_parser("run", parents=[common], help="agent GUI testing in a new isolated omabox").add_argument(
        "--api-only", action="store_true")
    tests = commands.add_parser("test", help="workspace tests in a new isolated omabox, with durable logs")
    tests.add_argument("-p", "--package", help="test only this Cargo package")
    tests.add_argument("--test", help="test only this integration target")
    from dev_tests import default_jobs, positive_jobs
    tests.add_argument("--jobs", type=positive_jobs, default=default_jobs(),
                       help="concurrent test binaries (default: CPU count / 4, capped at 8)")
    tests.add_argument("--serial", action="store_true", help="run one binary at a time with one test thread")
    tests.add_argument("filter", nargs=argparse.REMAINDER, help="libtest filter after --")
    commands.add_parser("box-worker", help=argparse.SUPPRESS).add_argument("spec")
    args, extra = parser.parse_known_args(argv)
    require(args.command == "curl" or not extra, f"unrecognized arguments: {' '.join(extra)}")

    def interrupted(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGHUP, interrupted)

    if args.command == "box-worker":
        return box_worker(json.loads(args.spec))
    if args.command == "run":
        return run_box(args)
    if args.command == "test":
        from dev_tests import run_tests
        return run_tests(args)
    session = Session.resolve(args.session, args.dir)
    if args.command in ("reset", "drop"):
        fd = session.lock()
        try:
            session.remove()
            if args.command == "reset":
                session.prepare(build("else-api", args.api_binary), host_providers=True)
        finally:
            os.close(fd)
    elif args.command == "api":
        return host_api(session, args)
    elif args.command == "status":
        print(json.dumps(session.doctor(), indent=2))
    elif args.command == "desktop":
        require(os.environ.get("WAYLAND_DISPLAY") != "omabox-guard",
                "no display here (omabox guard): run the desktop from your own terminal, or use `dev.py run`")
        url = session.doctor()["url"]
        binary = build("else-desktop", args.desktop_binary)
        if DARWIN:
            binary = str(session.app(binary))
        os.chdir("/")
        os.execve(binary, [binary], desktop_env(session, url))
    elif args.command == "curl":
        url = session.doctor()["url"]
        require(not any(re.match(r"^[A-Za-z][A-Za-z0-9+.-]*://", a) or a in ("--next", "-:") for a in extra),
                "curl takes /PATH arguments only; the owner token goes to this session's API alone")
        header = f"Authorization: Bearer {session.token_file.read_text().strip()}\n"
        argv = ["curl", "-sS", "--noproxy", "*", "--proto", "=http", "--connect-to", f"::{url[len('http://'):]}",
                "-H", "@-", "-H", "content-type: application/json",
                *[url + a if a.startswith("/") else a for a in extra]]
        return subprocess.run(argv, input=header, text=True).returncode
    elif args.command == "sql":
        require(session.owned(), f"session {session.name} does not exist at {session.state}")
        return subprocess.run(["sqlite3", "-readonly", str(session.database), args.query]).returncode
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Refused as error:
        print(f"dev.py: {error}", file=sys.stderr)
        raise SystemExit(1)
    except KeyboardInterrupt:
        raise SystemExit(130)
