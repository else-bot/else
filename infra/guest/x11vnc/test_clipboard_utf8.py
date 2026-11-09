"""Checks the patched x11vnc's clipboard at the X protocol level, inside the guest image.

Runs as the desktop user with this checkout's infra/guest mounted at /guest:

    docker run --rm --user elsebot -v "$PWD/infra/guest:/guest:ro" \\
        --entrypoint python3 else-guest:3 /guest/x11vnc/test_clipboard_utf8.py

It starts its own Xvfb, x11vnc with the image's flags (no password), and
websockify, then sends clipboard text the way the API's control connection
does: UTF-8 through the ExtendedClipboard extension, or Latin-1 from a
classic viewer. An X client then reads CLIPBOARD and PRIMARY as each target
a real application asks for.
"""

import ctypes
import os
import struct
import subprocess
import sys
import time
import unittest
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from test_keymap_settle import (  # noqa: E402
    DISPLAY, WS_PORT, Rfb, else_vnc_targets, listening, lock_free, wait_until, x11vnc_command,
)

EXTENDED_CLIPBOARD = 0xC0A1E5CE
CAPS, PROVIDE, TEXT = 1 << 24, 1 << 28, 1
SelectionNotify = 31


class Selections:
    """An X client that asks the selection owner for its text."""

    def __init__(self):
        x = ctypes.CDLL("libX11.so.6")
        x.XOpenDisplay.restype = ctypes.c_void_p
        x.XDefaultRootWindow.restype = ctypes.c_ulong
        x.XCreateSimpleWindow.restype = ctypes.c_ulong
        x.XInternAtom.restype = ctypes.c_ulong
        x.XGetAtomName.restype = ctypes.c_char_p
        x.XGetWindowProperty.argtypes = [
            ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_long, ctypes.c_long, ctypes.c_int,
            ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.POINTER(ctypes.c_ubyte))]
        self.x = x
        self.dpy = ctypes.c_void_p(x.XOpenDisplay(DISPLAY.encode()))
        root = ctypes.c_ulong(x.XDefaultRootWindow(self.dpy))
        self.win = ctypes.c_ulong(x.XCreateSimpleWindow(self.dpy, root, 0, 0, 1, 1, 0, 0, 0))
        self.prop = self.atom("ELSE_TEST_SELECTION")

    def atom(self, name):
        return ctypes.c_ulong(self.x.XInternAtom(self.dpy, name.encode(), 0))

    def read(self, selection, target):
        """(type name, bytes) the owner of `selection` gives for `target`."""
        self.x.XConvertSelection(self.dpy, self.atom(selection), self.atom(target), self.prop, self.win,
                                 ctypes.c_ulong(0))
        self.x.XFlush(self.dpy)
        ev = ctypes.create_string_buffer(192)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if self.x.XCheckTypedWindowEvent(self.dpy, self.win, SelectionNotify, ev):
                break
            time.sleep(0.01)
        else:
            raise AssertionError(f"no reply for {selection} as {target}")
        kind = ctypes.c_ulong()
        fmt = ctypes.c_int()
        count = ctypes.c_ulong()
        after = ctypes.c_ulong()
        data = ctypes.POINTER(ctypes.c_ubyte)()
        self.x.XGetWindowProperty(self.dpy, self.win.value, self.prop.value, 0, 1 << 20, 1, 0,
                                  ctypes.byref(kind), ctypes.byref(fmt), ctypes.byref(count),
                                  ctypes.byref(after), ctypes.byref(data))
        if fmt.value == 32:
            raw = [ctypes.cast(data, ctypes.POINTER(ctypes.c_ulong))[i] for i in range(count.value)]
            value = [self.x.XGetAtomName(self.dpy, ctypes.c_ulong(a)).decode() for a in raw]
        else:
            value = bytes(data[:count.value]) if count.value else b""
        if data:
            self.x.XFree(data)
        name = self.x.XGetAtomName(self.dpy, kind).decode() if kind.value else None
        return name, value

    def close(self):
        self.x.XCloseDisplay(self.dpy)


def caps(rfb):
    """Asks for ExtendedClipboard and returns the server's capability flags."""
    rfb.ws.send_binary(struct.pack(">BxHiI", 2, 2, 0, EXTENDED_CLIPBOARD))
    head = rfb.read(8)
    assert head[0] == 3, "the server answers with a ServerCutText"
    length = -struct.unpack(">i", head[4:8])[0]
    body = rfb.read(length)
    return struct.unpack(">I", body[:4])[0]


def provide(rfb, text):
    payload = text.encode() + b"\0"
    packed = zlib.compress(struct.pack(">I", len(payload)) + payload)
    rfb.ws.send_binary(struct.pack(">BxxxiI", 6, -(4 + len(packed)), PROVIDE | TEXT) + packed)


def classic(rfb, data):
    rfb.ws.send_binary(struct.pack(">BxxxI", 6, len(data)) + data)


class ClipboardUtf8(unittest.TestCase):

    @classmethod
    def setUpClass(cls):
        cls.procs = []
        cls.start(["Xvfb", DISPLAY, "-screen", "0", "640x480x24", "-nolisten", "tcp"])
        time.sleep(1)
        os.makedirs(else_vnc_targets.ACKS, exist_ok=True)
        cls.start(x11vnc_command())
        env = dict(os.environ, PYTHONPATH=os.path.join(os.path.dirname(HERE), "vnc"))
        cls.start(["websockify", "--token-plugin", "else_vnc_targets.Targets", "--host-token",
                   str(WS_PORT)], env=env)
        for port in (5900, WS_PORT):
            assert wait_until(lambda: listening(port), 10)
        cls.selections = Selections()
        # x11vnc creates the window that owns its selections 8 to 45 seconds
        # after it starts, once it is sure no display manager is running.
        rfb = Rfb()
        try:
            ready = wait_until(lambda: classic(rfb, b"ready") or cls.selections.read("CLIPBOARD", "STRING")[1] == b"ready",
                               60)
        finally:
            rfb.close()
        assert ready, "x11vnc never took the selection"

    @classmethod
    def start(cls, argv, **kw):
        kw.setdefault("stdout", subprocess.DEVNULL)
        kw.setdefault("stderr", subprocess.DEVNULL)
        cls.procs.append(subprocess.Popen(argv, **kw))

    @classmethod
    def tearDownClass(cls):
        cls.selections.close()
        for p in reversed(cls.procs):
            p.terminate()
            p.wait(10)

    def setUp(self):
        self.assertTrue(wait_until(lock_free, 5), "no control connection is left from a previous test")
        self.rfb = Rfb()
        self.addCleanup(self.rfb.close)

    def owned(self, expect):
        return wait_until(lambda: self.selections.read("CLIPBOARD", "UTF8_STRING")[1] == expect, 5)

    def test_utf8_text_reaches_both_selections_and_falls_back_to_latin1(self):
        flags = caps(self.rfb)
        self.assertTrue(flags & CAPS and flags & TEXT and flags & PROVIDE, hex(flags))
        text = "héllo ‘quoted’ 漢字 🙂\nsecond line"
        provide(self.rfb, text)
        self.assertTrue(self.owned(text.encode()), self.selections.read("CLIPBOARD", "UTF8_STRING"))
        for selection in ("CLIPBOARD", "PRIMARY"):
            self.assertEqual(self.selections.read(selection, "UTF8_STRING"), ("UTF8_STRING", text.encode()))
            self.assertEqual(self.selections.read(selection, "STRING"),
                             ("STRING", "héllo ?quoted? ?? ?\nsecond line".encode("latin-1")))
        name, targets = self.selections.read("CLIPBOARD", "TARGETS")
        self.assertEqual(name, "ATOM")
        self.assertEqual(targets, ["TARGETS", "UTF8_STRING", "STRING"])

    def test_a_classic_viewer_still_sets_latin1_text(self):
        classic(self.rfb, "café".encode("latin-1"))
        self.assertTrue(self.owned("café".encode()), self.selections.read("CLIPBOARD", "UTF8_STRING"))
        self.assertEqual(self.selections.read("CLIPBOARD", "STRING"), ("STRING", "café".encode("latin-1")))

    def test_malformed_utf8_becomes_question_marks_in_latin1(self):
        caps(self.rfb)
        payload = b"ok\xff\xc3(\xe2\x82end\0"
        packed = zlib.compress(struct.pack(">I", len(payload)) + payload)
        self.rfb.ws.send_binary(struct.pack(">BxxxiI", 6, -(4 + len(packed)), PROVIDE | TEXT) + packed)
        self.assertTrue(wait_until(
            lambda: self.selections.read("CLIPBOARD", "STRING")[1] == b"ok??(?end", 5),
            self.selections.read("CLIPBOARD", "STRING"))


if __name__ == "__main__":
    unittest.main(verbosity=2)
