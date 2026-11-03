"""else.bot guest CDP helper for Python scripts (POC-027). Standard library only.

Drives the computer's own visible Chromium (the one the owner sees over VNC)
through its DevTools endpoint on guest loopback, 127.0.0.1:9222. The endpoint
is fixed: this module never connects anywhere else. It works from any Python 3,
including a virtual environment:

    import sys; sys.path.insert(0, "/usr/local/lib/elsebot/cdp")
    import elsecdp
    with elsecdp.connect() as browser:          # waits for the browser, takes the lock
        page = browser.page(create="http://127.0.0.1:8000/")
        page.fill("#q", "picnic"); page.click("button")
        print(page.read("document.title"))

Control is serialized: connect() takes the computer's browser lock, which
computer_browser also takes. Reads are retried once after a browser restart;
actions that change a page (click, fill, navigate, evaluate) are never repeated
automatically: a lost reply raises Uncertain, and the script decides. Errors:
BrowserNotReady, BrowserBusy, TargetClosed, NavigationTimeout, Uncertain,
CdpError.
"""

import base64
import fcntl
import hashlib
import json
import os
import socket
import struct
import time
import urllib.parse
import urllib.request

ENDPOINT = "http://127.0.0.1:9222"
LOCK = "/var/lib/elsebot/browser.lock"


class CdpError(Exception):
    pass


class BrowserNotReady(CdpError):
    pass


class BrowserBusy(CdpError):
    pass


class TargetClosed(CdpError):
    pass


class NavigationTimeout(CdpError):
    pass


class Uncertain(CdpError):
    """The request was sent but no reply came: it may or may not have happened."""


def _get_json(path, timeout=2):
    with urllib.request.urlopen(ENDPOINT + path, timeout=timeout) as r:
        return json.load(r)


def ready(timeout=30):
    """Wait until DevTools answers (the browser may be starting or restarting)."""
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            return _get_json("/json/version")
        except Exception as e:  # noqa: BLE001 - any failure means not yet
            last = e
            time.sleep(0.25)
    raise BrowserNotReady(f"the browser did not answer at {ENDPOINT} within {timeout} s ({last})")


class _Socket:
    """A minimal WebSocket client (RFC 6455 text frames) for CDP over loopback."""

    def __init__(self, url, timeout):
        u = urllib.parse.urlparse(url)
        if u.hostname != "127.0.0.1" or u.port != 9222:
            raise CdpError(f"refusing a debugger endpoint other than {ENDPOINT}: {url}")
        self.sock = socket.create_connection(("127.0.0.1", 9222), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall(
            (f"GET {u.path} HTTP/1.1\r\nHost: 127.0.0.1:9222\r\nUpgrade: websocket\r\n"
             f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise CdpError("DevTools closed the connection during the handshake")
            head += chunk
        head, self.buf = head.split(b"\r\n\r\n", 1)
        accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        if b" 101 " not in head.split(b"\r\n", 1)[0] or accept.encode() not in head:
            raise CdpError("bad WebSocket handshake")
        self.closed = False

    def _read(self, n):
        while len(self.buf) < n:
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                raise
            except OSError:
                chunk = b""
            if not chunk:
                self.closed = True
                raise TargetClosed("the DevTools connection closed")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def recv(self, timeout):
        self.sock.settimeout(timeout)
        parts = []
        while True:
            b0, b1 = self._read(2)
            op, fin, n = b0 & 0x0F, b0 & 0x80, b1 & 0x7F
            if n == 126:
                n = struct.unpack(">H", self._read(2))[0]
            elif n == 127:
                n = struct.unpack(">Q", self._read(8))[0]
            payload = self._read(n)
            if op == 8:
                self.closed = True
                raise TargetClosed("DevTools closed the connection")
            if op == 9:
                self._frame(10, payload)
                continue
            if op in (0, 1):
                parts.append(payload)
                if fin:
                    return b"".join(parts).decode("utf-8")

    def _frame(self, op, data):
        mask = os.urandom(4)
        n = len(data)
        if n < 126:
            header = struct.pack(">BB", 0x80 | op, 0x80 | n)
        elif n < 65536:
            header = struct.pack(">BBH", 0x80 | op, 0x80 | 126, n)
        else:
            header = struct.pack(">BBQ", 0x80 | op, 0x80 | 127, n)
        body = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
        self.sock.sendall(header + mask + body)

    def send(self, text):
        if self.closed:
            raise TargetClosed("the DevTools connection is closed")
        try:
            self._frame(1, text.encode("utf-8"))
        except OSError as e:
            self.closed = True
            raise TargetClosed(f"the DevTools connection failed: {e}") from e

    def close(self):
        if not self.closed:
            try:
                self._frame(8, b"")
            except OSError:
                pass
            self.closed = True
        self.sock.close()


class _Connection:
    def __init__(self, url, timeout):
        self.socket = _Socket(url, timeout)
        self.next = 1

    def call(self, method, params=None, timeout=30, mutating=True):
        msg_id = self.next
        self.next += 1
        self.socket.send(json.dumps({"id": msg_id, "method": method, "params": params or {}}))
        deadline = time.monotonic() + timeout
        while True:
            left = deadline - time.monotonic()
            try:
                if left <= 0:
                    raise socket.timeout()
                msg = json.loads(self.socket.recv(left))
            except socket.timeout:
                if mutating:
                    raise Uncertain(f"{method} did not answer within {timeout} s; it may or may not have happened") from None
                raise CdpError(f"{method} did not answer within {timeout} s") from None
            except TargetClosed:
                if mutating:
                    raise Uncertain(f"the connection closed before {method} answered; it may or may not have happened") from None
                raise
            if msg.get("id") == msg_id:
                if "error" in msg:
                    raise CdpError(f"{msg['error'].get('message')} ({msg['error'].get('code')})")
                return msg.get("result", {})

    @property
    def closed(self):
        return self.socket.closed

    def close(self):
        self.socket.close()


class Page:
    def __init__(self, browser, target, timeout):
        self.browser = browser
        self.target = target
        self.timeout = timeout
        self.conn = _Connection(target["webSocketDebuggerUrl"], timeout)

    @property
    def id(self):
        return self.target["id"]

    def call(self, method, params=None, timeout=30, mutating=True):
        """A CDP command on this page. Pass mutating=False for pure reads."""
        if self.conn.closed:
            if not mutating:
                self.reconnect()
            else:
                raise TargetClosed("the page connection is closed; call reconnect() and check the page before acting again")
        try:
            return self.conn.call(method, params, timeout, mutating)
        except TargetClosed:
            if mutating:
                raise
            self.reconnect()
            return self.conn.call(method, params, timeout, mutating)

    def evaluate(self, expression, mutating=True, timeout=30):
        r = self.call("Runtime.evaluate", {"expression": expression, "returnByValue": True, "awaitPromise": True}, timeout, mutating)
        if "exceptionDetails" in r:
            d = r["exceptionDetails"]
            raise CdpError(f"page script failed: {d.get('exception', {}).get('description', d.get('text'))}")
        return r.get("result", {}).get("value")

    def read(self, expression, timeout=30):
        """Evaluate a side-effect-free expression (retried once after a restart)."""
        return self.evaluate(expression, mutating=False, timeout=timeout)

    def text(self):
        return self.read("document.body ? document.body.innerText : ''")

    def navigate(self, url, timeout=30):
        """Navigate and wait for the page to load (bounded)."""
        try:
            r = self.call("Page.navigate", {"url": url}, timeout)
        except Uncertain:
            # Chromium answers Page.navigate once the response starts; a
            # server that never answers is a navigation timeout.
            if self.conn.closed:
                raise
            raise NavigationTimeout(f"loading {url} did not start within {timeout} s; the tab may still be loading") from None
        if r.get("errorText"):
            raise CdpError(f"navigation to {url} failed: {r['errorText']}")
        self.wait_for("document.readyState === 'complete' && location.href !== 'about:blank'", timeout, f"loading {url}", NavigationTimeout)

    def wait_for(self, expression, timeout=10, what=None, error=CdpError):
        deadline = time.monotonic() + timeout
        while True:
            try:
                if self.read(f"!!({expression})", timeout=5):
                    return
            except TargetClosed:
                raise
            except CdpError:
                pass
            if time.monotonic() > deadline:
                raise error(f"timed out after {timeout} s waiting for {what or expression}")
            time.sleep(0.15)

    def fill(self, selector, value):
        ok = self.evaluate(
            f"(() => {{ const e = document.querySelector({json.dumps(selector)}); if (!e) return false; e.focus(); "
            f"e.value = {json.dumps(value)}; e.dispatchEvent(new Event('input', {{ bubbles: true }})); "
            f"e.dispatchEvent(new Event('change', {{ bubbles: true }})); return true; }})()")
        if not ok:
            raise CdpError(f"no element matches {selector}")

    def click(self, selector):
        ok = self.evaluate(f"(() => {{ const e = document.querySelector({json.dumps(selector)}); if (!e) return false; e.click(); return true; }})()")
        if not ok:
            raise CdpError(f"no element matches {selector}")

    def show(self):
        """Make this the tab shown in the visible browser window."""
        self.call("Page.bringToFront", mutating=False)

    def reconnect(self, timeout=30):
        """After a browser restart: find this page again (same target, or the
        same URL), or raise TargetClosed. Never repeats an action."""
        ready(timeout)
        pages = [t for t in _get_json("/json/list") if t.get("type") == "page"]
        found = next((t for t in pages if t["id"] == self.target["id"]), None) or next(
            (t for t in pages if self.target.get("url") and t["url"] == self.target["url"]), None)
        if not found:
            raise TargetClosed(f"the page {self.target.get('url') or self.target['id']} is gone")
        self.target = found
        self.conn = _Connection(found["webSocketDebuggerUrl"], timeout)

    def close(self):
        self.conn.close()


class Browser:
    def __init__(self, lock_fd, timeout):
        self.lock_fd = lock_fd
        self.timeout = timeout
        self.pages = []

    def targets(self):
        return [t for t in _get_json("/json/list") if t.get("type") == "page"]

    def page(self, id=None, url_prefix=None, title=None, create=None):
        """Attach to a page by id, url_prefix, or title; or create a new tab at
        a URL (shown in the visible window). Without a match and without
        create, the first page tab."""
        if create:
            version = ready(self.timeout)
            conn = _Connection(version["webSocketDebuggerUrl"], self.timeout)
            try:
                target_id = conn.call("Target.createTarget", {"url": "about:blank"})["targetId"]
            finally:
                conn.close()
            target = next((t for t in self.targets() if t["id"] == target_id), None)
            if not target:
                raise TargetClosed("the new tab disappeared")
        else:
            pages = self.targets()
            target = next((t for t in pages if (id and t["id"] == id) or (url_prefix and t["url"].startswith(url_prefix))
                           or (title and t["title"] == title)), None)
            if target is None and not (id or url_prefix or title) and pages:
                target = pages[0]
            if target is None:
                raise TargetClosed(f"no page matches {dict(id=id, url_prefix=url_prefix, title=title)}")
        page = Page(self, target, self.timeout)
        self.pages.append(page)
        page.show()
        if create:
            page.navigate(create, self.timeout)
            page.target["url"] = page.read("location.href")
        return page

    def close(self):
        for p in self.pages:
            p.close()
        if self.lock_fd is not None:
            os.close(self.lock_fd)
            self.lock_fd = None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()


def connect(timeout=30, lock_timeout=60):
    """Wait for the visible browser and take the browser lock."""
    fd = os.open(LOCK, os.O_RDWR | os.O_CREAT, 0o600)
    deadline = time.monotonic() + lock_timeout
    while True:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            break
        except BlockingIOError:
            if time.monotonic() > deadline:
                os.close(fd)
                raise BrowserBusy("another script or computer_browser controls the browser; wait for it or cancel its job") from None
            time.sleep(0.2)
    try:
        ready(timeout)
    except Exception:
        os.close(fd)
        raise
    return Browser(fd, timeout)
