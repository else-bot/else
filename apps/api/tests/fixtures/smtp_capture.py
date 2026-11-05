#!/usr/bin/env python3
"""A capture-only SMTP server for else.bot's live script tests.

It runs inside the bot's guest on loopback and delivers nothing: every
accepted message is appended to a JSON lines capture file. It requires
AUTH (PLAIN or LOGIN) with one user and password, and can inject faults:

  --drop-ack ADDR   for the first message to ADDR, store it, then close the
                    connection without answering DATA (the sender cannot
                    tell whether it was delivered)
  --refuse ADDR     answer RCPT TO:<ADDR> with 550

Usage: smtp_capture.py --port P --user U --password-file F --capture FILE
       [--drop-ack ADDR] [--refuse ADDR]
Stdlib only. Synthetic data only.
"""
import argparse
import base64
import json
import socketserver
import threading
import time

LOCK = threading.Lock()
DROPPED = set()


def record(args, entry):
    with LOCK:
        with open(args.capture, "a") as f:
            f.write(json.dumps(entry) + "\n")


class Session(socketserver.StreamRequestHandler):
    def send(self, line):
        self.wfile.write((line + "\r\n").encode())
        self.wfile.flush()

    def readline(self):
        line = self.rfile.readline(65536)
        if not line:
            raise ConnectionError("closed")
        return line.decode("utf-8", "replace").rstrip("\r\n")

    def handle(self):
        args = self.server.args
        authed = False
        sender, rcpts = None, []
        self.send("220 capture.example.test ESMTP capture-only")
        try:
            while True:
                line = self.readline()
                verb = line.split(" ", 1)[0].upper()
                rest = line[len(verb):].strip()
                if verb in ("EHLO", "HELO"):
                    self.send("250-capture.example.test")
                    self.send("250-AUTH PLAIN LOGIN")
                    self.send("250 8BITMIME")
                elif verb == "AUTH":
                    parts = rest.split()
                    mech = parts[0].upper() if parts else ""
                    if mech == "PLAIN":
                        blob = parts[1] if len(parts) > 1 else None
                        if blob is None:
                            self.send("334 ")
                            blob = self.readline()
                        try:
                            _, user, password = base64.b64decode(blob).decode().split("\0")
                        except Exception:
                            user, password = "", ""
                    elif mech == "LOGIN":
                        if len(parts) > 1:
                            user = base64.b64decode(parts[1]).decode()
                        else:
                            self.send("334 VXNlcm5hbWU6")
                            user = base64.b64decode(self.readline()).decode()
                        self.send("334 UGFzc3dvcmQ6")
                        password = base64.b64decode(self.readline()).decode()
                    else:
                        self.send("504 unsupported mechanism")
                        continue
                    if user == args.user and password == args.password:
                        authed = True
                        self.send("235 authenticated")
                    else:
                        record(args, {"event": "auth_failed", "user": user, "at": time.time()})
                        self.send("535 5.7.8 authentication failed")
                elif verb == "MAIL":
                    if not authed:
                        self.send("530 5.7.0 authentication required")
                        continue
                    sender, rcpts = rest.split(":", 1)[1].strip().strip("<>").split(">")[0], []
                    self.send("250 ok")
                elif verb == "RCPT":
                    addr = rest.split(":", 1)[1].strip().split(">")[0].strip("<")
                    if addr in args.refuse:
                        self.send("550 5.1.1 mailbox unavailable")
                    else:
                        rcpts.append(addr)
                        self.send("250 ok")
                elif verb == "DATA":
                    if not rcpts:
                        self.send("554 no valid recipients")
                        continue
                    self.send("354 end with .")
                    lines = []
                    while True:
                        data = self.readline()
                        if data == ".":
                            break
                        lines.append(data[1:] if data.startswith("..") else data)
                    body = "\n".join(lines)
                    message_id = ""
                    for header in lines:
                        if header.lower().startswith("message-id:"):
                            message_id = header.split(":", 1)[1].strip()
                        if header == "":
                            break
                    record(args, {"event": "message", "from": sender, "to": rcpts,
                                  "message_id": message_id, "data": body, "at": time.time()})
                    drop = [r for r in rcpts if r in args.drop_ack and r not in DROPPED]
                    if drop:
                        DROPPED.update(drop)
                        return
                    self.send("250 ok queued")
                    sender, rcpts = None, []
                elif verb == "RSET":
                    sender, rcpts = None, []
                    self.send("250 ok")
                elif verb == "NOOP":
                    self.send("250 ok")
                elif verb == "QUIT":
                    self.send("221 bye")
                    return
                else:
                    self.send("502 command not implemented")
        except ConnectionError:
            return


class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--port", type=int, required=True)
    p.add_argument("--user", required=True)
    p.add_argument("--password-file", required=True)
    p.add_argument("--capture", required=True)
    p.add_argument("--drop-ack", action="append", default=[])
    p.add_argument("--refuse", action="append", default=[])
    args = p.parse_args()
    with open(args.password_file) as f:
        args.password = f.read().strip()
    server = Server(("127.0.0.1", args.port), Session)
    server.args = args
    server.serve_forever()


if __name__ == "__main__":
    main()
