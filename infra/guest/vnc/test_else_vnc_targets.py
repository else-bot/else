"""Checks else_vnc_targets.py against the guest image's real websockify and CDP helpers.

Runs inside the guest image as the desktop user, with this checkout's
infra/guest mounted at /guest:

    docker run --rm --user elsebot -v "$PWD/infra/guest:/guest:ro" \\
        --entrypoint python3 else-guest:3 /guest/vnc/test_else_vnc_targets.py

Fake VNC servers on 5900 and 5901 stand in for x11vnc; each greets with its
port so a test can tell which one websockify reached.
"""

import os
import signal
import socket
import subprocess
import sys
import threading
import time
import unittest

import websocket

HERE = os.path.dirname(os.path.abspath(__file__))
CDP = os.path.join(os.path.dirname(HERE), "cdp")
sys.path.insert(0, HERE)
import else_vnc_targets  # noqa: E402

LOCK = else_vnc_targets.LOCK
PORT = 6080


SLOW = {"pause": 0.0, "close_after": None, "ack_delay": 0.0, "port_offset": 0, "received": []}


def fake_vnc(port):
    server = socket.create_server(("127.0.0.1", port))

    def serve(conn):
        conn.sendall(b"PORT %d\n" % port)
        control = port == 5900
        slow = dict(SLOW) if control else {"pause": 0.0, "close_after": None, "ack_delay": None, "received": []}
        peer = conn.getpeername()[1]
        time.sleep(slow["pause"])
        total = 0
        try:
            while chunk := conn.recv(65536):
                total += len(chunk)
        except ConnectionResetError:
            total = -1
        slow["received"].append(total)
        if slow["close_after"]:
            slow["close_after"].wait(30)
        conn.close()
        if slow["ack_delay"] is not None:
            time.sleep(slow["ack_delay"])
            open(os.path.join(else_vnc_targets.ACKS, str(peer + slow["port_offset"])), "w").close()

    def accept():
        while True:
            conn, _ = server.accept()
            threading.Thread(target=serve, args=(conn,), daemon=True).start()

    threading.Thread(target=accept, daemon=True).start()


def listening(port):
    with socket.socket() as s:
        return s.connect_ex(("127.0.0.1", port)) == 0


def lock_free():
    return subprocess.run(["flock", "-n", LOCK, "true"]).returncode == 0


def wait_until(check, limit=3.0):
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        if check():
            return True
        time.sleep(0.05)
    return check()


def open_ws(host, test=None):
    ws = websocket.create_connection(f"ws://127.0.0.1:{PORT}/", host=host,
                                     subprotocols=["binary"], timeout=10)
    if test:
        test.addCleanup(ws.close)
    return ws, ws.recv()


def refused(host):
    started = time.monotonic()
    try:
        ws, _ = open_ws(host)
    except (websocket.WebSocketException, OSError):
        return time.monotonic() - started
    ws.close()
    raise AssertionError(f"{host} connected while the lock was held")


class Targets(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        os.makedirs(else_vnc_targets.ACKS, exist_ok=True)
        fake_vnc(5900)
        fake_vnc(5901)
        env = dict(os.environ, PYTHONPATH=HERE)
        cls.proxy = subprocess.Popen(
            ["/usr/bin/websockify", "--token-plugin", "else_vnc_targets.Targets", "--host-token", str(PORT)],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        assert wait_until(lambda: listening(PORT), 10), "websockify listens"

    @classmethod
    def tearDownClass(cls):
        cls.proxy.terminate()
        cls.proxy.wait(10)

    def tearDown(self):
        SLOW.update(pause=0.0, close_after=None, ack_delay=0.0, port_offset=0, received=[])
        self.assertTrue(wait_until(lock_free), "the lock is free after each test")

    def test_watch_reaches_the_view_only_server_without_the_lock(self):
        ws, banner = open_ws("else-watch")
        self.assertEqual(banner, b"PORT 5901\n")
        self.assertTrue(lock_free(), "watching leaves the browser lock free")
        ws.close()

    def test_a_missing_lock_is_created_for_the_desktop_user(self):
        if os.path.exists(LOCK):
            os.unlink(LOCK)
        ws, banner = open_ws("127.0.0.1", self)
        self.assertEqual(banner, b"PORT 5900\n")
        st = os.stat(LOCK)
        self.assertEqual((st.st_uid, st.st_mode & 0o777), (os.getuid(), 0o600), "the lock file belongs to the desktop user")
        ws.close()
        self.assertTrue(wait_until(lock_free), "helpers can open and take the created lock")

    def test_control_holds_the_lock_until_it_disconnects(self):
        ws, banner = open_ws("127.0.0.1", self)
        self.assertEqual(banner, b"PORT 5900\n")
        self.assertFalse(lock_free(), "a controlling connection holds the browser lock")
        watch, _ = open_ws("else-watch")
        watch.close()
        ws.close()
        self.assertTrue(wait_until(lock_free), "closing the connection releases the lock")

    def test_one_controller_at_a_time(self):
        ws, _ = open_ws("127.0.0.1", self)
        took = refused("127.0.0.1:6080")
        self.assertLess(took, else_vnc_targets.LOCK_WAIT + 2, "a second controller is refused promptly")
        ws.send_binary(b"x")
        ws.close()

    def test_a_script_holding_the_lock_refuses_take_over_until_it_ends(self):
        holder = subprocess.Popen(["flock", "-x", LOCK, "-c", "echo locked; exec cat > /dev/null"],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        self.addCleanup(holder.kill)
        self.addCleanup(holder.stdout.close)
        self.assertEqual(holder.stdout.readline(), b"locked\n")
        took = refused("127.0.0.1")
        self.assertGreaterEqual(took, else_vnc_targets.LOCK_WAIT - 0.5, "take-over waits briefly for the lock")
        self.assertLess(took, else_vnc_targets.LOCK_WAIT + 2, "then refuses instead of waiting for the script")
        holder.stdin.close()
        holder.wait(5)
        ws, banner = open_ws("127.0.0.1")
        self.assertEqual(banner, b"PORT 5900\n")
        ws.close()

    def test_cdp_helpers_wait_for_the_owner(self):
        ws, _ = open_ws("127.0.0.1", self)
        py = subprocess.run(
            [sys.executable, "-c",
             "import sys; sys.path.insert(0, sys.argv[1]); import elsecdp\n"
             "try:\n elsecdp.connect(timeout=1, lock_timeout=1)\n"
             "except elsecdp.BrowserBusy:\n sys.exit(3)", CDP],
            timeout=20)
        self.assertEqual(py.returncode, 3, "elsecdp.connect reports BrowserBusy while the owner controls")
        node = subprocess.run(
            ["node", "-e",
             "import(process.argv[1]).then(m => m.connect({ timeoutMs: 1000, lockTimeoutMs: 1000 })"
             ".then(() => process.exit(0), e => process.exit(e instanceof m.BrowserBusy ? 3 : 1)))",
             os.path.join(CDP, "cdp.mjs")],
            timeout=20)
        self.assertEqual(node.returncode, 3, "cdp.mjs connect reports BrowserBusy while the owner controls")
        ws.close()

    def test_a_killed_handler_releases_the_lock(self):
        ws, _ = open_ws("127.0.0.1", self)
        children = subprocess.run(["pgrep", "-P", str(self.proxy.pid)], capture_output=True, text=True).stdout.split()
        self.assertEqual(len(children), 1, "one handler process per connection")
        os.kill(int(children[0]), signal.SIGKILL)
        self.assertTrue(wait_until(lock_free), "the kernel releases the lock of a killed handler")
        ws.close()

    def test_the_lock_waits_for_the_server_to_finish_the_input(self):
        done = threading.Event()
        SLOW.update(close_after=done, received=[])
        ws, _ = open_ws("127.0.0.1", self)
        ws.send_binary(b"k" * 64)
        ws.close()
        self.assertTrue(wait_until(lambda: SLOW["received"] == [64]), "the server read all input sent before the close")
        time.sleep(0.5)
        self.assertFalse(lock_free(), "the lock stays held while the server is still on the connection")
        done.set()
        self.assertTrue(wait_until(lock_free, 1.0), "the lock is released once the server acknowledges")

    def test_the_lock_waits_for_the_acknowledgement_not_the_socket_close(self):
        SLOW.update(ack_delay=1.0, received=[])
        ws, _ = open_ws("127.0.0.1", self)
        ws.close()
        self.assertTrue(wait_until(lambda: SLOW["received"]), "the server saw end of input and closed")
        time.sleep(0.6)
        self.assertFalse(lock_free(), "a closed socket without the acknowledgement keeps the lock")
        self.assertTrue(wait_until(lock_free, 2.0), "the acknowledgement releases it")

    def test_without_an_acknowledgement_the_lock_stays_held(self):
        done = threading.Event()
        self.addCleanup(done.set)
        SLOW.update(close_after=done, ack_delay=None, received=[])
        ws, _ = open_ws("127.0.0.1", self)
        ws.close()
        time.sleep(else_vnc_targets.DRAIN_WAIT + 1.5)
        try:
            self.assertFalse(lock_free(), "unknown input state keeps the browser lock")
        finally:
            self.stop_waiting_handler()

    def test_an_acknowledgement_for_another_connection_does_not_release(self):
        SLOW.update(port_offset=1, received=[])
        ws, _ = open_ws("127.0.0.1", self)
        ws.close()
        self.assertTrue(wait_until(lambda: SLOW["received"]))
        time.sleep(0.5)
        try:
            self.assertFalse(lock_free(), "another port's acknowledgement is not this connection's")
        finally:
            self.stop_waiting_handler()

    def stop_waiting_handler(self):
        children = subprocess.run(["pgrep", "-P", str(self.proxy.pid)], capture_output=True, text=True).stdout.split()
        for pid in children:
            os.kill(int(pid), signal.SIGKILL)

    def test_input_queued_past_the_server_socket_is_dropped_at_close(self):
        SLOW.update(pause=2.0, received=[])
        ws, _ = open_ws("127.0.0.1", self)
        sent = 0
        for _ in range(256):
            ws.send_binary(b"k" * 65536)
            sent += 65536
        ws.close()
        self.assertTrue(wait_until(lambda: SLOW["received"], 10), "the server reaches end of input")
        self.assertLess(SLOW["received"][0], sent, "input websockify still queued at the close is dropped")
        self.assertTrue(wait_until(lock_free, 1.0), "and the lock is released once the server acknowledges")

if __name__ == "__main__":
    unittest.main(verbosity=2)
