#!/usr/bin/env python3
"""Offline Codex protocol fixture for native desktop journeys, never contacts a model."""
import json
import os
import re
import urllib.request
from pathlib import Path
import sys
import threading
import tomllib
import uuid

if '--version' in sys.argv:
    print('codex-cli desktop-fixture')
    raise SystemExit(0)
if sys.argv[1:] != ['app-server']:
    raise SystemExit('fixture accepts --version or app-server only')
lock = threading.Lock()
stopped = None
thread_id = 'desktop-fixture-thread'
turn_id = None

def send(value):
    with lock:
        print(json.dumps(value), flush=True)

def event(method, params):
    send({'jsonrpc': '2.0', 'method': method, 'params': params})

def finish(turn, stopped, status):
    with lock:
        if stopped.is_set():
            return
        stopped.set()
        print(json.dumps({'jsonrpc': '2.0', 'method': 'turn/completed', 'params': {'threadId': thread_id, 'turn': {'id': turn, 'status': status}}}), flush=True)


def email_tool(name, args):
    config = tomllib.loads((Path(os.environ['CODEX_HOME']) / 'config.toml').read_text())
    server = next(iter(config['mcp_servers'].values()))
    body = {'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call',
            'params': {'name': name, 'arguments': args}}
    request = urllib.request.Request(server['url'], data=json.dumps(body).encode(),
        headers={'Content-Type': 'application/json',
                 'Authorization': 'Bearer ' + os.environ[server['bearer_token_env_var']]})
    with urllib.request.urlopen(request, timeout=30) as response:
        result = json.load(response)['result']
    if result.get('isError'):
        raise RuntimeError(result['content'][0]['text'])
    return json.loads(result['content'][0]['text'])


def email_reply(turn, latest, packet, stopped):
    for example in ('[ui:email]', 'Go ahead.', 'Please ask for a screenshot.', '[ui:email-fail]', '[ui:email-uncertain]'):
        if packet.rstrip().endswith(example):
            latest = example
    blocks = re.findall(r'<email_draft ([^>]+)>(.*?)</email_draft>', packet, re.S)
    current = None
    if blocks:
        attributes, content = blocks[-1]
        current = (dict(re.findall(r'(\w+)="([^"]*)"', attributes)), content.strip())
    approved = current and current[0]['status'] == 'approved'
    if not (latest == '[ui:email]' or current):
        return False
    if approved or latest in {'Go ahead.', '[ui:email-fail]', '[ui:email-uncertain]'}:
        draft_id = current[0]['id']
        if not approved:
            email_tool('confirm_email_approval', {'draft_id': draft_id})
        claimed = email_tool('start_email_send', {'draft_id': draft_id})
        if stopped.wait(3):
            return True
        outcome = {'[ui:email-fail]': 'failed', '[ui:email-uncertain]': 'uncertain'}.get(latest, 'sent')
        outbox = Path(os.environ['CODEX_HOME']) / 'fixture-outbox'
        outbox.mkdir(exist_ok=True)
        (outbox / (draft_id + '.json')).write_text(json.dumps(claimed) + '\n')
        email_tool('report_email_send', {'draft_id': draft_id, 'status': outcome,
                   'detail': 'Offline fixture result. No external email was sent.'})
    elif current and latest == 'Please ask for a screenshot.':
        attributes, content = current
        headers, body = content.split('\n\n', 1)
        subject = headers.split('Subject: ', 1)[1]
        email_tool('draft_email', {'from': 'proliquid@example.invalid', 'to': ['ji.long@example.invalid'],
            'subject': subject, 'body': body + '\n\nCould you also send us a screenshot?',
            'replaces': attributes['id']})
    elif current and current[0]['status'] == 'discarded':
        pass
    elif latest == '[ui:email]':
        email_tool('draft_email', {'from': 'proliquid@example.invalid', 'to': ['ji.long@example.invalid'],
            'subject': 'Your Proliquid feedback on chart candles',
            'body': "Hi Ji Long,\n\nThanks for your support and for reporting this!\n\nWhich exchange and market did you see the spikes on?\n\nOur recent candle update follows the order book and last traded price more closely. That may explain the phantom wicks. We'll get it fixed.\n\nThanks again,\nProliquid"})
    else:
        return False
    finish(turn, stopped, 'completed')
    return True


def stream(turn, latest, packet, stopped):
    try:
        if email_reply(turn, latest, packet, stopped):
            return
    except Exception as error:
        print('Email fixture failed: ' + str(error), file=sys.stderr, flush=True)
        finish(turn, stopped, 'failed')
        return
    if latest == '[ui:hang]':
        return
    for text in ['Fixture ', 'reply ', '✓ ', 'from ', 'the ', 'desktop ', 'composer.']:
        if stopped.wait(1.0):
            return
        event('item/agentMessage/delta', {'threadId':thread_id,'turnId':turn,'itemId':'fixture-message','delta':text})
    finish(turn, stopped, 'completed')

for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    method = request['method']
    params = request.get('params', {})
    result = {}
    if method == 'skills/list':
        result = {'data': []}
    elif method == 'mcpServerStatus/list':
        config = tomllib.loads((Path(os.environ['CODEX_HOME'])/'config.toml').read_text())
        result = {'data':[{'name': name,'tools': [{'name':tool} for tool in value['enabled_tools']]} for name,value in config.get('mcp_servers',{}).items()]}
    elif method == 'account/read':
        result = {'account':{'type':'chatgpt','email':'desktop-fixture@example.invalid'}}
    elif method in ['thread/start','thread/resume']:
        thread_id = params.get('threadId', thread_id)
        result = {'thread':{'id':thread_id},'model':params.get('model'),'reasoningEffort':params.get('config',{}).get('model_reasoning_effort'),'serviceTier':params.get('serviceTier')}
    elif method == 'turn/start':
        turn_id = str(uuid.uuid4())
        stopped = threading.Event()
        result = {'turn':{'id':turn_id}}
    elif method == 'turn/interrupt':
        pass
    elif method != 'initialize':
        send({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32601,'message':'unsupported fixture method '+method}})
        continue
    send({'jsonrpc':'2.0','id':request['id'],'result':result})
    if method == 'turn/start':
        text = '\n'.join(item.get('text', '') for item in params.get('input', []))
        latest = text.rsplit("Owner's latest message:\n", 1)[-1].strip()
        threading.Thread(target=stream,args=(turn_id, latest, text, stopped),daemon=True).start()
    elif method == 'turn/interrupt':
        finish(params['turnId'], stopped, 'interrupted')
