#!/usr/bin/env python3
"""A local web fixture for else.bot's visual input tests (POC-029). Stdlib only.

/?x=X&y=Y&word=W  one button at (X, Y); clicking it reveals a text box that
                  asks for W; Enter submits to /done
/done?word=...    a long page; at the bottom, the confirmation code when the
                  word was right (the code is CODE, printed nowhere else)
/log              the words submitted, one per line

Usage: input_site.py PORT CODE. Synthetic content only; binds 127.0.0.1.
"""
import html
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

PORT, CODE = int(sys.argv[1]), sys.argv[2]
SUBMITTED = []


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def page(self, body):
        data = body.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        url = urlparse(self.path)
        q = {k: v[0] for k, v in parse_qs(url.query).items()}
        if url.path == "/":
            x, y, word = int(q.get("x", 100)), int(q.get("y", 100)), html.escape(q.get("word", ""))
            self.page(f"""<title>Visual task</title>
<style>body{{margin:0;font:22px sans-serif}} #go{{position:absolute;left:{x}px;top:{y}px;
font-size:26px;padding:14px 28px;background:#2563eb;color:#fff;border:0;border-radius:8px}}
#ask{{display:none;position:absolute;left:40px;top:40px}} input{{font-size:24px}}</style>
<button id="go" onclick="document.getElementById('ask').style.display='block';this.style.display='none';document.getElementById('w').focus()">Start</button>
<form id="ask" action="/done"><p>Type the word <b>{word}</b> in the box and press Enter:</p><input id="w" name="word" autocomplete="off"></form>""")
        elif url.path == "/done":
            word = q.get("word", "")
            SUBMITTED.append(word)
            spacer = "<div style='height:2600px;background:linear-gradient(#fff,#cbd5e1)'>Scroll down</div>"
            self.page(f"<title>Done</title><body style='font:24px sans-serif'><p>Thanks.</p>{spacer}"
                      f"<p id='end' style='font-size:40px'>Confirmation code: {html.escape(CODE)}</p></body>")
        elif url.path == "/log":
            self.page("\n".join(SUBMITTED))
        else:
            self.send_response(404)
            self.end_headers()


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
