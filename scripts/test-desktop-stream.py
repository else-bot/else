#!/usr/bin/env python3
"""Exercise a disposable bot's desktop stream against a real Microsandbox guest.

Run with uv run --with websockets --with pillow scripts/test-desktop-stream.py
--api-url URL --data-dir DIR --bot UUID --output DIR. The bot is stopped and
woken during this check. Use a disposable installation, never a working bot.
Native GPUI input remains a separate journey documented in the handoff.
"""
import argparse
import base64
import io
import json
import os
from pathlib import Path
import shlex
import subprocess
import time
import urllib.error
import urllib.request
import uuid

from PIL import Image
from websockets.sync.client import connect


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--api-url', required=True)
    parser.add_argument('--data-dir', type=Path, required=True)
    parser.add_argument('--bot', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--msb-bin', default='msb')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    token = (args.data_dir / 'owner-token').read_text().strip()
    base = args.api_url.rstrip('/') + '/v1/bots/' + args.bot
    env = {**os.environ, 'MSB_HOME': str(args.data_dir / 'microsandbox')}
    results = []

    def check(name, predicate):
        if not predicate:
            raise AssertionError(name)
        results.append(name)
        print(name, flush=True)

    def observed(read, predicate):
        deadline = time.monotonic() + 10
        value = read()
        while not predicate(value) and time.monotonic() < deadline:
            time.sleep(.05)
            value = read()
        return value

    def request(method, path):
        req = urllib.request.Request(base + path, method=method,
                                     headers={'Authorization': 'Bearer ' + token})
        with urllib.request.urlopen(req, timeout=90) as response:
            body = response.read()
            return json.loads(body) if body else None

    def stream(wake=False):
        url = base.replace('http://', 'ws://').replace('https://', 'wss://')
        return connect(url + '/desktop/stream?mode=watch' + ('&wake=true' if wake else ''),
                       additional_headers={'Authorization': 'Bearer ' + token},
                       proxy=None, open_timeout=90, max_size=8 * 1024 * 1024)

    def until(ws, kind, value=None):
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            data = ws.recv(timeout=max(.1, deadline-time.monotonic()))
            if isinstance(data, bytes):
                image = Image.open(io.BytesIO(data))
                check('frame is the full guest screen', image.size == (1280, 800))
                image.save(args.output / 'frame.png')
                if kind == 'frame':
                    return image
            else:
                event = json.loads(data)
                if event['type'] == 'error':
                    if kind == 'error':
                        return event
                    raise RuntimeError(event)
                if event['type'] == kind and (value is None or event.get('mode') == value):
                    return event
        raise TimeoutError(kind)

    def send(ws, kind, **fields):
        ws.send(json.dumps({'type': kind, **fields}))

    def guest(command):
        state = request('GET', '/computer')
        result = subprocess.run([args.msb_bin, 'exec', state['runtime_name'], '--', 'sh', '-c', command],
                                env=env, text=True, capture_output=True, timeout=40, check=True)
        return result.stdout.strip()

    def write_guest(path, content):
        encoded = base64.b64encode(content).decode()
        guest('printf %s ' + shlex.quote(encoded) + ' | base64 -d >' + shlex.quote(path))

    def browser(expression, create=False):
        selection = "{create:'http://127.0.0.1:8421/desktop-login.html'}" if create else "{urlPrefix:'http://127.0.0.1:8421/desktop-login.html'}"
        script = ("import {connect} from '/usr/local/lib/elsebot/cdp/cdp.mjs';"
                  "const b=await connect(); try {const p=await b.page(" + selection + ");"
                  "console.log(JSON.stringify(await p.read(" + json.dumps(expression) + ")));}"
                  "finally {await b.close()}")
        write_guest('/home/elsebot/work/viewer-probe.mjs', script.encode())
        return json.loads(guest('runuser -u elsebot -- node /home/elsebot/work/viewer-probe.mjs'))

    request('POST', '/computer/resume')
    with stream(True) as ws:
        until(ws, 'mode', 'watch')
        until(ws, 'frame')
    guest('mkdir -p /home/elsebot/work/viewer-fixture')
    write_guest('/home/elsebot/work/viewer-fixture/desktop-login.html',
                (Path(__file__).parent / 'fixtures/desktop-login.html').read_bytes())
    guest('nohup runuser -u elsebot -- python3 -m http.server 8421 --bind 127.0.0.1 '
          '--directory /home/elsebot/work/viewer-fixture >/tmp/viewer-http.log 2>&1 </dev/null &')
    guest('for attempt in $(seq 1 100); do curl -sf http://127.0.0.1:8421/desktop-login.html >/dev/null && break; sleep 0.1; done')
    browser('document.title', create=True)
    browser("document.querySelector('#user').value='';document.querySelector('#pass').value='';"
            "localStorage.clear();document.cookie='session=;Max-Age=0';true")
    button = browser("(()=>{const r=document.querySelector('#login').getBoundingClientRect();"
                     "return {x:Math.floor(screenX+r.x+r.width/2),"
                     "y:Math.floor(screenY+outerHeight-innerHeight+r.y+r.height/2)}})()")
    login_marker = '/login-observed-' + uuid.uuid4().hex
    browser("document.querySelector('#login').addEventListener('click',()=>fetch("
            + json.dumps(login_marker) + "+'?user='+encodeURIComponent(document.querySelector('#user').value)));true")
    with stream() as ws:
        until(ws, 'mode', 'watch')
        until(ws, 'frame')
        send(ws, 'pointer', x=button['x'], y=button['y'], buttons=1)
        send(ws, 'key', key=65, down=True)
        send(ws, 'key', key=65, down=False)
        send(ws, 'mode', mode='watch')
        until(ws, 'mode', 'watch')
        check('watch ignores pointer and keyboard input', browser("document.querySelector('#user').value") == '')
        check('CDP works while watching', browser('document.title') == 'Desktop login verification')
        browser("document.querySelector('#user').focus();true")
        send(ws, 'mode', mode='control')
        until(ws, 'mode', 'control')
        check('take-over holds the guest browser lock', guest('if flock -n /var/lib/elsebot/browser.lock true; then echo unlocked; else echo locked; fi') == 'locked')
        for char in 'stream-owner':
            send(ws, 'key', key=ord(char), down=True)
            send(ws, 'key', key=ord(char), down=False)
        send(ws, 'key', key=0xff09, down=True)
        send(ws, 'key', key=0xff09, down=False)
        for char in 'synthetic-password':
            send(ws, 'key', key=ord(char), down=True)
            send(ws, 'key', key=ord(char), down=False)
        send(ws, 'pointer', x=button['x'], y=button['y'], buttons=1)
        send(ws, 'pointer', x=button['x'], y=button['y'], buttons=0)
        check('guest consumes the login before hand-back', login_marker + '?user=stream-owner' in observed(
            lambda: guest('cat /tmp/viewer-http.log'),
            lambda log: login_marker + '?user=stream-owner' in log))
        send(ws, 'mode', mode='watch')
        until(ws, 'mode', 'watch')
        state = observed(lambda: browser("({user:document.querySelector('#user').value, cookie:document.cookie,session:localStorage.getItem('session')})"), lambda value: value["session"] == "stream-owner")
        check('hand-back releases the guest browser lock', guest('if flock -n /var/lib/elsebot/browser.lock true; then echo unlocked; else echo locked; fi') == 'unlocked')
        check('manual login is visible through bot CDP', state == {
            'user': 'stream-owner', 'cookie': 'session=stream-owner', 'session': 'stream-owner'})
        request('POST', '/computer/stop')
        check('owner stop ends the viewer', until(ws, 'error')['code'] == 'computer_stopped')
    check('state inspection does not wake the computer', request('GET', '/computer')['desired_state'] == 'stopped')
    with stream() as ws:
        check('preview never resumes an owner-stopped computer', until(ws, 'error')['code'] == 'computer_stopped')
    request('POST', '/computer/resume')
    with stream(True) as ws:
        until(ws, 'mode', 'watch')
        until(ws, 'frame')
    guest('nohup runuser -u elsebot -- python3 -m http.server 8421 --bind 127.0.0.1 --directory /home/elsebot/work/viewer-fixture >/tmp/viewer-http.log 2>&1 </dev/null &')
    guest('for attempt in $(seq 1 100); do curl -sf http://127.0.0.1:8421/desktop-login.html >/dev/null && break; sleep 0.1; done')
    browser('document.title', create=True)
    check('explicit wake preserves the Chromium login', browser("localStorage.getItem('session')") == 'stream-owner')
    check('explicit wake preserves the login cookie', browser('document.cookie') == 'session=stream-owner')
    (args.output / 'results.json').write_text(json.dumps({'status': 'passed', 'checks': results}, indent=2) + '\n')


if __name__ == '__main__':
    main()
