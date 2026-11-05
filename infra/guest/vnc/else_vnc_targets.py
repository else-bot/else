"""websockify target selection for the desktop (see supervisord.conf).

websockify runs with --host-token, so the token is the WebSocket request's
Host name. The API's in-app viewer sends "else-watch" to reach the view-only
VNC server on 5901, which else-input ignores, so watching never blocks the
bot. Every other Host, such as the noVNC page on 127.0.0.1, reaches the
controlling VNC server on 5900.

A controlling connection takes the browser lock that the CDP helpers,
else-browser and else-input take, so the owner and a bot script never drive
the browser at once. websockify looks the target up in the process it forks
for each connection, and that process exits when the connection ends, so the
kernel releases the lock however the connection ends. If a script holds the
lock, the connection is refused after a short wait.

x11vnc can still be applying a controlling connection's queued input when
the viewer leaves, since a new keysym costs it a settle. So when the viewer
closes, the handler sends x11vnc only what its socket accepts at once, drops
the rest, half-closes, and keeps the lock until x11vnc acknowledges the
connection: x11vnc drops presses queued after the hang-up, releases what it
still holds for the viewer, syncs with the X server, and only then runs its
-gone command, which creates ACKS/<port> for the connection's port. Socket
EOF is no acknowledgement, because x11vnc closes the socket before that
cleanup. After DRAIN_WAIT without one, the handler resets the connection so
x11vnc gives up on it, and keeps waiting: input state unknown, the lock stays
held. websockify has no hook for this, so its handler is wrapped below.
"""

import fcntl
import os
import socket
import struct
import sys
import time

from websockify import websocketproxy

LOCK = "/var/lib/elsebot/browser.lock"
LOCK_WAIT = 3.0
DRAIN_WAIT = 2.0
ACKS = "/var/lib/elsebot/vnc-released"


class BrowserBusy(Exception):
    pass


def take_lock(wait):
    fd = os.open(LOCK, os.O_RDWR | os.O_CREAT, 0o600)
    deadline = time.monotonic() + wait
    while True:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return fd
        except BlockingIOError:
            if time.monotonic() > deadline:
                os.close(fd)
                raise BrowserBusy("BROWSER_BUSY: a script controls the browser") from None
            time.sleep(0.1)


class Targets:
    def __init__(self, source=None):
        self.held = None

    def lookup(self, token):
        if token == "else-watch":
            return ["127.0.0.1", "5901"]
        self.held = take_lock(LOCK_WAIT)
        return ["127.0.0.1", "5900"]


def ack_path(target):
    return os.path.join(ACKS, str(target.getsockname()[1]))


def discard_stale_ack(path):
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass


def wait_for_ack(target, path):
    try:
        target.shutdown(socket.SHUT_WR)
    except OSError:
        pass
    deadline = time.monotonic() + DRAIN_WAIT
    reset = False
    readable = True
    while not os.path.exists(path):
        if not reset and time.monotonic() > deadline:
            reset = True
            print("else_vnc_targets: x11vnc has not finished the viewer's input; resetting the "
                  "connection and keeping the browser lock until it does", file=sys.stderr, flush=True)
            target.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            target.close()
        if readable and not reset:
            target.settimeout(0.005)
            try:
                readable = bool(target.recv(65536))
            except TimeoutError:
                pass
            except OSError:
                readable = False
        else:
            time.sleep(0.005)
    discard_stale_ack(path)


_new_websocket_client = websocketproxy.ProxyRequestHandler.new_websocket_client
_do_proxy = websocketproxy.ProxyRequestHandler.do_proxy
_recv_frames = websocketproxy.ProxyRequestHandler.recv_frames


def new_websocket_client(self):
    try:
        _new_websocket_client(self)
    except OSError:
        # websockify's own cleanup fails on a connection wait_for_ack reset.
        if getattr(self, "control_target", None) is None or self.control_target.fileno() != -1:
            raise


def do_proxy(self, target):
    if getattr(self.server.token_plugin, "held", None) is None:
        return _do_proxy(self, target)
    path = ack_path(target)
    discard_stale_ack(path)
    self.control_target = target
    try:
        return _do_proxy(self, target)
    except BlockingIOError:
        raise self.CClose(1000, "Client closed") from None
    finally:
        wait_for_ack(target, path)


def recv_frames(self):
    bufs, closed = _recv_frames(self)
    if closed and getattr(self, "control_target", None):
        # websockify flushes what is queued with blocking sends; stop at
        # what x11vnc's socket takes now instead.
        self.control_target.setblocking(False)
    return bufs, closed


websocketproxy.ProxyRequestHandler.new_websocket_client = new_websocket_client
websocketproxy.ProxyRequestHandler.do_proxy = do_proxy
websocketproxy.ProxyRequestHandler.recv_frames = recv_frames
