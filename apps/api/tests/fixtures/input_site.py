#!/usr/bin/env python3
"""A local web fixture for else.bot's visual input tests (POC-029). Stdlib only.

/                 one button at (X, Y); clicking it reveals a text box that
                  asks for WORD; Enter submits to /done (the layout is fixed
                  at startup, so nothing about it is in any URL)
/done?word=...    a long page; at the bottom, the confirmation code CODE
/search           a library search box; /search?q=... lists matching
                  libraries with their opening hours (HOURS for Riverside)
/log              the words and queries submitted, one per line

Usage: input_site.py PORT CODE X Y WORD HOURS. Synthetic content only; binds
127.0.0.1.
"""
import html
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

PORT, CODE, X, Y, WORD, HOURS = int(sys.argv[1]), sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), sys.argv[5], sys.argv[6]
LIBRARIES = [("Riverside Library", HOURS), ("Hillcrest Library", "09:00-17:00"), ("Old Town Library", "10:00-16:00")]
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
            x, y, word = X, Y, html.escape(WORD)
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
        elif url.path == "/search":
            query = q.get("q")
            form = ("<form action='/search'><input name='q' autocomplete='off' style='font-size:24px'>"
                    "<button style='font-size:24px'>Search</button></form>")
            if query is None:
                self.page(f"<title>Library search</title><body style='font:24px sans-serif'><h1>Library search</h1>{form}</body>")
                return
            SUBMITTED.append(f"query:{query}")
            hits = [f"<li>{html.escape(n)}: open {html.escape(h)}</li>" for n, h in LIBRARIES
                    if query.lower() in n.lower()] or ["<li>No library matches.</li>"]
            self.page(f"<title>Results</title><body style='font:24px sans-serif'><h1>Results for {html.escape(query)}</h1>"
                      f"<ul>{''.join(hits)}</ul>{form}</body>")
        elif url.path == "/log":
            self.page("\n".join(SUBMITTED))
        else:
            self.send_response(404)
            self.end_headers()


ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
