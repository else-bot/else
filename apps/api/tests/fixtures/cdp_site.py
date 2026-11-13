#!/usr/bin/env python3
"""A local web fixture for else.bot's CDP tests. Stdlib only.

/          a search form (input name=q, submit button)
/search    results for ?q= (title "Results for <q>", #results)
/slow      never finishes loading within a minute
/draft     a textarea whose page asks to confirm leaving once it holds text
/order     a page whose button POSTs to /count
/controls  a React-style email field (state follows only real input), an
           icon-only Like button, a Rename button that relabels the Remove
           button, and a button under an overlay
/count     POST: counts one order at once, then withholds the reply for
           30 s (so a test can cut the connection after the side effect
           and before its acknowledgment); GET: the count

Usage: cdp_site.py PORT. Synthetic content only; binds 127.0.0.1.
"""
import html
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

COUNT = 0
LOCK = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def page(self, body, status=200):
        data = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        url = urlparse(self.path)
        if url.path == "/":
            self.page("<title>Fixture search</title><form action='/search'>"
                      "<input name='q'><button type='submit'>Search</button></form>")
        elif url.path == "/search":
            q = html.escape(parse_qs(url.query).get("q", [""])[0])
            self.page(f"<title>Results for {q}</title><div id='results'>3 results for {q}: "
                      f"{q}-alpha, {q}-beta, {q}-gamma</div>")
        elif url.path == "/slow":
            time.sleep(60)
            self.page("<title>Finally</title>")
        elif url.path == "/draft":
            self.page("<title>Draft</title><textarea id='draft'></textarea><script>"
                      "addEventListener('beforeunload', e => { if (document.getElementById('draft').value) "
                      "{ e.preventDefault(); e.returnValue = ''; } })</script>")
        elif url.path == "/order":
            self.page("<title>Order</title><button id='buy'>Buy</button>")
        elif url.path == "/controls":
            self.page("<title>Controls</title><input id='email' aria-label='Email'><div id='state'>state:</div>"
                      "<button aria-label='Like' onclick=\"document.getElementById('likes').textContent='liked'\">&#9829;</button>"
                      "<div id='likes'></div><button onclick=\"document.getElementById('remove').textContent='Keep'\">Rename</button>"
                      "<button id='remove' onclick=\"document.getElementById('removed').textContent='removed'\">Remove</button>"
                      "<div id='removed'></div><div style='position:relative;width:120px'><button id='covered'>Hidden pay</button>"
                      "<div style='position:absolute;inset:0;z-index:1;background:#0006'></div></div><script>"
                      "const e = document.getElementById('email'); let tracked = '';"
                      "const d = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value');"
                      "Object.defineProperty(e, 'value', { get() { return d.get.call(this) },"
                      " set(v) { tracked = String(v); d.set.call(this, v) } });"
                      "e.addEventListener('input', () => { if (e.value !== tracked) { tracked = e.value;"
                      " document.getElementById('state').textContent = 'state:' + e.value } });</script>")
        elif url.path == "/count":
            self.page(str(COUNT))
        else:
            self.page("not found", 404)

    def do_POST(self):
        global COUNT
        if urlparse(self.path).path == "/count":
            with LOCK:
                COUNT += 1
                n = COUNT
            time.sleep(30)
            self.page(str(n))
        else:
            self.page("not found", 404)


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
