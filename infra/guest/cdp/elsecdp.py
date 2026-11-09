"""else.bot guest CDP helper for Python scripts. Standard library only.

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
computer_browser also takes. Nothing that runs in a page is ever sent twice: a
lost reply raises Uncertain (for actions) or TargetClosed, and the script
decides. Page code runs with a browser-side time limit, and the tabs a script
controls are recorded, so if the script is stopped the computer closes the tabs
it opened and halts code in the ones it attached to. A native dialog (alert,
confirm, prompt, "Leave site?") pauses its tab: calls fail fast with DialogOpen
until page.handle_dialog() answers it, and navigate() stays on a page that asks
to confirm leaving unless told leave=True, so unsent input survives. Errors:
BrowserNotReady, BrowserBusy, TargetClosed, NavigationTimeout,
NavigationBlocked, DialogOpen, Uncertain, CdpError.
"""

import base64
import fcntl
import hashlib
import json
import os
import select
import socket
import struct
import subprocess
import time
import urllib.parse
import urllib.request

ENDPOINT = "http://127.0.0.1:9222"
DESKTOP = "/usr/local/bin/else-desktop"
SCREEN = "/tmp/.X11-unix/X99"
LOCK = "/var/lib/elsebot/browser.lock"
MAX_EVALUATE = 300
CONTROLLED = "/var/lib/elsebot/browser-controlled.jsonl"


def _record(target, created):
    fd = os.open(CONTROLLED, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(fd, (json.dumps({"pid": os.getpid(), "target": target, "created": created}) + "\n").encode())
    finally:
        os.close(fd)


def _forget():
    try:
        with open(CONTROLLED) as f:
            lines = f.read().splitlines()
        kept = []
        for line in lines:
            try:
                if json.loads(line)["pid"] != os.getpid():
                    kept.append(line)
            except Exception:  # noqa: BLE001 - drop unreadable lines
                pass
        with open(CONTROLLED, "w") as f:
            f.write("".join(l + "\n" for l in kept))
    except OSError:
        pass


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


class NavigationBlocked(CdpError):
    """The page asked to confirm leaving; navigate() stayed on it."""


class DialogOpen(Exception):
    """A native dialog pauses the tab; .dialog holds its type and message."""

    def __init__(self, dialog):
        message = f" ({json.dumps(dialog.get('message'))})" if dialog.get("message") else ""
        super().__init__(f"a {dialog.get('type')} dialog is open on {dialog.get('url')}{message}; the tab is paused "
                         "until page.handle_dialog(accept) answers it, and an action that opened it has already happened")
        self.dialog = dialog


def _get_json(path, timeout=2):
    with urllib.request.urlopen(ENDPOINT + path, timeout=timeout) as r:
        return json.load(r)


def ready(timeout=30):
    """Wait until DevTools answers (the browser may be starting or restarting)."""
    deadline = time.monotonic() + timeout
    last = None
    # Only a desktop that never started is started here; a browser that is
    # restarting comes back by itself.
    started = os.path.exists(SCREEN) or not os.access(DESKTOP, os.X_OK)
    while time.monotonic() < deadline:
        try:
            return _get_json("/json/version")
        except Exception as e:  # noqa: BLE001 - any failure means not yet
            last = e
            if not started:
                # The desktop starts on first use.
                started = True
                try:
                    subprocess.run([DESKTOP], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                   timeout=max(1, deadline - time.monotonic()), check=False)
                except subprocess.TimeoutExpired:
                    pass
                continue
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
        self.parts = []

    def recv(self, deadline):
        """The next text message, by an absolute monotonic deadline. Frame
        parsing state survives a timeout: partial frames stay buffered."""
        while True:
            frame = self._parse()
            if frame is not None:
                op, fin, payload = frame
                if op == 8:
                    self.closed = True
                    raise TargetClosed("DevTools closed the connection")
                if op == 9:
                    self._frame(10, payload)
                    continue
                if op in (0, 1):
                    self.parts.append(payload)
                    if fin:
                        text = b"".join(self.parts).decode("utf-8")
                        self.parts = []
                        return text
                continue
            left = deadline - time.monotonic()
            if left <= 0:
                if not select.select([self.sock], [], [], 0)[0]:
                    raise socket.timeout()
                left = 1
            self.sock.settimeout(left)
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

    def _parse(self):
        """One complete frame from the buffer, or None (nothing consumed)."""
        b = self.buf
        if len(b) < 2:
            return None
        op, fin, n, at = b[0] & 0x0F, b[0] & 0x80, b[1] & 0x7F, 2
        if n == 126:
            if len(b) < 4:
                return None
            n, at = struct.unpack(">H", b[2:4])[0], 4
        elif n == 127:
            if len(b) < 10:
                return None
            n, at = struct.unpack(">Q", b[2:10])[0], 10
        if len(b) < at + n:
            return None
        self.buf = b[at + n:]
        return op, fin, b[at:at + n]

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
    def __init__(self, url, timeout, on_event):
        self.socket = _Socket(url, timeout)
        self.next = 1
        self.on_event = on_event

    def poll(self):
        """Handle the events that already arrived, without waiting."""
        while True:
            try:
                msg = json.loads(self.socket.recv(time.monotonic()))
            except socket.timeout:
                return
            if "method" in msg:
                try:
                    self.on_event(msg)
                except DialogOpen:
                    pass

    def call(self, method, params=None, timeout=30, mutating=True):
        """on_event may raise to stop waiting; a late reply is ignored."""
        msg_id = self.next
        self.next += 1
        self.socket.send(json.dumps({"id": msg_id, "method": method, "params": params or {}}))
        deadline = time.monotonic() + timeout
        while True:
            try:
                msg = json.loads(self.socket.recv(deadline))
            except socket.timeout:
                if mutating:
                    raise Uncertain(f"{method} did not answer within {timeout} s; it may or may not have happened") from None
                raise CdpError(f"{method} did not answer within {timeout} s") from None
            except TargetClosed:
                if mutating:
                    raise Uncertain(f"the connection closed before {method} answered; it may or may not have happened") from None
                raise
            if "method" in msg:
                self.on_event(msg)
            elif msg.get("id") == msg_id:
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
        self._watch()

    def _watch(self):
        self.dialog = None
        self.conn = _Connection(self.target["webSocketDebuggerUrl"], self.timeout, self._on_event)

    def _on_event(self, msg):
        if msg["method"] == "Page.javascriptDialogOpening":
            self.dialog = msg["params"]
            raise DialogOpen(self.dialog)
        if msg["method"] == "Page.javascriptDialogClosed":
            self.dialog = None

    def _attach(self):
        """Subscribe to this tab's dialogs. A tab that a dialog already paused
        before this connection existed never answers."""
        try:
            self.call("Page.enable", timeout=5, mutating=False)
        except TargetClosed:
            raise
        except CdpError:
            raise CdpError(f"the tab {self.target.get('url') or self.target['id']} does not answer; a native dialog may "
                           "be open on it. Look at the screen, answer the dialog there, then attach again") from None
        self.show()

    @property
    def id(self):
        return self.target["id"]

    def call(self, method, params=None, timeout=30, mutating=True):
        """A CDP command on this page. mutating=False only changes how a lost
        reply is reported (TargetClosed instead of Uncertain); nothing is ever
        resent. After a restart, call reconnect() or open the page again."""
        if self.conn.closed:
            raise TargetClosed("the page connection is closed; call reconnect() or open the page again, and check it before acting")
        self.conn.poll()
        if self.dialog and method != "Page.handleJavaScriptDialog":
            raise DialogOpen(self.dialog)
        return self.conn.call(method, params, timeout, mutating)

    def handle_dialog(self, accept=False, text=None):
        """Answer the open dialog: accept (OK, or leave the page) or dismiss
        (Cancel, or stay). text fills a prompt()."""
        self.conn.poll()
        dialog = self.dialog
        if not dialog:
            raise CdpError("no dialog is open on this tab")
        params = {"accept": bool(accept)}
        if text is not None:
            params["promptText"] = text
        self.call("Page.handleJavaScriptDialog", params)
        # Answering can open the next dialog before this reply arrives.
        if self.dialog is dialog:
            self.dialog = None

    def evaluate(self, expression, mutating=True, timeout=30):
        limit = min(timeout, MAX_EVALUATE)
        r = self.call("Runtime.evaluate", {"expression": expression, "returnByValue": True, "awaitPromise": True,
                                           "timeout": int(limit * 1000)}, limit + 2, mutating)
        if "exceptionDetails" in r:
            d = r["exceptionDetails"]
            raise CdpError(f"page script failed: {d.get('exception', {}).get('description', d.get('text'))}")
        return r.get("result", {}).get("value")

    def read(self, expression, timeout=30):
        """Evaluate an expression you consider side-effect free (a lost reply is
        TargetClosed or CdpError rather than Uncertain; it is not resent)."""
        return self.evaluate(expression, mutating=False, timeout=timeout)

    def text(self):
        return self.read("document.body ? document.body.innerText : ''")

    def navigate(self, url, timeout=30, leave=False):
        """Navigate and wait for that navigation's document to load (bounded).
        A page that asks to confirm leaving (it may hold unsent input) is kept:
        NavigationBlocked, unless leave=True discards it."""
        token = os.urandom(8).hex()
        try:
            self.evaluate(f"window.__elseNav = {json.dumps(token)}", timeout=5)
        except CdpError:
            pass
        self.conn.poll()
        # From here a beforeunload dialog can only come from this navigation.
        if self.dialog:
            raise DialogOpen(self.dialog)
        r = None
        try:
            r = self.call("Page.navigate", {"url": url}, timeout)
        except DialogOpen as e:
            if e.dialog.get("type") != "beforeunload":
                raise
            self.handle_dialog(leave)
            if not leave:
                raise NavigationBlocked(f"{self.target.get('url') or 'the page'} asked to confirm leaving, so it stayed open "
                                        f"with its input; pass leave=True to discard it and load {url}") from None
        except Uncertain:
            # Chromium answers Page.navigate once the response starts; a
            # server that never answers is a navigation timeout.
            if self.conn.closed:
                raise
            raise NavigationTimeout(f"loading {url} did not start within {timeout} s; the tab may still be loading") from None
        if r and r.get("errorText"):
            raise CdpError(f"navigation to {url} failed: {r['errorText']}")
        # A new document lacks the marker; a same-document navigation (no
        # loaderId) keeps its document. Leaving after a confirmation (no reply
        # kept) always loads a new one.
        fresh = f"window.__elseNav !== {json.dumps(token)} && " if r is None or r.get("loaderId") else ""
        self.wait_for(f"{fresh}document.readyState === 'complete'", timeout, f"loading {url}", NavigationTimeout)
        self.target["url"] = self.read("location.href")

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
        self.call("Page.bringToFront")

    def reconnect(self, timeout=30):
        """Reattach to this same tab (same target ID) after its connection
        dropped, or raise TargetClosed: a browser restart gives tabs new IDs, so
        open the page again with browser.page() and check it. Never repeats an
        action, never picks another tab."""
        ready(timeout)
        found = next((t for t in _get_json("/json/list") if t.get("type") == "page" and t["id"] == self.target["id"]), None)
        if not found:
            raise TargetClosed(f"the tab {self.target.get('url') or self.target['id']} is gone")
        self.target = found
        self.timeout = timeout
        self._watch()
        self._attach()

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
            conn = _Connection(version["webSocketDebuggerUrl"], self.timeout, lambda msg: None)
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
        _record(target["id"], bool(create))
        page = Page(self, target, self.timeout)
        self.pages.append(page)
        page._attach()
        if create:
            page.navigate(create, self.timeout)
        return page

    def close(self):
        for p in self.pages:
            p.close()
        _forget()
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
