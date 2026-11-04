#!/usr/bin/env python3
"""One disposable Lume macOS VM per verification run, driven inside the guest only.

  lume.py run --base NAME --base-storage PATH --api-binary PATH --desktop-binary PATH
              [--age-i18n PATH] [--output PATH] [--session work] [--welcome] [--cpu 4] [--memory 8GB]
  lume.py snapshot|click|type|key --run PATH --name NAME [--x X --y Y | --label L | --value V] [--role R]
              [--text T] [--key K --mod M] [--window-title T]
  lume.py probe --run PATH [--path /v1/bots] [--method GET] [--body JSON] [--expected 200] [--name NAME]
  lume.py down --run PATH

`run` stays in the foreground. It clones the stopped base into the run directory, boots the
clone headless, starts verify.py --session inside it, and on Ctrl-C, SIGTERM, `down`, or
session exit collects guest evidence and deletes only that clone. Run it with a Python that
has cua==0.10.1 installed.
"""

import argparse
import asyncio
import base64
from datetime import datetime, timezone
import fcntl
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import platform
import re
import secrets
import shlex
import shutil
import signal
import socket
import subprocess
import tarfile
import threading
import time
import urllib.error
import urllib.request
import uuid

SKILL = Path(__file__).resolve().parent
ROOT = SKILL.parents[2]
LUME = shutil.which("lume") or str(Path.home() / ".local/bin/lume")
PORT = 3211
SHARE = "/Volumes/My Shared Files/setup"
# The public Lume macOS base image's documented account; never a host credential.
GUEST_SUDO_PASSWORD = "lume"
NAME = re.compile(r"[A-Za-z0-9_-]{1,64}")
AGE_PATH = re.compile(rb"/[^\x00\r\n]*?/age-[0-9][0-9.]*/i18n")
BOOT_TIMEOUT = 300
AUTH_TIMEOUT = 240
POLL = 2.0
LIVENESS_EVERY = 30.0
DOWN_TIMEOUT = 600
CLEANING = threading.Event()


class Interrupted(BaseException):
    pass


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def sha256(path):
    with open(path, "rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


class Run:
    """A run directory. Only the process holding coordinator.lock writes manifest.json."""

    def __init__(self, path):
        self.path = Path(path).resolve()
        self.storage = self.path / "vms"
        self.setup = self.path / "setup"
        self.evidence = self.path / "evidence"

    @property
    def manifest_path(self):
        return self.path / "manifest.json"

    def read(self):
        require(self.manifest_path.is_file(), f"{self.path} is not a Lume verification run")
        return json.loads(self.manifest_path.read_text())

    def write(self, manifest):
        staging = self.path / f".manifest-{uuid.uuid4().hex}"
        staging.write_text(json.dumps(manifest, indent=2) + "\n")
        staging.replace(self.manifest_path)

    def token(self):
        return (self.setup / "env-token").read_text().strip()

    def redact(self, text):
        try:
            return str(text).replace(self.token(), "[redacted]")
        except OSError:
            return str(text)

    def lock(self, blocking):
        """The coordinator's liveness proof. Returns the open fd, or None while a live process holds it."""
        fd = os.open(self.path / "coordinator.lock", os.O_RDWR | os.O_CREAT, 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB))
            return fd
        except BlockingIOError:
            os.close(fd)
            return None


def lume(run, *argv, timeout=120, check=True):
    with (run.path / "lume.log").open("a") as log:
        log.write(f"[{now()}] $ lume {shlex.join(argv)}\n")
        log.flush()
        result = subprocess.run([LUME, *argv], env={**os.environ, "LUME_TELEMETRY_ENABLED": "false"},
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, stdin=subprocess.DEVNULL, text=True,
                                timeout=timeout)
        log.write(result.stdout + result.stderr + f"[exit {result.returncode}]\n")
    require(not check or result.returncode == 0, f"lume {argv[0]} failed with {result.returncode}; see {run.path}/lume.log")
    return result


def describe(run, name, storage):
    """Fresh `lume get`, or None when Lume reports no such VM."""
    result = lume(run, "get", name, "--storage", str(storage), "--format", "json", check=False, timeout=30)
    if result.returncode:
        require("not found" in result.stderr.lower(), f"cannot query VM {name}; see {run.path}/lume.log")
        return None
    info = json.loads(result.stdout)
    if isinstance(info, list):
        require(len(info) == 1, f"lume get {name} returned {len(info)} VMs")
        info = info[0]
    require(info.get("name") == name, f"lume get {name} described {info.get('name')!r}")
    return info


def guest_address(value):
    """A private IPv4 address that belongs to no host interface."""
    try:
        address = ipaddress.IPv4Address(value)
    except (ipaddress.AddressValueError, TypeError):
        raise RuntimeError(f"guest has no IPv4 address yet: {value!r}") from None
    require(address.is_private and not (address.is_loopback or address.is_link_local or address.is_unspecified
                                         or address.is_multicast), f"refusing non-private guest address {address}")
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        try:
            probe.bind((str(address), 0))
        except OSError:
            return str(address)
    raise RuntimeError(f"refusing {address}: it is a host interface, not the guest")


def owned(run, manifest, info):
    """The clone Lume describes is the one this run created and configured."""
    vm = manifest["vm"]
    require(Path(info.get("locationName") or "").resolve() == run.storage,
            f"{vm['name']} is not stored in {run.storage}")
    if vm.get("mac"):
        require(info.get("macAddress") == vm["mac"] and info.get("machineIdentifier") == vm["machine_identifier"],
                f"{vm['name']} identity changed since this run configured it")


def live_guest(run, manifest):
    """Fresh identity-checked guest URL. Every guest call derives its endpoint here."""
    info = describe(run, manifest["vm"]["name"], run.storage)
    require(info, f"{manifest['vm']['name']} no longer exists")
    owned(run, manifest, info)
    require(manifest["vm"].get("mac"), "this run never recorded the clone's identity")
    require(info.get("status") == "running", f"{manifest['vm']['name']} is {info.get('status')}, not running")
    require(info.get("vncUrl") is None, "the clone exposes VNC")
    return f"http://{guest_address(info.get('ipAddress'))}:{PORT}"


class Guest:
    """cua-spacesd inside the clone, through the pinned cua SDK with run-private state."""

    def __init__(self, run, base):
        self.run = run
        self.base = base

    def _call(self, operation, timeout):
        os.environ.update(DO_NOT_TRACK="1", CUA_TELEMETRY="0")
        import cua
        state = self.run.path / "private-sdk-state"
        state.mkdir(mode=0o700, exist_ok=True)

        async def call():
            runtime = cua.embedded(state_dir=str(state), spaces_home=str(state / "spaces"),
                                   teleport_home=str(state / "teleport"), account_from_session=False)
            client = await asyncio.wait_for(runtime.spacesd(self.base, self.run.token()), 30)
            return await asyncio.wait_for(operation(client), timeout)

        return asyncio.run(call())

    def sh(self, script, timeout=120):
        result = self._call(lambda client: client.sh(script, timeout * 1000), timeout + 15)
        return (result.exit.success, result.stdout.decode(errors="replace"),
                self.run.redact(result.stderr.decode(errors="replace")))

    def download(self, path, timeout=300):
        return self._call(lambda client: client.download(path), timeout)


def http(request, timeout):
    return urllib.request.build_opener(urllib.request.ProxyHandler({})).open(request, timeout=timeout)


class Mcp:
    """cua-driver's MCP endpoint inside the guest, JSON-RPC over HTTP."""

    def __init__(self, run, base):
        self.run = run
        self.url = base + "/mcp"
        self.headers = {"Authorization": "Bearer " + run.token(), "Content-Type": "application/json",
                        "Accept": "application/json, text/event-stream"}
        self.seq = 0
        self.server = self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                              "clientInfo": {"name": "else-verify-lume", "version": "1"}})["serverInfo"]
        self.post({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def post(self, body):
        request = urllib.request.Request(self.url, data=json.dumps(body).encode(), headers=self.headers, method="POST")
        try:
            with http(request, 120) as response:
                if response.headers.get("Mcp-Session-Id"):
                    self.headers["Mcp-Session-Id"] = response.headers["Mcp-Session-Id"]
                text = response.read().decode()
                kind = response.headers.get("Content-Type", "")
        except urllib.error.HTTPError as error:
            raise RuntimeError(self.run.redact(f"guest MCP HTTP {error.code}: {error.read()[:500]!r}")) from None
        if "text/event-stream" in kind:
            data = [line[5:].strip() for line in text.splitlines() if line.startswith("data:")]
            text = data[-1] if data else ""
        return json.loads(text) if text.strip() else None

    def rpc(self, method, params):
        self.seq += 1
        body = self.post({"jsonrpc": "2.0", "id": self.seq, "method": method, "params": params})
        require(body and "error" not in body, self.run.redact(f"guest MCP {method} failed: {body}"))
        return body["result"]

    def call(self, tool, arguments):
        result = self.rpc("tools/call", {"name": tool, "arguments": arguments})
        content = result.get("content", [])
        return {"tool": tool, "arguments": arguments, "is_error": bool(result.get("isError")),
                "text": "\n".join(c["text"] for c in content if c.get("type") == "text"),
                "structured": result.get("structuredContent"),
                "images": [base64.b64decode(c["data"]) for c in content if c.get("type") == "image"],
                "endpoint": self.url, "server": self.server}


def native_binary(path):
    path = path.resolve()
    require(path.is_file(), f"{path} does not exist")
    kind = subprocess.check_output(["file", "-b", str(path)], text=True).strip()
    require(kind.startswith("Mach-O 64-bit executable arm64"), f"{path} is not a native macOS arm64 executable: {kind}")
    return path


def age_resources(api, explicit):
    """The age i18n directory the debug API reads from its compile-time absolute path, or None."""
    binary = api.read_bytes()
    embedded = {match.decode(errors="replace") for match in AGE_PATH.findall(binary)}
    if not embedded:
        require(explicit is None, f"{api} does not read age resources from a source path; drop --age-i18n")
        return None
    if explicit:
        chosen = str(explicit.absolute())
        if chosen.encode() not in binary:
            chosen = str(explicit.resolve())
    else:
        try:
            metadata = json.loads(subprocess.check_output(
                ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"],
                cwd=ROOT, text=True, timeout=120, stderr=subprocess.DEVNULL))
            age = next(p for p in metadata["packages"] if p["name"] == "age")
            chosen = str(Path(age["manifest_path"]).parent / "i18n")
        except (subprocess.SubprocessError, OSError, StopIteration, json.JSONDecodeError):
            raise RuntimeError(f"cannot locate age resources with cargo metadata; pass --age-i18n, "
                               f"the API reads {sorted(embedded)}") from None
    require(chosen.encode() in binary, f"{api} reads age resources from {sorted(embedded)}, not {chosen}; pass --age-i18n")
    require(Path(chosen).is_dir(), f"{chosen} does not exist on this host")
    return Path(chosen)


def stage_payload(run, api, desktop, age):
    """Snapshot this checkout's harness and the binaries into the read-only setup share."""
    run.setup.mkdir(mode=0o700)
    fd = os.open(run.setup / "env-token", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as token:
        token.write(secrets.token_urlsafe(32))
    files = {"scripts/dev.py": ROOT / "scripts/dev.py",
             ".agents/skills/verify-else/verify.py": SKILL / "verify.py",
             ".agents/skills/verify-else/fake-codex.py": SKILL / "fake-codex.py",
             "bin/else-api": api, "bin/else-desktop": desktop}
    hashes = {}
    for relative, source in files.items():
        target = run.setup / "payload" / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, target)
        hashes[relative] = {"sha256": sha256(target), "bytes": target.stat().st_size}
    resources = None
    if age:
        shutil.copytree(age, run.setup / "age-i18n")
        resources = {"path": str(age), "files": {str(p.relative_to(run.setup / "age-i18n")): sha256(p)
                                                 for p in sorted((run.setup / "age-i18n").rglob("*")) if p.is_file()}}
    return hashes, resources


def start_script(manifest, welcome):
    guest = manifest["guest"]["dir"]
    session = manifest["guest"]["session"]
    age = manifest["age_i18n"]
    lines = ["set -eu", f"share={shlex.quote(SHARE)}", f"guest={shlex.quote(guest)}",
             f"test \"$(cat \"$share/run-id\")\" = {shlex.quote(manifest['run_id'])}",
             "mkdir \"$guest\"", "/usr/bin/ditto \"$share/payload\" \"$guest/root\"",
             "chmod +x \"$guest/root/bin/else-api\" \"$guest/root/bin/else-desktop\" "
             "\"$guest/root/.agents/skills/verify-else/fake-codex.py\""]
    if age:
        lines += ["/usr/bin/ditto \"$share/age-i18n\" \"$guest/age-i18n\"",
                  f"printf '%s\\n' {shlex.quote(GUEST_SUDO_PASSWORD)} | sudo -S -p '' /usr/bin/ditto "
                  f"\"$guest/age-i18n\" {shlex.quote(age['path'])}"]
    lines += ["python=$(/usr/local/bin/uv python find 3.14 2>/dev/null || "
              "{ /usr/local/bin/uv python install 3.14 >\"$guest/uv.log\" 2>&1 && /usr/local/bin/uv python find 3.14; })",
              "export PATH=\"$(dirname \"$python\"):$PATH\"",
              f"exec \"$python\" - \"$guest\" {shlex.quote(session)} {'1' if welcome else '0'} <<'PY'", LAUNCHER, "PY"]
    return "\n".join(lines) + "\n"


LAUNCHER = r"""
import hashlib, json, subprocess, sys, time
from pathlib import Path
guest, session, welcome = Path(sys.argv[1]), sys.argv[2], sys.argv[3] == "1"
root, evidence = guest / "root", guest / "evidence"
argv = [sys.executable, str(root / ".agents/skills/verify-else/verify.py"), "--backend", "process",
        "--session", session, "--output", str(evidence), "--api-binary", str(root / "bin/else-api"),
        "--desktop-binary", str(root / "bin/else-desktop"), "--fixture-provider"] + (["--welcome"] if welcome else [])
with (guest / "launch.log").open("w") as log:
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
(guest / "helper.pid").write_text(str(child.pid))
deadline = time.monotonic() + 180
while True:
    try:
        api = json.loads((evidence / "session.json").read_text())
        desktop = json.loads((evidence / "desktop-process.json").read_text())
        break
    except (OSError, ValueError):
        pass
    if child.poll() is not None:
        sys.exit("verify.py --session exited: " + (guest / "launch.log").read_text()[-2000:])
    if time.monotonic() > deadline:
        sys.exit("verify.py --session did not start a desktop within 180s")
    time.sleep(0.2)
digest = lambda p: hashlib.sha256(Path(p).read_bytes()).hexdigest()
print(json.dumps({"helper_pid": child.pid, "api_pid": api["pid"], "api_url": api["url"], "desktop_pid": desktop["pid"],
                  "desktop_binary": desktop["binary"], "bundle_id": desktop["bundle_id"], "python": sys.executable,
                  "sha256": {"bin/else-api": digest(root / "bin/else-api"), "bin/else-desktop": digest(root / "bin/else-desktop")}}))
"""


def collect_script(guest):
    return f"""set -u
guest={shlex.quote(guest)}
helper=$(cat "$guest/helper.pid" 2>/dev/null || true)
if [ -n "$helper" ] && ps -p "$helper" -o command= | grep -F -- "$guest/root/" >/dev/null; then
    kill -TERM "$helper"
    i=0; while kill -0 "$helper" 2>/dev/null && [ $i -lt 150 ]; do sleep 0.2; i=$((i+1)); done
fi
cd "$guest" || exit 1
set --
for p in evidence launch.log uv.log helper.pid; do [ -e "$p" ] && set -- "$@" "$p"; done
[ $# -gt 0 ] || exit 3
/usr/bin/tar --exclude evidence/apps -czf collected.tgz "$@"
"""


def collect(run, manifest):
    """Stop the guest session helper and copy its evidence out. Never raises."""
    record = {"at": now()}
    try:
        guest = Guest(run, live_guest(run, manifest))
        ok, _, stderr = guest.sh(collect_script(manifest["guest"]["dir"]), timeout=90)
        require(ok, f"guest evidence collection failed: {stderr[-500:]}")
        archive = run.path / "guest-evidence.tgz"
        archive.write_bytes(guest.download(manifest["guest"]["dir"] + "/collected.tgz"))
        with tarfile.open(archive) as bundle:
            bundle.extractall(run.path / "guest", filter="data")
        record.update(status="collected", path=str(run.path / "guest"))
    except BaseException as error:
        record.update(status="failed", error=run.redact(error))
    return record


def remove_clone(run, manifest):
    """Stop and delete only this run's clone, then prove it stopped and is absent."""
    name = manifest["vm"]["name"]
    proof = {}
    info = describe(run, name, run.storage)
    if info:
        owned(run, manifest, info)
        if info.get("status") != "stopped":
            lume(run, "stop", name, "--storage", str(run.storage), "--timeout", "20", timeout=120)
            info = describe(run, name, run.storage)
            require(info and info.get("status") == "stopped", f"{name} did not stop: {info and info.get('status')}")
        proof["stopped"] = {k: info.get(k) for k in ("name", "status", "ipAddress", "vncUrl", "macAddress",
                                                     "machineIdentifier", "locationName")}
        lume(run, "delete", name, "--storage", str(run.storage), "--force", timeout=300)
    elif (run.storage / name).exists():
        require(not manifest["vm"].get("mac"), f"cannot describe configured clone {name}; refusing directory removal")
        # An interrupted clone leaves a directory Lume cannot describe; this run's private storage owns it.
        shutil.rmtree(run.storage / name)
        proof["removed_partial_clone"] = str(run.storage / name)
    require(describe(run, name, run.storage) is None, f"lume still lists {name}")
    require(not (run.storage / name).exists(), f"{run.storage / name} still exists")
    proof["absent"] = {"at": now(), "lume_get": "not found", "directory": str(run.storage / name), "exists": False}
    return proof


def base_unchanged(run, manifest):
    base = manifest.get("base")
    if not base:
        return {"at": now(), "status": "not inspected"}
    info = describe(run, base["name"], base["storage"])
    return {"at": now(), "status": info and info.get("status"),
            "identity_unchanged": bool(info) and info.get("macAddress") == base["mac"]
            and info.get("machineIdentifier") == base["machine_identifier"]}


def cleanup(run, manifest, reason):
    """Idempotent: collect guest evidence once, remove the clone, record proof. Caller holds the lock."""
    CLEANING.set()
    manifest.update(phase="stopping", stop_reason=manifest.get("stop_reason") or reason)
    run.write(manifest)
    if manifest.get("authenticated_at") and "collected" not in manifest:
        manifest["collected"] = collect(run, manifest)
        run.write(manifest)
    try:
        manifest["cleanup"] = remove_clone(run, manifest)
        manifest["phase"] = "cleaned"
    except BaseException as error:
        manifest.update(phase="cleanup-failed", cleanup_error=run.redact(error))
    try:
        manifest["base_after"] = base_unchanged(run, manifest)
    except BaseException as error:
        manifest["base_after"] = {"error": run.redact(error)}
    run.write(manifest)


def set_phase(run, manifest, phase, **fields):
    manifest.update(phase=phase, **fields)
    manifest.setdefault("events", []).append({"at": now(), "phase": phase})
    run.write(manifest)


def git(*argv):
    return subprocess.run(["git", *argv], cwd=ROOT, capture_output=True, text=True, timeout=30).stdout.strip()


def run_vm(args):
    require((platform.system(), platform.machine()) == ("Darwin", "arm64"), "Lume verification requires macOS arm64")
    require(Path(LUME).is_file(), f"lume is not installed at {LUME}")
    require(NAME.fullmatch(args.session), "--session must be a simple name")
    api, desktop = native_binary(args.api_binary), native_binary(args.desktop_binary)
    age = age_resources(api, args.age_i18n)
    run_id = uuid.uuid4().hex
    checkout = ROOT.resolve()
    path = (args.output or checkout / ".else-dev/lume" / f"{time.strftime('%Y%m%d-%H%M%S')}-{run_id[:8]}").resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.mkdir(mode=0o700)
    os.chmod(path, 0o700)
    run = Run(path)
    lock = run.lock(blocking=False)
    require(lock is not None, f"{path} is already owned")
    base_storage = args.base_storage.resolve()
    require(base_storage != run.storage, "the base storage cannot be this run's storage")
    manifest = {"run_id": run_id, "created_at": now(), "coordinator_pid": os.getpid(),
                "checkout": {"path": str(checkout), "revision": git("rev-parse", "HEAD"),
                             "dirty": bool(git("status", "--porcelain"))},
                "vm": {"name": f"else-verify-{hashlib.sha256(str(checkout).encode()).hexdigest()[:8]}-{run_id[:12]}",
                       "storage": str(run.storage), "cpu": args.cpu, "memory": args.memory},
                "guest": {"dir": f"/tmp/else-lume-{run_id}", "session": args.session},
                "lume": LUME, "vm_log": str(run.path / "vm.log"), "phase": "preparing", "events": []}
    run.write(manifest)
    name = manifest["vm"]["name"]
    try:
        base = describe(run, args.base, base_storage)
        require(base, f"base {args.base} is not in {base_storage}")
        require(base.get("status") == "stopped", f"base {args.base} is {base.get('status')}; it must be stopped")
        digest = base_storage / args.base / ".manifest-digest"
        manifest["base"] = {"name": args.base, "storage": str(base_storage), "mac": base.get("macAddress"),
                            "machine_identifier": base.get("machineIdentifier"),
                            "manifest_digest": digest.read_text().strip() if digest.is_file() else None}
        manifest["payload"], manifest["age_i18n"] = stage_payload(run, api, desktop, age)
        (run.setup / "run-id").write_text(run_id)
        run.storage.mkdir(mode=0o700)
        set_phase(run, manifest, "cloning")
        lume(run, "clone", args.base, name, "--source-storage", str(base_storage), "--dest-storage", str(run.storage),
             timeout=1800)
        lume(run, "set", name, "--storage", str(run.storage), "--machine-identifier", "random",
             "--mac-address", "random")
        lume(run, "set", name, "--storage", str(run.storage), "--cpu", str(args.cpu), "--memory", args.memory)
        clone = describe(run, name, run.storage)
        require(clone and clone.get("status") == "stopped", f"clone {name} is not a stopped VM")
        require(clone.get("macAddress") != base.get("macAddress") and
                clone.get("machineIdentifier") != base.get("machineIdentifier"), "the clone kept the base identity")
        manifest["vm"].update(mac=clone.get("macAddress"), machine_identifier=clone.get("machineIdentifier"))
        owned(run, manifest, clone)
        set_phase(run, manifest, "booting")
        lume(run, "run", name, "--storage", str(run.storage), "--display", "none", "--vnc", "disabled", "--detach",
             "--shared-dir", f"{run.setup}:ro", "--log-file", str(run.path / "vm.log"))
        base_url = until(lambda: live_guest(run, manifest), BOOT_TIMEOUT, "the clone did not get a private address")
        guest = Guest(run, base_url)

        def authenticated():
            ok, stdout, _ = guest.sh(f"cat {shlex.quote(SHARE + '/run-id')}", timeout=15)
            require(ok and stdout.strip() == run_id, "guest does not see this run's setup share")
            return True

        until(authenticated, AUTH_TIMEOUT, "guest spacesd did not accept this run's token")
        set_phase(run, manifest, "authenticated", authenticated_at=now(), guest_url=base_url)
        ok, stdout, stderr = guest.sh(start_script(manifest, args.welcome), timeout=600)
        require(ok, f"guest session failed to start: {stderr[-1500:]}")
        started = json.loads(stdout.strip().splitlines()[-1])
        for binary in ("bin/else-api", "bin/else-desktop"):
            require(started["sha256"][binary] == manifest["payload"][binary]["sha256"], f"guest {binary} hash differs")
        manifest["guest"].update(started)
        set_phase(run, manifest, "ready")
        print(json.dumps({"run": str(run.path), "vm": name, "guest": base_url, "session": args.session,
                          "desktop_pid": started["desktop_pid"], "api_url_in_guest": started["api_url"]}, indent=2))
        print(f"Ready. Drive with: lume.py snapshot --run {shlex.quote(str(run.path))} --name 01-initial", flush=True)
        last = time.monotonic()
        while not (run.path / "stop").exists():
            time.sleep(POLL)
            if time.monotonic() - last >= LIVENESS_EVERY:
                last = time.monotonic()
                alive, _, _ = Guest(run, live_guest(run, manifest)).sh(f"kill -0 {int(started['helper_pid'])}", timeout=15)
                if not alive:
                    manifest["stop_reason"] = "guest session ended"
                    break
        manifest.setdefault("stop_reason", "stop requested")
    except Interrupted as signal_name:
        manifest["stop_reason"] = f"interrupted by {signal_name}"
    except BaseException as error:
        manifest["error"] = run.redact(error)
        raise
    finally:
        cleanup(run, manifest, manifest.get("stop_reason") or "failed")
        os.close(lock)
        print(f"Run {run.path}: {manifest['phase']}; evidence kept", flush=True)
    return 0 if manifest["phase"] == "cleaned" and "error" not in manifest else 1


def until(probe, timeout, description):
    deadline = time.monotonic() + timeout
    while True:
        try:
            return probe()
        except Exception as error:  # includes the cua SDK's native errors while the guest boots
            last = error
        if time.monotonic() >= deadline:
            raise RuntimeError(f"timed out: {description}; last error: {last}")
        time.sleep(POLL)


def down(args):
    run = Run(args.run)
    run.read()
    lock = run.lock(blocking=False)
    if lock is None:
        stop = run.path / "stop"
        if not stop.exists():
            stop.write_text(now() + "\n")
        deadline = time.monotonic() + DOWN_TIMEOUT
        while (lock := run.lock(blocking=False)) is None:
            require(time.monotonic() < deadline, f"coordinator for {run.path} did not finish cleanup")
            time.sleep(0.5)
    try:
        manifest = run.read()
        if manifest["phase"] != "cleaned":
            cleanup(run, manifest, "down after coordinator exited")
        print(json.dumps({k: manifest.get(k) for k in ("phase", "stop_reason", "error", "cleanup", "cleanup_error",
                                                       "collected", "base_after")}, indent=2))
        return 0 if manifest["phase"] == "cleaned" else 1
    finally:
        os.close(lock)


def save(path, data):
    path.write_text(json.dumps(data, indent=2, default=repr) + "\n")


def snapshot(mcp, pid, title, evidence, label):
    listing = mcp.call("list_windows", {"pid": pid, "on_screen_only": True})
    require(not listing["is_error"], f"list_windows failed: {listing['text']}")
    windows = [w for w in (listing["structured"] or {}).get("windows", []) if title is None or w.get("title") == title]
    require(len(windows) == 1, f"expected one on-screen window, found {[w.get('title') for w in windows]}")
    window = windows[0]
    state = mcp.call("get_window_state", {
        "pid": pid, "window_id": window["window_id"], "full_output": True, "include_accessibility_tree": True,
        "include_screenshot": True, "timeout_ms": 10000, "max_elements": 2000, "max_image_dimension": 0,
        "verbose": True})
    structured = state["structured"] or {}
    save(evidence / f"{label}.json", {"captured_at": now(), "endpoint": state["endpoint"], "server": state["server"],
                                      "window": window, "is_error": state["is_error"], "text": state["text"],
                                      **{k: v for k, v in structured.items() if k != "tree_markdown"}})
    (evidence / f"{label}.ax.txt").write_text(structured.get("tree_markdown") or "")
    for index, image in enumerate(state["images"]):
        (evidence / (f"{label}.png" if index == 0 else f"{label}-{index}.png")).write_bytes(image)
    require(not state["is_error"], f"get_window_state failed: {state['text']}")
    require(structured.get("screenshot_frame_valid") is True, "the screenshot frame is not valid")
    require(len(state["images"]) == 1 and structured.get("capture_id"), "the snapshot has no single bound capture")
    print(f"{evidence.name}/{label}: {structured.get('window_title')!r} elements={len(structured.get('elements') or [])} "
          f"capture={structured['capture_id']}", flush=True)
    return window, structured


def element(args, structured):
    def matches(e):
        if args.role and e.get("role") != args.role:
            return False
        return e.get("value") == args.value if args.value is not None else e.get("label") == args.label
    found = [e for e in structured.get("elements") or [] if matches(e)]
    require(len(found) == 1, f"selector label={args.label!r} value={args.value!r} role={args.role!r} "
                             f"matched {len(found)} elements; it must match exactly one")
    return found[0]["element_token"]


def act(args):
    run = Run(args.run)
    manifest = run.read()
    require(manifest["phase"] == "ready", f"run is {manifest['phase']}, not ready")
    require(NAME.fullmatch(args.name), "--name must be a simple name")
    point = args.x is not None or args.y is not None
    selector = args.label is not None or args.value is not None
    require(not (point and selector), "use either --x/--y or a selector, not both")
    require(not point or (args.x is not None and args.y is not None), "--x and --y go together")
    require(args.command != "click" or point or selector, "click needs --x/--y or --label/--value")
    require(args.command != "type" or args.text is not None, "type needs --text")
    require(args.command != "key" or not point, "key takes a selector, not --x/--y")
    base = live_guest(run, manifest)
    evidence = run.evidence / args.name
    run.evidence.mkdir(mode=0o700, exist_ok=True)
    try:
        evidence.mkdir(mode=0o700)
    except FileExistsError:
        raise RuntimeError(f"evidence {args.name} already exists; choose a new --name") from None
    mcp = Mcp(run, base)
    pid = manifest["guest"]["desktop_pid"]
    window, before = snapshot(mcp, pid, args.window_title, evidence, "before" if args.command != "snapshot" else "snapshot")
    if args.command == "snapshot":
        return 0
    background = {"delivery_mode": "background"}
    target = {"kind": "window", "pid": pid, "window_id": window["window_id"]}
    if args.command == "click":
        request = ({"pid": pid, "window_id": window["window_id"], "x": args.x, "y": args.y,
                    "capture_id": before["capture_id"]} if point else {"element_token": element(args, before)})
        tool = "click"
    elif args.command == "type":
        request = {"text": args.text, **({"element_token": element(args, before)} if selector else
                                         {"target": target, **({"x": args.x, "y": args.y} if point else {})})}
        tool = "type_text"
    else:
        request = {"key": args.key, **({"element_token": element(args, before)} if selector else {"target": target}),
                   **({"modifiers": args.mod} if args.mod else {})}
        tool = "press_key"
    result = mcp.call(tool, {**request, **background})
    result.pop("images")
    save(evidence / "action.json", result)
    print(json.dumps({k: result[k] for k in ("tool", "is_error", "text")}, ensure_ascii=False), flush=True)
    require(not result["is_error"], f"{tool} failed in the guest; not retried: {result['text']}")
    time.sleep(args.settle)
    snapshot(Mcp(run, live_guest(run, manifest)), pid, args.window_title, evidence, "after")
    return 0


def probe(args):
    run = Run(args.run)
    manifest = run.read()
    require(manifest["phase"] == "ready", f"run is {manifest['phase']}, not ready")
    guest = manifest["guest"]
    evidence = None
    if args.name:
        require(NAME.fullmatch(args.name), "--name must be a simple name")
        run.evidence.mkdir(mode=0o700, exist_ok=True)
        evidence = run.evidence / args.name
        try:
            evidence.mkdir(mode=0o700)
        except FileExistsError:
            raise RuntimeError(f"evidence {args.name} already exists; choose a new --name") from None
    argv = [guest["python"], f"{guest['dir']}/root/.agents/skills/verify-else/verify.py", "--backend", "process",
            "--probe", guest["session"], "--output", f"{guest['dir']}/evidence", "--method", args.method,
            "--path", args.path, *(["--body", args.body] if args.body else []),
            "--expected", *map(str, args.expected)]
    ok, stdout, stderr = Guest(run, live_guest(run, manifest)).sh(shlex.join(argv), timeout=90)
    if evidence:
        save(evidence / "probe.json", {"at": now(), "argv": argv[1:], "ok": ok,
                                                       "stdout": stdout, "stderr": stderr})
    print(stdout, end="")
    require(ok, f"probe failed in the guest: {stderr[-1500:]}")
    return 0


def parser():
    top = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = top.add_subparsers(dest="command", required=True)
    start = commands.add_parser("run", help="clone, boot, and own one VM until stopped")
    start.add_argument("--base", required=True, help="stopped prepared base VM; only read and cloned")
    start.add_argument("--base-storage", type=Path, required=True)
    start.add_argument("--api-binary", type=Path, required=True)
    start.add_argument("--desktop-binary", type=Path, required=True)
    start.add_argument("--age-i18n", type=Path, help="age i18n directory the debug API was compiled against")
    start.add_argument("--output", type=Path, help="new run directory, default .else-dev/lume/<unique>")
    start.add_argument("--session", default="work")
    start.add_argument("--welcome", action="store_true", help="leave onboarding to the desktop")
    start.add_argument("--cpu", type=int, default=4)
    start.add_argument("--memory", default="8GB")
    for verb in ("snapshot", "click", "type", "key"):
        action = commands.add_parser(verb, help=f"{verb} in the guest desktop, saving before/action/after evidence")
        action.add_argument("--run", type=Path, required=True)
        action.add_argument("--name", required=True, help="new evidence directory name; never overwritten")
        action.add_argument("--window-title", help="pick this window when the app shows several")
        if verb == "snapshot":
            continue
        action.add_argument("--x", type=float, help="screenshot pixel, bound to the fresh capture")
        action.add_argument("--y", type=float)
        action.add_argument("--label", help="exactly one AX element with this label")
        action.add_argument("--value", help="exactly one AX element with this value")
        action.add_argument("--role", help="narrow --label/--value to this AX role")
        action.add_argument("--settle", type=float, default=1.5)
        if verb == "type":
            action.add_argument("--text", required=True)
        if verb == "key":
            action.add_argument("--key", required=True)
            action.add_argument("--mod", action="append", default=[])
    call = commands.add_parser("probe", help="call the session's API inside the guest")
    call.add_argument("--run", type=Path, required=True)
    call.add_argument("--method", default="GET", choices=["GET", "POST", "PATCH", "PUT", "DELETE"])
    call.add_argument("--path", default="/v1/bots")
    call.add_argument("--body")
    call.add_argument("--expected", type=int, nargs="+", default=[200])
    call.add_argument("--name", help="also save the response as new evidence")
    stop = commands.add_parser("down", help="stop a live run, or clean up after a dead coordinator")
    stop.add_argument("--run", type=Path, required=True)
    return top


def main(argv=None):
    args = parser().parse_args(argv)
    for name in ("text", "key", "label", "value", "role", "x", "y"):
        vars(args).setdefault(name, None)

    def interrupted(number, _frame):
        if not CLEANING.is_set():
            raise Interrupted(signal.Signals(number).name)

    if args.command == "run":
        for number in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            signal.signal(number, interrupted)
        return run_vm(args)
    if args.command == "down":
        return down(args)
    if args.command == "probe":
        return probe(args)
    return act(args)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RuntimeError as error:
        raise SystemExit(f"lume.py: {error}")
