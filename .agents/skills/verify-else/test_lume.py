#!/usr/bin/env python3
"""Ownership and failure-path checks for lume.py against a fake Lume CLI, fake guest shell, and local MCP server."""

import base64
import importlib.util
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import platform
import secrets
import shutil
import signal
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
import urllib.parse
import urllib.request

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
ARM64 = b"\xcf\xfa\xed\xfe\x0c\x00\x00\x01\x00\x00\x00\x00\x02\x00\x00\x00" + bytes(16)


def fake_lume(argv):
    """Just enough of `lume` 0.6.1: one fake.json per VM under its storage directory."""
    home = Path(os.environ["FAKE_LUME_HOME"])
    with (home / "journal.jsonl").open("a") as journal:
        journal.write(json.dumps(argv) + "\n")
    command, name, rest = argv[0], argv[1], argv[2:]
    options, flags, positional = {}, set(), []
    while rest:
        item = rest.pop(0)
        if item in ("--detach", "--force"):
            flags.add(item)
        elif item.startswith("--"):
            options[item] = rest.pop(0)
        else:
            positional.append(item)

    def config(storage, vm):
        return Path(storage).resolve() / vm / "fake.json"

    if command == "get":
        path = config(options["--storage"], name)
        if not path.exists():
            print(f"VM {name} not found", file=sys.stderr)
            return 1
        info = json.loads(path.read_text())
        info.update(name=name, locationName=str(path.parent.parent))
        print(json.dumps([info]))
        return 0
    if command == "clone":
        source = json.loads(config(options["--source-storage"], name).read_text())
        assert source["status"] == "stopped"
        target = config(options["--dest-storage"], positional[0])
        target.parent.mkdir(parents=True)
        target.write_text(json.dumps(source))
        return 0
    path = config(options["--storage"], name)
    info = json.loads(path.read_text())
    if command == "set":
        assert info["status"] == "stopped"
        if options.get("--mac-address") == "random":
            info["macAddress"] = "02:" + ":".join(secrets.token_hex(1) for _ in range(5))
        if options.get("--machine-identifier") == "random":
            info["machineIdentifier"] = base64.b64encode(secrets.token_bytes(24)).decode()
        info.update(cpuCount=int(options.get("--cpu", info.get("cpuCount", 4))))
    elif command == "run":
        assert info["status"] == "stopped" and options["--vnc"] == "disabled" and "--detach" in flags
        with (home / "ip-counter").open("a+") as counter:
            counter.seek(0)
            number = len(counter.read()) + 10
            counter.write("x")
        info.update(status="running", ipAddress=f"192.168.250.{number}", vncUrl=None,
                    fake_shared=options["--shared-dir"].removesuffix(":ro"))
        (home / "ips").mkdir(exist_ok=True)
        (home / "ips" / info["ipAddress"]).write_text(str(path))
        Path(options["--log-file"]).write_text("booted\n")
    elif command == "stop":
        info.update(status="stopped", ipAddress=None)
    elif command == "delete":
        assert "--force" in flags
        shutil.rmtree(path.parent)
        return 0
    path.write_text(json.dumps(info))
    return 0


class FakeGuest:
    """cua-spacesd inside whichever fake VM currently holds the requested IP."""

    def __init__(self, run, base):
        self.run = run
        ip = urllib.parse.urlsplit(base).hostname
        self.vm = json.loads(Path((Path(os.environ["FAKE_LUME_HOME"]) / "ips" / ip).read_text()).read_text())
        self.share = Path(self.vm["fake_shared"])
        self.mode = (Path(os.environ["FAKE_LUME_HOME"]) / "guest-mode").read_text().strip()

    def sh(self, script, timeout=120):
        if script.startswith("cat "):
            if self.mode == "no-auth":
                raise RuntimeError("Unauthenticated")
            return True, (self.share / "run-id").read_text(), ""
        if "exec \"$python\"" in script:
            if self.mode == "start-fails":
                return False, "", "verify.py --session exited: api crashed"
            payload = self.share / "payload"
            digest = {b: __import__("hashlib").sha256((payload / b).read_bytes()).hexdigest()
                      for b in ("bin/else-api", "bin/else-desktop")}
            return True, json.dumps({"helper_pid": 900, "api_pid": 901, "api_url": "http://127.0.0.1:50000",
                                     "desktop_pid": 930, "desktop_binary": "/tmp/x", "bundle_id": "b",
                                     "python": "/guest/python3", "sha256": digest}) + "\n", ""
        if script.startswith("kill -0"):
            return True, "", ""
        if "collected.tgz" in script:
            return True, "", ""
        if "--probe" in script:
            return True, json.dumps({"bots": [], "probed": self.vm["ipAddress"]}) + "\n", ""
        raise AssertionError(f"unexpected guest script: {script[:200]}")

    def download(self, path, timeout=300):
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w:gz") as bundle:
            for name, text in (("launch.log", f"guest {self.vm['ipAddress']} {self.mode}\n"),
                               ("evidence/session.json", "{}\n")):
                info = tarfile.TarInfo(name)
                info.size = len(text.encode())
                bundle.addfile(info, io.BytesIO(text.encode()))
        return buffer.getvalue()


def harness(lume_path, argv):
    """Coordinator in a child process so a test can kill it like a crashed terminal."""
    module = load(lume_path, "lume_child")
    module.LUME = os.environ["FAKE_LUME_BIN"]
    module.Guest = FakeGuest
    module.POLL = 0.02
    module.run_vm(module.parser().parse_args(argv))


def load(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Driver(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        state = self.server.state
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if "id" not in body:
            self.send_response(202)
            self.end_headers()
            return
        if body["method"] == "initialize":
            result = {"serverInfo": {"name": "cua-driver", "version": "fake"}}
        else:
            tool, arguments = body["params"]["name"], body["params"]["arguments"]
            state["calls"].append((tool, arguments, self.headers["Authorization"]))
            result = {"content": [], "structuredContent": {}}
            if tool == "list_windows":
                result["structuredContent"] = {"windows": [{"window_id": 41, "title": "else.bot"}]}
            elif tool == "get_window_state":
                state["captures"] += 1
                result["structuredContent"] = {"capture_id": f"capture-{state['captures']}", "window_title": "else.bot",
                                               "screenshot_frame_valid": state["frame_valid"],
                                               "elements": state["elements"], "tree_markdown": "- AXWindow"}
                result["content"] = [{"type": "image", "data": base64.b64encode(
                    b"png-%d" % state["captures"]).decode()}]
            else:
                result.update(isError=state["tool_error"], content=[{"type": "text", "text": "done"}])
        data = json.dumps({"jsonrpc": "2.0", "id": body["id"], "result": result}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@unittest.skipUnless(sys.platform == "darwin" and platform.machine() == "arm64", "Lume requires macOS arm64")
class LumeRuns(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="lume-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.home = self.tmp / "fake-lume"
        self.home.mkdir()
        self.fake = self.home / "lume"
        self.fake.write_text(f"#!{sys.executable}\nimport sys\nsys.path.insert(0, {str(HERE)!r})\n"
                             "import test_lume\nraise SystemExit(test_lume.fake_lume(sys.argv[1:]))\n")
        self.fake.chmod(0o755)
        self.guest_mode("ok")
        previous = {k: os.environ.get(k) for k in ("FAKE_LUME_HOME", "FAKE_LUME_BIN")}
        os.environ.update(FAKE_LUME_HOME=str(self.home), FAKE_LUME_BIN=str(self.fake))
        self.addCleanup(lambda: [os.environ.pop(k, None) if v is None else os.environ.__setitem__(k, v)
                                 for k, v in previous.items()])
        self.base_storage = self.tmp / "base-storage"
        (self.base_storage / "golden").mkdir(parents=True)
        self.base_config = self.base_storage / "golden/fake.json"
        self.base_config.write_text(json.dumps({"status": "stopped", "macAddress": "ae:db:96:3d:c5:4b",
                                                "machineIdentifier": "YmFzZQ==", "ipAddress": None, "vncUrl": None}))
        self.base_bytes = self.base_config.read_bytes()
        self.binaries = self.tmp / "bin"
        self.binaries.mkdir()
        for name in ("else-api", "else-desktop"):
            (self.binaries / name).write_bytes(ARM64 + secrets.token_bytes(64))
        self.state = {"calls": [], "captures": 0, "frame_valid": True, "tool_error": False,
                      "elements": [{"label": "Send", "role": "AXButton", "element_token": "s1:3"}]}
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Driver)
        self.server.state = self.state
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.hosts = []
        self.threads = []

    def guest_mode(self, mode):
        (self.home / "guest-mode").write_text(mode)

    def route(self, request, timeout):
        url = urllib.parse.urlsplit(request.full_url)
        self.hosts.append(url.hostname)
        local = urllib.request.Request(f"http://127.0.0.1:{self.server.server_port}{url.path}", data=request.data,
                                       headers=dict(request.header_items()), method=request.get_method())
        return urllib.request.build_opener(urllib.request.ProxyHandler({})).open(local, timeout=timeout)

    def checkout(self, name):
        root = self.tmp / name
        skill = root / ".agents/skills/verify-else"
        skill.mkdir(parents=True)
        (root / "scripts").mkdir()
        shutil.copy2(REPO / "scripts/dev.py", root / "scripts/dev.py")
        for script in ("lume.py", "verify.py", "fake-codex.py"):
            shutil.copy2(HERE / script, skill / script)
        module = load(skill / "lume.py", f"lume_{name}_{secrets.token_hex(4)}")
        module.LUME = str(self.fake)
        module.Guest = FakeGuest
        module.http = self.route
        module.POLL = 0.02
        module.LIVENESS_EVERY = 0.05
        module.AUTH_TIMEOUT = 0.5
        return module

    def run_argv(self, *extra):
        return ["run", "--base", "golden", "--base-storage", str(self.base_storage),
                "--api-binary", str(self.binaries / "else-api"), "--desktop-binary", str(self.binaries / "else-desktop"),
                *extra]

    def start(self, module):
        errors = []

        def coordinate():
            try:
                module.run_vm(module.parser().parse_args(self.run_argv()))
            except BaseException as error:
                errors.append(error)

        before = set((module.ROOT / ".else-dev/lume").glob("*")) if (module.ROOT / ".else-dev/lume").exists() else set()
        thread = threading.Thread(target=coordinate, daemon=True)
        thread.start()
        self.threads.append(thread)
        self.addCleanup(self.stop_thread, module, thread)
        run = self.wait_ready(module.ROOT / ".else-dev/lume", before, errors)
        return module.Run(run), thread, errors

    def stop_thread(self, module, thread):
        if thread.is_alive():
            for run in (module.ROOT / ".else-dev/lume").glob("*"):
                (run / "stop").write_text("test cleanup\n")
            thread.join(10)

    def wait_ready(self, parent, before, errors, phase="ready"):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            self.assertFalse(errors, errors)
            for run in set(parent.glob("*")) - before if parent.exists() else ():
                manifest = run / "manifest.json"
                if manifest.exists() and json.loads(manifest.read_text())["phase"] == phase:
                    return run
            time.sleep(0.02)
        self.fail(f"no run reached {phase}")

    def vm_config(self, run):
        manifest = run.read()
        return run.storage / manifest["vm"]["name"] / "fake.json"

    def mutate(self, run, **fields):
        path = self.vm_config(run)
        info = json.loads(path.read_text())
        original = dict(info)
        info.update(fields)
        path.write_text(json.dumps(info))
        return lambda: path.write_text(json.dumps(original))

    def act(self, module, run, *argv):
        return module.main([argv[0], "--run", str(run.path), *argv[1:]])

    def journal(self):
        return [json.loads(line) for line in (self.home / "journal.jsonl").read_text().splitlines()]

    def assert_base_untouched(self):
        self.assertEqual(self.base_config.read_bytes(), self.base_bytes)
        for argv in self.journal():
            if argv[1] == "golden":
                self.assertIn(argv[0], ("get", "clone"), f"base was the target of {argv}")

    def test_concurrent_worktree_runs_are_private_and_down_removes_only_its_own_clone(self):
        first, second = self.checkout("worktree-a"), self.checkout("worktree-b")
        run_a, thread_a, _ = self.start(first)
        run_b, thread_b, _ = self.start(second)
        a, b = run_a.read(), run_b.read()
        for run, module in ((run_a, first), (run_b, second)):
            self.assertEqual(run.path.parent, (module.ROOT / ".else-dev/lume").resolve())
            self.assertEqual(run.path.stat().st_mode & 0o777, 0o700)
            self.assertEqual((run.setup / "env-token").stat().st_mode & 0o777, 0o600)
            self.assertNotIn(run.token(), (run.path / "manifest.json").read_text())
            self.assertNotIn(run.token(), (run.path / "lume.log").read_text())
        for key in (("run_id",), ("vm", "name"), ("vm", "storage"), ("vm", "mac"), ("vm", "machine_identifier"),
                    ("guest", "dir"), ("checkout", "path")):
            self.assertNotEqual(self.dig(a, key), self.dig(b, key), key)
        self.assertNotEqual(a["vm"]["name"].split("-")[2], b["vm"]["name"].split("-")[2], "checkout hash in VM name")
        self.assertNotEqual(a["vm"]["mac"], a["base"]["mac"])
        self.assertIn(a["run_id"], a["guest"]["dir"])
        self.assertNotEqual(run_a.token(), run_b.token())

        self.assertEqual(self.act(first, run_a, "snapshot", "--name", "a1"), 0)
        self.assertEqual(self.act(second, run_b, "snapshot", "--name", "b1"), 0)
        ip_a, ip_b = (json.loads(self.vm_config(r).read_text())["ipAddress"] for r in (run_a, run_b))
        self.assertEqual(self.hosts, [ip_a] * (len(self.hosts) // 2) + [ip_b] * (len(self.hosts) // 2))
        self.assertEqual({auth for _, _, auth in self.state["calls"]},
                         {"Bearer " + run_a.token(), "Bearer " + run_b.token()})

        self.assertEqual(first.down(first.parser().parse_args(["down", "--run", str(run_a.path)])), 0)
        thread_a.join(10)
        self.assertFalse(thread_a.is_alive())
        cleaned = run_a.read()
        self.assertEqual(cleaned["phase"], "cleaned")
        self.assertEqual(cleaned["stop_reason"], "stop requested")
        self.assertFalse(cleaned["cleanup"]["absent"]["exists"])
        self.assertEqual(cleaned["cleanup"]["stopped"]["status"], "stopped")
        self.assertEqual(cleaned["collected"]["status"], "collected")
        self.assertTrue((run_a.path / "guest/launch.log").exists())
        self.assertTrue(cleaned["base_after"]["identity_unchanged"])
        self.assertFalse(self.vm_config(run_a).parent.exists())
        self.assertEqual(json.loads(self.vm_config(run_b).read_text())["status"], "running")
        self.assertEqual(self.act(second, run_b, "snapshot", "--name", "b2"), 0)
        with self.assertRaisesRegex(RuntimeError, "not ready"):
            self.act(first, run_a, "snapshot", "--name", "a2")
        self.assertEqual(first.down(first.parser().parse_args(["down", "--run", str(run_a.path)])), 0)

        self.assertEqual(second.down(second.parser().parse_args(["down", "--run", str(run_b.path)])), 0)
        thread_b.join(10)
        self.assertEqual(run_b.read()["phase"], "cleaned")
        self.assert_base_untouched()

    @staticmethod
    def dig(data, keys):
        for key in keys:
            data = data[key]
        return data

    def test_guest_calls_refuse_host_loopback_public_and_changed_identity(self):
        module = self.checkout("worktree")
        module.LIVENESS_EVERY = 3600
        run, _, _ = self.start(module)
        probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        probe.connect(("192.0.2.1", 9))
        host_ip = probe.getsockname()[0]
        probe.close()
        refusals = [dict(ipAddress="127.0.0.1"), dict(ipAddress="8.8.8.8"), dict(ipAddress=None),
                    dict(macAddress="02:00:00:00:00:01"), dict(machineIdentifier="b3RoZXI="),
                    dict(status="stopped"), dict(vncUrl="vnc://192.168.250.10:5900")]
        if host_ip.startswith(("10.", "172.", "192.168.")):
            refusals.append(dict(ipAddress=host_ip))
        for index, fields in enumerate(refusals):
            restore = self.mutate(run, **fields)
            try:
                with self.assertRaises(RuntimeError, msg=fields):
                    self.act(module, run, "snapshot", "--name", f"refused-{index}")
                with self.assertRaises(RuntimeError, msg=fields):
                    self.act(module, run, "probe")
            finally:
                restore()
            self.assertFalse((run.evidence / f"refused-{index}").exists())
        self.assertEqual(self.hosts, [])
        self.assertEqual(self.state["calls"], [])
        self.assertEqual(self.act(module, run, "probe", "--name", "bots"), 0)
        probed = json.loads((run.evidence / "bots/probe.json").read_text())
        self.assertEqual(json.loads(probed["stdout"])["probed"], json.loads(self.vm_config(run).read_text())["ipAddress"])
        self.assertIn("--probe", probed["argv"])

    def test_evidence_names_are_never_overwritten(self):
        module = self.checkout("worktree")
        run, _, _ = self.start(module)
        self.assertEqual(self.act(module, run, "click", "--name", "send", "--label", "Send", "--settle", "0"), 0)
        saved = {p.name: p.read_bytes() for p in (run.evidence / "send").iterdir()}
        self.assertEqual(set(saved), {"before.json", "before.ax.txt", "before.png", "action.json",
                                      "after.json", "after.ax.txt", "after.png"})
        calls = len(self.state["calls"])
        for argv in (("snapshot", "--name", "send"), ("click", "--name", "send", "--x", "1", "--y", "2"),
                     ("probe", "--name", "send")):
            with self.assertRaisesRegex(RuntimeError, "already exists"):
                self.act(module, run, *argv)
        self.assertEqual({p.name: p.read_bytes() for p in (run.evidence / "send").iterdir()}, saved)
        self.assertEqual(len(self.state["calls"]), calls)

    def test_ambiguous_selectors_invalid_frames_and_tool_errors_are_refused_once(self):
        module = self.checkout("worktree")
        run, _, _ = self.start(module)
        self.state["elements"] = [{"label": "Send", "role": "AXButton", "element_token": "s1:3"},
                                  {"label": "Send", "role": "AXButton", "element_token": "s1:4"}]
        with self.assertRaisesRegex(RuntimeError, "matched 2 elements"):
            self.act(module, run, "click", "--name", "ambiguous", "--label", "Send")
        self.assertEqual([t for t, _, _ in self.state["calls"]], ["list_windows", "get_window_state"])

        self.state["tool_error"] = True
        with self.assertRaisesRegex(RuntimeError, "not retried"):
            self.act(module, run, "click", "--name", "pixel", "--x", "610", "--y", "522", "--settle", "0")
        clicks = [arguments for tool, arguments, _ in self.state["calls"] if tool == "click"]
        before = json.loads((run.evidence / "pixel/before.json").read_text())
        self.assertEqual(len(clicks), 1)
        self.assertEqual(clicks[0]["capture_id"], before["capture_id"])
        self.assertTrue(json.loads((run.evidence / "pixel/action.json").read_text())["is_error"])
        self.assertFalse((run.evidence / "pixel/after.json").exists())

        self.state["frame_valid"] = False
        with self.assertRaisesRegex(RuntimeError, "frame is not valid"):
            self.act(module, run, "type", "--name", "invalid-frame", "--text", "hi", "--settle", "0")
        self.assertTrue((run.evidence / "invalid-frame/before.png").exists())
        self.assertEqual(len([t for t, _, _ in self.state["calls"] if t == "type_text"]), 0)

    def test_debug_resources_accept_the_compiled_path_with_spaces(self):
        module = self.checkout("worktree")
        resources = self.tmp / "External Disk/age-0.12.1/i18n"
        resources.mkdir(parents=True)
        api = self.tmp / "debug-api"
        api.write_bytes(ARM64 + b"\0" + str(resources).encode() + b"\0")
        self.assertEqual(module.age_resources(api, resources), resources)
        wrong = self.tmp / "Other Disk/age-0.12.1/i18n"
        wrong.mkdir(parents=True)
        with self.assertRaisesRegex(RuntimeError, "pass --age-i18n"):
            module.age_resources(api, wrong)

    def test_failed_vm_queries_never_remove_a_configured_clone_directory(self):
        module = self.checkout("worktree")
        run, _, _ = self.start(module)
        manifest = run.read()
        for message, expected in (("permission denied", "cannot query VM"),
                                  ("VM not found", "cannot describe configured clone")):
            failed = subprocess.CompletedProcess([], 1, stdout="", stderr=message)
            with patch.object(module, "lume", return_value=failed):
                with self.assertRaisesRegex(RuntimeError, expected):
                    module.remove_clone(run, manifest)
            self.assertTrue(self.vm_config(run).exists())
        self.assert_base_untouched()

    def test_boot_and_session_failures_keep_evidence_and_remove_the_clone(self):
        module = self.checkout("worktree")
        for mode, expected in (("no-auth", "did not accept"), ("start-fails", "failed to start")):
            self.guest_mode(mode)
            output = self.tmp / f"run-{mode}"
            with self.assertRaisesRegex(RuntimeError, expected):
                module.run_vm(module.parser().parse_args(self.run_argv("--output", str(output))))
            run = module.Run(output)
            manifest = run.read()
            self.assertEqual(manifest["phase"], "cleaned")
            self.assertIn(expected, manifest["error"])
            self.assertFalse((run.storage / manifest["vm"]["name"]).exists())
            self.assertTrue((output / "lume.log").exists() and (output / "vm.log").exists())
            if mode == "start-fails":
                self.assertEqual(manifest["collected"]["status"], "collected")
                self.assertIn("start-fails", (output / "guest/launch.log").read_text())
            else:
                self.assertNotIn("collected", manifest)
        with self.assertRaises(FileExistsError):
            module.run_vm(module.parser().parse_args(self.run_argv("--output", str(self.tmp / "run-no-auth"))))
        self.assert_base_untouched()

    def test_down_after_a_dead_coordinator_removes_only_an_identity_matching_clone(self):
        module = self.checkout("worktree")
        output = self.tmp / "crashed"
        child = subprocess.Popen(
            [sys.executable, "-c", "import sys, json; sys.path.insert(0, sys.argv[1]); import test_lume; "
             "test_lume.harness(sys.argv[2], json.loads(sys.argv[3]))", str(HERE), str(module.ROOT / ".agents/skills/verify-else/lume.py"),
             json.dumps(self.run_argv("--output", str(output)))],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        self.addCleanup(lambda: child.poll() is None and child.kill())
        deadline = time.monotonic() + 20
        while not ((output / "manifest.json").exists() and json.loads((output / "manifest.json").read_text())["phase"] == "ready"):
            self.assertIsNone(child.poll(), "coordinator exited early")
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.02)
        os.killpg(child.pid, signal.SIGKILL)
        child.wait()
        run = module.Run(output)
        args = module.parser().parse_args(["down", "--run", str(output)])

        restore = self.mutate(run, macAddress="02:00:00:00:00:99")
        self.assertEqual(module.down(args), 1)
        self.assertEqual(run.read()["phase"], "cleanup-failed")
        self.assertIn("identity changed", run.read()["cleanup_error"])
        self.assertTrue(self.vm_config(run).exists())
        restore()

        self.assertEqual(module.down(args), 0)
        manifest = run.read()
        self.assertEqual(manifest["phase"], "cleaned")
        self.assertEqual(manifest["stop_reason"], "down after coordinator exited")
        self.assertFalse(self.vm_config(run).parent.exists())
        self.assertEqual(module.down(args), 0)
        self.assert_base_untouched()


if __name__ == "__main__":
    unittest.main()
