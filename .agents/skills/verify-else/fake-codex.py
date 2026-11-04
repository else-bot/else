#!/usr/bin/env python3
"""Offline Codex protocol fixture for native desktop journeys, never contacts a model."""
import json
import os
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


def stream(turn, hang, stopped):
    if hang:
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
        threading.Thread(target=stream,args=(turn_id, latest == '[ui:hang]', stopped),daemon=True).start()
    elif method == 'turn/interrupt':
        finish(params['turnId'], stopped, 'interrupted')
