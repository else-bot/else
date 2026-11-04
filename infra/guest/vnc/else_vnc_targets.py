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
"""

import fcntl
import os
import time

LOCK = "/var/lib/elsebot/browser.lock"
LOCK_WAIT = 3.0


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
