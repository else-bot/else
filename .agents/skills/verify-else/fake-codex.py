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


def outgoing_tool(name, args):
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


def draft_reply(turn, latest, packet, stopped):
    examples = ('[ui:email]', '[ui:post]', '[ui:telegram]', 'Go ahead.',
                'Please ask for a screenshot.', 'Please add a link.',
                '[ui:draft-fail]', '[ui:draft-uncertain]')
    for example in examples:
        if packet.rstrip().endswith(example):
            latest = example
    blocks = re.findall(r'<outgoing_draft ([^>]+)>(.*?)</outgoing_draft>', packet, re.S)
    current = None
    if blocks:
        attributes, content = blocks[-1]
        current = (dict(re.findall(r'(\w+)="([^"]*)"', attributes)), content.strip())
    approved = current and current[0]['status'] == 'approved'
    if not (latest in {'[ui:email]', '[ui:post]', '[ui:telegram]'} or current):
        return False
    if approved or latest in {'Go ahead.', '[ui:draft-fail]', '[ui:draft-uncertain]'}:
        draft_id = current[0]['id']
        if not approved:
            outgoing_tool('confirm_draft_approval', {'draft_id': draft_id})
        if current[0].get('account_id'):
            outgoing_tool('send_draft', {'draft_id': draft_id})
            finish(turn, stopped, 'completed')
            return True
        claimed = outgoing_tool('start_draft_send', {'draft_id': draft_id})
        if stopped.wait(3):
            return True
        outcome = {'[ui:draft-fail]': 'failed', '[ui:draft-uncertain]': 'uncertain'}.get(latest, 'sent')
        outbox = Path(os.environ['CODEX_HOME']) / 'fixture-outbox'
        outbox.mkdir(exist_ok=True)
        (outbox / (draft_id + '.json')).write_text(json.dumps(claimed) + '\n')
        outgoing_tool('report_draft_send', {'draft_id': draft_id, 'status': outcome,
                      'detail': 'Offline fixture result. No external message was sent.'})
    elif current and latest in {'Please ask for a screenshot.', 'Please add a link.'}:
        attributes, content = current
        if attributes['channel'] == 'email':
            headers, body = content.split('\n\n', 1)
            fields = dict(line.split(': ', 1) for line in headers.splitlines())
            args = {'from': fields['From'], 'to': fields['To'].split(', '),
                    'subject': fields['Subject'], 'body': body}
            for name in ['Cc', 'Bcc']:
                if name in fields:
                    args[name.lower()] = fields[name].split(', ')
            if attributes.get('account_id'):
                args['account_id'] = attributes['account_id']
            if fields.get('Reply to message'):
                args['reply_to'] = json.loads(fields['Reply to message'])
            tool = 'draft_email'
        else:
            args = {'platform': attributes['platform']}
            lines = content.splitlines()
            for name in ['Account', 'Destination']:
                if lines and lines[0].startswith(name + ': '):
                    args[name.lower()] = lines.pop(0).split(': ', 1)[1]
            args['body'] = '\n'.join(lines).lstrip('\n')
            tool = 'draft_message'
        addition = ('Could you also send us a screenshot?' if latest == 'Please ask for a screenshot.'
                    else 'Learn more: https://example.invalid/update')
        args.update(body=args['body'] + '\n\n' + addition, replaces=attributes['id'])
        outgoing_tool(tool, args)
    elif current and current[0]['status'] == 'discarded':
        pass
    elif latest == '[ui:email]':
        outgoing_tool('draft_email', {'from': 'proliquid@example.invalid', 'to': ['ji.long@example.invalid'],
            'subject': 'Your Proliquid feedback on chart candles',
            'body': "Hi Ji Long,\n\nThanks for your support and for reporting this!\n\nWhich exchange and market did you see the spikes on?\n\nOur recent candle update follows the order book and last traded price more closely. That may explain the phantom wicks. We'll get it fixed.\n\nThanks again,\nProliquid"})
    elif latest == '[ui:post]':
        outgoing_tool('draft_message', {'platform': 'x', 'account': '@proliquid',
            'body': 'We just shipped more responsive chart candles. Thanks to everyone who helped us improve them.'})
    elif latest == '[ui:telegram]':
        outgoing_tool('draft_message', {'platform': 'telegram', 'account': '@proliquid_bot',
            'destination': 'Product updates',
            'body': 'Hey everyone! The chart update is ready. Try it out and tell us what you think.'})
    else:
        return False
    finish(turn, stopped, 'completed')
    return True


def stream(turn, latest, packet, stopped):
    try:
        if draft_reply(turn, latest, packet, stopped):
            return
    except Exception as error:
        print('Outgoing draft fixture failed: ' + str(error), file=sys.stderr, flush=True)
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
