"""Checks the patched x11vnc at the X protocol level, inside the guest image.

Runs as the desktop user with this checkout's infra/guest mounted at /guest:

    docker run --rm --user elsebot -v "$PWD/infra/guest:/guest:ro" \\
        --entrypoint python3 else-guest:3 /guest/x11vnc/test_keymap_settle.py

It starts its own Xvfb, x11vnc with the image's flags (no password), and
websockify with the image's target plugin, then speaks RFB over WebSocket
the way the API's control connection does. An X client with the focus
records every key event as the keysym it reads, so a lost, misread, or
late key shows up as a mismatch. Both keymap paths run: the one x11vnc
picks for this display and the other one.
"""

import configparser
import ctypes
import fcntl
import json
import os
import shlex
import socket
import struct
import subprocess
import sys
import threading
import time
import unittest

import websocket

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(HERE), "vnc"))
import else_vnc_targets  # noqa: E402

DISPLAY = ":120"
WS_PORT = 6080
LOCK = else_vnc_targets.LOCK

KeyPress, KeyRelease, MappingNotify = 2, 3, 34


def keysym(ch):
    cp = ord(ch)
    return cp if cp < 0x100 else 0x1000000 + cp


def cjk(start, n):
    return "".join(chr(0x4E00 + start + i) for i in range(n))


def record():
    x = ctypes.CDLL("libX11.so.6")
    x.XOpenDisplay.restype = ctypes.c_void_p
    x.XDefaultRootWindow.restype = ctypes.c_ulong
    x.XCreateSimpleWindow.restype = ctypes.c_ulong
    for name in ("XSelectInput", "XMapRaised", "XSetInputFocus", "XSync", "XNextEvent",
                 "XLookupString", "XRefreshKeyboardMapping", "XCreateSimpleWindow",
                 "XDefaultRootWindow"):
        getattr(x, name).argtypes = None
    dpy = ctypes.c_void_p(x.XOpenDisplay(DISPLAY.encode()))
    root = ctypes.c_ulong(x.XDefaultRootWindow(dpy))
    win = ctypes.c_ulong(x.XCreateSimpleWindow(dpy, root, 0, 0, 200, 200, 0, 0, 0))
    x.XSelectInput(dpy, win, ctypes.c_long(3))
    x.XMapRaised(dpy, win)
    x.XSync(dpy, 0)
    time.sleep(0.2)
    x.XSetInputFocus(dpy, win, 2, ctypes.c_ulong(0))
    # Xlib loads its keymap on the first lookup; load it before any key
    # arrives so a mapping change cannot slip in while it is being loaded.
    x.XKeysymToKeycode(dpy, ctypes.c_ulong(ord("a")))
    x.XSync(dpy, 0)
    print(json.dumps({"ready": True}), flush=True)
    ev = ctypes.create_string_buffer(192)
    buf = ctypes.create_string_buffer(64)
    ks = ctypes.c_ulong()
    while True:
        x.XNextEvent(dpy, ev)
        kind = struct.unpack_from("i", ev.raw, 0)[0]
        if kind == MappingNotify:
            x.XRefreshKeyboardMapping(ev)
        elif kind in (KeyPress, KeyRelease):
            x.XLookupString(ev, buf, 64, ctypes.byref(ks), None)
            keycode = struct.unpack_from("I", ev.raw, 84)[0]
            print(json.dumps({"t": time.monotonic(), "down": kind == KeyPress,
                              "ks": ks.value, "kc": keycode}), flush=True)


def held_input():
    x = ctypes.CDLL("libX11.so.6")
    x.XOpenDisplay.restype = ctypes.c_void_p
    x.XDefaultRootWindow.restype = ctypes.c_ulong
    dpy = ctypes.c_void_p(x.XOpenDisplay(DISPLAY.encode()))
    keys = ctypes.create_string_buffer(32)
    x.XQueryKeymap(dpy, keys)
    w = ctypes.c_ulong()
    i = ctypes.c_int()
    mask = ctypes.c_uint()
    x.XQueryPointer(dpy, ctypes.c_ulong(x.XDefaultRootWindow(dpy)), ctypes.byref(w), ctypes.byref(w),
                    ctypes.byref(i), ctypes.byref(i), ctypes.byref(i), ctypes.byref(i), ctypes.byref(mask))
    x.XCloseDisplay(dpy)
    down = [k for k in range(256) if keys.raw[k // 8] & (1 << (k % 8))]
    return down, mask.value & 0x1F00


def x11vnc_command():
    conf = configparser.ConfigParser(interpolation=None)
    conf.read("/etc/elsebot/supervisord.conf")
    argv = shlex.split(conf["program:x11vnc"]["command"])
    argv[argv.index("-display") + 1] = DISPLAY
    at = argv.index("-passwdfile")
    argv[at:at + 2] = ["-nopw"]
    return argv


def listening(port):
    with socket.socket() as probe:
        return probe.connect_ex(("127.0.0.1", port)) == 0


def lock_free():
    fd = os.open(LOCK, os.O_RDWR | os.O_CREAT, 0o600)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return True
    except BlockingIOError:
        return False
    finally:
        os.close(fd)


def wait_until(check, limit):
    deadline = time.monotonic() + limit
    while time.monotonic() < deadline:
        if check():
            return True
        time.sleep(0.005)
    return check()


class Rfb:

    def __init__(self):
        self.ws = websocket.create_connection(f"ws://127.0.0.1:{WS_PORT}/", host="127.0.0.1",
                                              subprotocols=["binary"], timeout=10)
        self.buf = b""
        assert self.read(12).startswith(b"RFB 003.")
        self.ws.send_binary(b"RFB 003.008\n")
        n = self.read(1)[0]
        assert 1 in self.read(n), "the test server offers no authentication"
        self.ws.send_binary(b"\x01")
        assert self.read(4) == b"\0\0\0\0"
        self.ws.send_binary(b"\x01")
        name_len = struct.unpack(">I", self.read(24)[20:24])[0]
        self.read(name_len)

    def read(self, n):
        while len(self.buf) < n:
            self.buf += self.ws.recv()
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def keys(self, events):
        self.ws.send_binary(b"".join(struct.pack(">BBxxI", 4, down, ks) for ks, down in events))

    def type(self, text):
        self.keys([e for ch in text for e in ((keysym(ch), 1), (keysym(ch), 0))])

    def pointer(self, buttons, x=10, y=10):
        self.ws.send_binary(struct.pack(">BBHH", 5, buttons, x, y))

    def close(self):
        self.ws.close()


class KeymapSettle(unittest.TestCase):
    extra = []

    @classmethod
    def setUpClass(cls):
        cls.procs = []
        cls.start(["Xvfb", DISPLAY, "-screen", "0", "640x480x24", "-nolisten", "tcp"])
        time.sleep(1)
        cls.x11vnc_log = open(f"/tmp/x11vnc{''.join(cls.extra)}.log", "w+")
        os.makedirs(else_vnc_targets.ACKS, exist_ok=True)
        cls.start(x11vnc_command() + cls.extra, stderr=cls.x11vnc_log)
        env = dict(os.environ, PYTHONPATH=os.path.join(os.path.dirname(HERE), "vnc"))
        cls.start(["websockify", "--token-plugin", "else_vnc_targets.Targets", "--host-token",
                   str(WS_PORT)], env=env)
        for port in (5900, WS_PORT):
            assert wait_until(lambda: listening(port), 10)
        cls.recorder = subprocess.Popen([sys.executable, __file__, "--record"], stdout=subprocess.PIPE,
                                        env=dict(os.environ, DISPLAY=DISPLAY), text=True)
        cls.procs.append(cls.recorder)
        assert json.loads(cls.recorder.stdout.readline()) == {"ready": True}
        cls.events = []
        threading.Thread(target=cls.collect, daemon=True).start()

    @classmethod
    def start(cls, argv, **kw):
        kw.setdefault("stdout", subprocess.DEVNULL)
        kw.setdefault("stderr", subprocess.DEVNULL)
        cls.procs.append(subprocess.Popen(argv, **kw))

    @classmethod
    def collect(cls):
        for line in cls.recorder.stdout:
            cls.events.append(json.loads(line))

    @classmethod
    def tearDownClass(cls):
        for p in reversed(cls.procs):
            p.terminate()
            p.wait(10)
            if p.stdout is not None:
                p.stdout.close()
        cls.x11vnc_log.close()

    def setUp(self):
        self.assertTrue(wait_until(lock_free, 5), "no control connection is left from a previous test")
        del self.events[:]

    def typed(self, expect_presses, limit):
        def enough():
            return sum(e["down"] for e in self.events) >= expect_presses
        wait_until(enough, limit)
        time.sleep(0.2)
        return "".join(chr(e["ks"] if e["ks"] < 0x100 else e["ks"] - 0x1000000)
                       for e in self.events if e["down"])

    def test_mixed_text_and_100_distinct_new_characters_arrive_in_order(self):
        text = "native-é漢字かな" + cjk(200, 100)
        rfb = Rfb()
        self.addCleanup(rfb.close)
        rfb.type(text)
        self.assertEqual(self.typed(len(text), 30), text)
        rfb.close()

    def test_recycled_keycodes_do_not_retype_earlier_characters(self):
        mixed = "aé漢ü字ñか"
        rfb = Rfb()
        self.addCleanup(rfb.close)
        rfb.type(mixed)
        rfb.type(cjk(400, 40))
        rfb.type(mixed)
        expect = mixed + cjk(400, 40) + mixed
        self.assertEqual(self.typed(len(expect), 30), expect)
        rfb.close()

    def test_a_held_key_keeps_its_keycode_while_others_are_recycled(self):
        held = keysym("鍵")
        rfb = Rfb()
        self.addCleanup(rfb.close)
        rfb.keys([(held, 1)])
        rfb.type(cjk(600, 40))
        rfb.keys([(held, 0)])
        self.typed(41, 30)
        wait_until(lambda: any(not e["down"] and e["ks"] == held for e in self.events), 5)
        press = next(e for e in self.events if e["down"] and e["ks"] == held)
        others = [e for e in self.events if e["ks"] != held]
        self.assertEqual([e["ks"] for e in others if e["down"]], [keysym(ch) for ch in cjk(600, 40)], self.events)
        self.assertNotIn(press["kc"], {e["kc"] for e in others}, "no other key used the held keycode")
        self.assertTrue(any(not e["down"] and e["ks"] == held and e["kc"] == press["kc"] for e in self.events),
                        "the held key is released on its own keycode")
        rfb.close()

    def test_a_long_commit_stops_at_hang_up_and_nothing_follows_the_lock(self):
        text = cjk(1000, 4096)
        rfb = Rfb()
        self.addCleanup(rfb.close)
        rfb.keys([(0xFFE1, 1)])
        rfb.pointer(1)
        rfb.type(text)
        rfb.keys([(0xFFE1, 0)])
        rfb.pointer(0)
        closed = time.monotonic()
        rfb.close()
        self.assertTrue(wait_until(lock_free, else_vnc_targets.DRAIN_WAIT + 5), "the lock is released")
        freed = time.monotonic()
        time.sleep(1.0)
        presses = [e for e in self.events if e["down"] and e["ks"] != 0xFFE1]
        late = [e for e in self.events if e["t"] > freed]
        down, buttons = held_input()
        summary = {"extra": self.extra, "typed": len(presses), "of": len(text),
                   "lock_released_after_close_s": round(freed - closed, 3),
                   "events_after_lock": len(late), "keys_down": down, "buttons": buttons}
        print(json.dumps(summary), file=sys.stderr, flush=True)
        self.assertEqual(late, [], "no key event reaches the display after the lock is released")
        self.assertEqual((down, buttons), ([], 0), "nothing is left held")
        self.assertLess(len(presses), len(text), "presses still queued at the hang-up were dropped")
        typed = "".join(chr(e["ks"] - 0x1000000) for e in presses)
        self.assertEqual(typed, text[:len(typed)], "what was typed is an in-order prefix")
        self.assertLess(freed - closed, else_vnc_targets.DRAIN_WAIT, "x11vnc finished before the drain wait")

    def test_the_lock_is_released_only_after_x11vnc_releases_held_input(self):
        rfb = Rfb()
        rfb.keys([(0xFFE1, 1), (keysym("鍵"), 1)])
        rfb.pointer(1)
        rfb.type(cjk(1000, 200))
        state = {}

        def take_lock():
            fd = os.open(LOCK, os.O_RDWR)
            fcntl.flock(fd, fcntl.LOCK_EX)
            state["at_lock"] = held_input()
            state["events_before"] = len(self.events)
            fcntl.flock(fd, fcntl.LOCK_UN)
            os.close(fd)

        time.sleep(0.3)
        waiter = threading.Thread(target=take_lock)
        waiter.start()
        rfb.ws.sock.close()
        waiter.join(else_vnc_targets.DRAIN_WAIT + 5)
        self.assertEqual(state.get("at_lock"), ([], 0), "keys and buttons are released before the lock is")
        time.sleep(0.5)
        self.assertEqual(len(self.events), state["events_before"], "no key event follows the lock")

    def test_reconnect_after_forced_release_does_not_restore_a_modifier(self):
        rfb = Rfb()
        rfb.keys([(0xFFE1, 1)])
        self.assertTrue(wait_until(lambda: bool(held_input()[0]), 5))
        rfb.ws.sock.close()
        self.assertTrue(wait_until(lock_free, 5))
        self.assertEqual(held_input(), ([], 0))
        del self.events[:]
        next_rfb = Rfb()
        self.addCleanup(next_rfb.close)
        next_rfb.type("a")
        self.assertTrue(wait_until(lambda: any(e["down"] for e in self.events), 5))
        time.sleep(0.2)
        self.assertEqual([e["ks"] for e in self.events if e["down"]], [ord("a")])
        self.assertEqual(held_input(), ([], 0), "typing after reconnect never restores Shift")
        next_rfb.keys([(0xFFE3, 1), (ord("a"), 1), (ord("a"), 0), (0xFFE3, 0)])
        self.assertTrue(wait_until(lambda: sum(e["down"] for e in self.events) >= 3, 5))
        self.assertTrue(wait_until(lambda: held_input() == ([], 0), 5))
        next_rfb.close()

    def test_a_press_with_every_added_keycode_held_is_ignored_without_naming_it(self):
        rfb = Rfb()
        self.addCleanup(rfb.close)
        pool = cjk(2000, 200)
        rfb.keys([(keysym(ch), 1) for ch in pool])
        self.typed(1, 30)
        time.sleep(0.5)
        accepted = [e for e in self.events if e["down"]]
        rfb.keys([(keysym(ch), 0) for ch in pool])
        rfb.close()
        self.assertTrue(0 < len(accepted) < len(pool), "presses beyond the free keycodes are ignored")
        self.x11vnc_log.seek(0)
        log = self.x11vnc_log.read()
        self.assertIn("keystroke ignored, every added keycode is held", log)
        for ch in pool:
            self.assertNotIn("%x" % keysym(ch), log.lower(), "the log never names a key")
        self.assertTrue(wait_until(lambda: held_input() == ([], 0), 5), "every key is released")


class KeymapSettleXkb(KeymapSettle):
    extra = ["-xkb"]


class KeymapSettleNoXkb(KeymapSettle):
    extra = ["-noxkb"]


if __name__ == "__main__":
    if sys.argv[1:] == ["--record"]:
        record()
    else:
        unittest.main(verbosity=2)
