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
project = None
VISUAL = """<style>
.kpis { display: flex; flex-wrap: wrap; gap: 8px; }
.kpi { flex: 1 1 140px; min-width: 0; padding: 12px 14px; background: var(--card); border: 1px solid var(--border); border-radius: var(--radius); }
.label { color: var(--muted-foreground); font-size: 12px; }
.value { font-size: 22px; font-weight: 650; font-variant-numeric: tabular-nums; }
.down { color: var(--danger); font-size: 12px; } .flat { color: var(--muted-foreground); font-size: 12px; }
.section { margin-top: 12px; padding: 14px; background: var(--card); border: 1px solid var(--border); border-radius: var(--radius); }
.row { display: flex; align-items: center; gap: 10px; margin-top: 8px; font-size: 12px; }
.name { width: 110px; color: var(--muted-foreground); }
.track { flex: 1; height: 10px; background: var(--muted); border-radius: 5px; overflow: hidden; }
.bar { height: 10px; border-radius: 5px; }
</style>
<div>
<div class="kpis">
<div class="kpi"><div class="label">Revenue 24h</div><div class="value">$70.37</div><div class="flat">7d avg $72.39</div></div>
<div class="kpi"><div class="label">Signups 24h</div><div class="value">2</div><div class="down">-74% vs 7d avg</div></div>
<div class="kpi"><div class="label">Landing visitors</div><div class="value">139</div><div class="flat">Nov 8: 140</div></div>
</div>
<div class="section">
<div class="row"><span class="name">Organic search</span><div class="track"><div class="bar" style="width: 59%; background: var(--chart-1)"></div></div><span>44</span></div>
<div class="row"><span class="name">Paid (X Ads)</span><div class="track"><div class="bar" style="width: 41%; background: var(--chart-2)"></div></div><span>30</span></div>
<svg width="100%" height="48" viewBox="0 0 300 48" preserveAspectRatio="none" style="display: block; margin-top: 10px"><polyline fill="none" stroke="var(--chart-3)" stroke-width="2" points="0,36 50,26 100,30 150,14 200,18 250,6 300,22"/></svg>
</div>
</div>
"""

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


def handoff(turn, packet, stopped):
    found = re.search(r'\[ui:(delegate|council)\]((?:\s+[0-9a-f-]{36})+)\s*$', packet)
    if not found:
        return False
    targets = found.group(2).split()
    if found.group(1) == 'delegate':
        result = outgoing_tool('delegate', {'target_bot_id': targets[0],
            'task': 'Find three coworking spaces in Lisbon under 300 EUR a month with fast Wi-Fi. [ui:slow]'})
    else:
        result = outgoing_tool('council', {'target_bot_ids': targets,
            'task': 'Should the studio plan move from 29 to 39 EUR a month? [ui:slow]'})
    if result['status'] != 'pending':
        return False
    event('item/agentMessage/delta', {'threadId': thread_id, 'turnId': turn,
          'itemId': 'fixture-handoff', 'delta': 'Handing this off.'})
    finish(turn, stopped, 'completed')
    return True


def stream(turn, latest, packet, stopped):
    try:
        if handoff(turn, packet, stopped):
            return
        if latest == '[ui:handoff]':
            outgoing_tool('computer_handoff', {'reason': 'X Chat is blank at x.com/messages.'})
            event('item/agentMessage/delta', {'threadId': thread_id, 'turnId': turn,
                  'itemId': 'fixture-computer-handoff',
                  'delta': 'Please take over my computer to inspect the blank X Chat page.'})
            finish(turn, stopped, 'completed')
            return
        if packet.rstrip().endswith('[ui:permission]') and '[else.bot owner answers]' not in packet:
            result = outgoing_tool('ask_permission', {
                'kind': 'destructive',
                'action': "ssh omarchy-m1 'sudo tailscale serve --bg --https=443 http://127.0.0.1:3773'",
                'reason': 'Enable Tailscale HTTPS on omarchy-m1',
                'task_scope': {'task': 'Manage Tailscale', 'target': 'SSH host omarchy-m1'},
            })
            if result['status'] == 'pending':
                finish(turn, stopped, 'completed')
                return
        question_modes = {'[ui:question]': False, '[ui:single]': False, '[ui:multiple]': True}
        marker = next((marker for marker in question_modes if packet.rstrip().endswith(marker)), None)
        if marker and '[else.bot owner answers]' not in packet:
            multiple = question_modes[marker]
            args = {
                'question': 'Which inbox tasks should I take care of?' if multiple else
                    'Should I automatically archive cold recruiter emails in your personal Gmail?',
                'choices': ['Archive cold recruiter emails', 'Unsubscribe from newsletters',
                    'Label receipts and invoices', 'Draft replies to starred emails',
                    'Summarize unread threads'] if multiple else ['Yes, archive them', 'No, keep them visible'],
                'multiple': multiple,
            }
            if marker == '[ui:single]':
                args.update(question='What should I prepare for the changelog?', choices=[
                    'Skip posting', 'Prepare a Discord + Telegram draft',
                    'Prepare a Discord + Telegram + X draft', 'Edit the draft first',
                    'Something else'])
            outgoing_tool('ask_owner', args)
            finish(turn, stopped, 'completed')
            return
        rules = {
            '[ui:rule]': ('Start a thread for each new issue.',
                'When: a new issue lands in Linear project ELSE\n'
                'Do: start a T3 Code thread in `else` that describes the issue'),
            '[ui:rule-change]': ('Just send fix issue ELSE-142, nothing else.',
                'When: a new issue lands in Linear project ELSE\n'
                'Do: start a T3 Code thread in `else` with exactly `fix issue {ISSUE_ID}`\n'
                'Never: add words, a title, or context to that message.'),
        }
        rule = next((marker for marker in rules if packet.rstrip().endswith(marker)), None)
        if rule:
            words, content = rules[rule]
            outgoing_tool('rules_update', {'section': 'New Linear issue', 'content': content,
                                           'owner_words': words})
        if draft_reply(turn, latest, packet, stopped):
            return
    except Exception as error:
        print('Outgoing draft fixture failed: ' + str(error), file=sys.stderr, flush=True)
        event('item/agentMessage/delta', {'threadId': thread_id, 'turnId': turn,
              'itemId': 'fixture-error', 'delta': 'Fixture tool call failed: ' + str(error)})
        finish(turn, stopped, 'failed')
        return
    if latest == '[ui:hang]':
        return
    if latest == '[ui:visual]':
        viz = Path(project) / 'viz'
        viz.mkdir(exist_ok=True)
        (viz / 'digest.html').write_text(VISUAL)
        preview = outgoing_tool('visual_preview', {'path': 'viz/digest.html'})
        reply = (f"Growth digest, previewed at {preview['content_height']}px.\n\n"
                 "![Growth digest](viz/digest.html)\n\nSignups are down; revenue is flat.")
        event('item/agentMessage/delta', {'threadId': thread_id, 'turnId': turn,
              'itemId': 'fixture-visual', 'delta': reply})
        finish(turn, stopped, 'completed')
        return
    pause = 4.0 if '[ui:slow]' in packet else 1.0
    for text in ['Fixture ', 'reply ', '✓ ', 'from ', 'the ', 'desktop ', 'composer.']:
        if stopped.wait(pause):
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
        project = params.get('cwd', project)
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
