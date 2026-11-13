"""Idempotent screenshot fixture. Run inside omabox: seed.py STATE_DIR.

Uses owner API for categories, bots, attachments and the live hanging turn.
Typed tool-only snapshot records are inserted transactionally with foreign keys
and all existing triggers enabled. Never restart the API after seeding pending
cards: restart reconciliation deliberately withdraws in-flight requests.
"""
import base64
import hashlib
import json
import sqlite3
import sys
import time
import urllib.request
import uuid
from pathlib import Path

state = Path(sys.argv[1])
data = state / 'data'
url = json.loads((data / 'api.json').read_text())['url']
token = (data / 'owner-token').read_text().strip()
NS = uuid.UUID('61625abe-23a9-4f49-9643-f3464ff52584')
def uid(key): return str(uuid.uuid5(NS, key))
def blob(value): return uuid.UUID(value).bytes
def digest(value): return hashlib.sha256(value.encode()).digest()
def js(value): return json.dumps(value, ensure_ascii=False)
def req(method, path, body=None):
    request = urllib.request.Request(url + '/v1' + path, method=method,
        data=None if body is None else js(body).encode(),
        headers={'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=20) as response:
        raw = response.read()
        return json.loads(raw) if raw else None

def unwrap(value, key): return value.get(key) or value
listing = req('GET', '/bots')
main = listing['default_bot_id']
categories = req('GET', '/categories')
if isinstance(categories, dict): categories = categories['categories']
cats = {c['name']: c['id'] for c in categories}
for name in ['Work', 'Personal']:
    if name not in cats:
        cats[name] = unwrap(req('POST', '/categories', {'request_id': uid('category/' + name), 'name': name}), 'category')['id']
bots = {b['name']: b['id'] for b in listing['bots']}
for name, category, shape, color in [
    ('Research','Work','sparkle','purple'), ('Code Reviewer','Work','tile','green'),
    ('Inbox','Work','cloud','amber'), ('Travel Planner','Personal','house','teal'),
    ('Fitness Coach','Personal','flower','pink'), ('Release Monitor','Work','tile','blue')]:
    if name not in bots:
        bots[name] = unwrap(req('POST', '/bots', {'request_id': uid('bot/' + name),
            'name': name, 'category_id': cats[category], 'avatar': {'shape': shape, 'color': color}}), 'bot')['id']
bots['Development Assistant'] = main

# A real uploaded file gives the attachment chip a working download target.
file = req('POST', f'/bots/{main}/attachments', {'id': uid('release-file'),
    'name': 'billing-release-notes.md', 'media_type': 'text/markdown',
    'data': base64.b64encode(b'# Billing release\n\nProration dry run, durable webhook retries, and rollout checklist.\n').decode()})

db = sqlite3.connect(data / 'else.sqlite3', timeout=20)
db.execute('PRAGMA foreign_keys=ON')
def insert(table, **values):
    columns = ','.join(values)
    db.execute(f'INSERT OR IGNORE INTO {table} ({columns}) VALUES ({",".join("?" for _ in values)})', list(values.values()))
def conversation(bot):
    return db.execute("SELECT id FROM conversations WHERE bot_id=? AND kind='owner'", (blob(bot),)).fetchone()[0]
def item(key, turn, bot, kind, text, **values):
    ident = blob(uid('item/' + key))
    if db.execute('SELECT 1 FROM items WHERE id=?', (ident,)).fetchone(): return ident
    conv = conversation(bot)
    seq = db.execute('UPDATE conversations SET last_item_seq=last_item_seq+1 WHERE id=? RETURNING last_item_seq', (conv,)).fetchone()[0]
    insert('items', id=ident, conversation_id=conv, seq=seq, turn_id=turn, kind=kind,
           text=text, provider_id='synthetic', **values)
    return ident

def turn(key, bot, prompt, status='completed', **values):
    ident = blob(uid('turn/' + key))
    if db.execute('SELECT 1 FROM turns WHERE id=?', (ident,)).fetchone(): return ident
    conv = conversation(bot)
    no = db.execute('UPDATE conversations SET last_turn_no=last_turn_no+1 WHERE id=? RETURNING last_turn_no', (conv,)).fetchone()[0]
    insert('turns', id=ident, bot_id=blob(bot), conversation_id=conv, request_id=blob(uid('request/' + key)),
        request_hash=digest(prompt), turn_no=no, owner_item_id=blob(uid('item/' + key + '/user')),
        provider_id='synthetic', status=status, attempt=1,
        started_at='2026-11-13T14:15:00+00:00',
        finished_at='2026-11-13T14:17:00+00:00' if status in ['completed','failed'] else None, **values)
    item(key + '/user', ident, bot, 'user', prompt)
    return ident

with db:
    root = turn('release-prep', main, 'Prepare Thursday\'s billing release. Ask Research and Code Reviewer to check the risks, and remember our rollout rules.')
    for name, task, result in [
        ('Research', 'Compare the proration migration with recent support reports.', 'The migration addresses all 12 rounding reports. Run it in dry-run mode before updating historical invoices.'),
        ('Code Reviewer', 'Review the retry queue and identify release blockers.', 'The queue needs durable retry state. I recommend persisting the next attempt before acknowledging each webhook.')]:
        child = turn('delegate/' + name, bots[name], task)
        item('delegate/' + name + '/reply', child, bots[name], 'assistant', result)
        did = blob(uid('delegation/' + name))
        insert('delegations', id=did, root_turn_id=root, parent_attempt=1, parent_bot_id=blob(main),
            target_bot_id=blob(bots[name]), task=task, task_hash=digest(task),
            conversation_id=conversation(bots[name]), child_turn_id=child, status='completed',
            result=result, delivered_attempt=1, finished_at='2026-11-13T14:17:00+00:00')
        # The root's status item shares its ID with the delegation, as ITEM_COLUMNS expects.
        if not db.execute('SELECT 1 FROM items WHERE id=?',(did,)).fetchone():
            conv=conversation(main)
            seq=db.execute('UPDATE conversations SET last_item_seq=last_item_seq+1 WHERE id=? RETURNING last_item_seq',(conv,)).fetchone()[0]
            insert('items',id=did,conversation_id=conv,seq=seq,turn_id=root,kind='status',text=result)
        # A delegated provider turn owns a task conversation, never the owner's chat.
        task_conv=blob(uid('task-conversation/' + name))
        turn_no=db.execute('SELECT turn_no FROM turns WHERE id=?',(child,)).fetchone()[0]
        insert('conversations',id=task_conv,bot_id=blob(bots[name]),kind='task',last_item_seq=2,last_turn_no=turn_no)
        db.execute('UPDATE turns SET conversation_id=?,request_id=? WHERE id=?',(task_conv,did,child))
        db.execute("UPDATE items SET conversation_id=?,seq=1 WHERE id=?",(task_conv,blob(uid('item/delegate/' + name + '/user'))))
        db.execute("UPDATE items SET conversation_id=?,seq=2 WHERE id=?",(task_conv,blob(uid('item/delegate/' + name + '/reply'))))
        db.execute('UPDATE delegations SET conversation_id=? WHERE id=?',(task_conv,did))
    item('created', root, main, 'status', 'Created Release Monitor', bot_created=js({
        'bot_id':bots['Release Monitor'],'name':'Release Monitor','creator_bot_id':main,
        'creator_name':'Development Assistant','purpose':'Watch billing deploys and summarize changes in error rates.'}))
    diff=[{'op':'remove','text':'Summarize deployment risks.'},
          {'op':'add','text':'Include a dry run, rollback command, and an owner for every billing migration.'}]
    rules={'version':1,'section':'Release safety','change':'added',
        'owner_words':'Always include a dry run and a rollback plan for billing changes.','diff':diff}
    insert('bot_rules',bot_id=blob(main),version=1,markdown='## Release safety\n\nInclude a dry run, rollback command, and an owner for every billing migration.\n',
        section=rules['section'],change='added',owner_words=rules['owner_words'],author='bot',diff=js(diff),turn_id=root)
    item('rules',root,main,'status','Added rule: Release safety',rules_update=js(rules))
    for key, name, cron, run_at, prompt in [
        ('digest','Morning release brief','30 8 * * 1-5',None,'Summarize overnight billing errors, open release blockers, and the next deploy.'),
        ('review','Friday reliability review','0 16 * * 5',None,'Review failed webhooks and suggest one reliability improvement.'),
        ('reminder','Review the billing dry run','once','2027-03-04T09:00:00+00:00','Remind me to review the proration dry-run report with Maya.')]:
        rid=blob(uid('routine/' + key))
        insert('routines',id=rid,bot_id=blob(main),conversation_id=conversation(main),name=name,prompt=prompt,
            cron=cron,timezone='Europe/Paris',enabled=1,next_due_at=run_at or '2027-03-01T07:30:00+00:00',
            created_turn_id=root,created_name=name,request_hash=digest(prompt),run_at=run_at)
        if key=='digest': item('routine',root,main,'status','Created routine: '+name,routine_update=js({'routine_id':str(uuid.UUID(bytes=rid)),'name':name}))
    insert('webhooks',id=blob(uid('webhook')),bot_id=blob(main),conversation_id=conversation(main),
        name='Billing deploy completed',prompt='Inspect the deploy and compare error rates with the previous hour.',
        token_digest=digest('offline-fixture-no-live-token'),min_interval_minutes=15,daily_limit=24,
        created_turn_id=root,created_name='Billing deploy completed',request_hash=digest('deploy'))
    item('prep-reply',root,main,'assistant','Research and Code Reviewer agree on two release conditions: preview the migration and persist retry state. I also created Release Monitor and scheduled your morning brief.')
    md = turn('release-plan',main,'Write the rollout plan and attach the release notes.')
    reply = """## Billing release plan

**Maya owns the migration; Sam owns webhook delivery.** Review `--dry-run` output before Thursday's deploy.

1. Preview the proration backfill and compare the total delta.
2. Save the retry queue before restarting billing workers.

| Change | Owner | Release gate |
| --- | --- | --- |
| Per-line rounding | Maya | Dry run approved |
| Durable retries | Sam | Survives a restart |
| PDF renderer | Alex | Flag stays off |

### Run and verify
```rust
let report = backfill.preview(&invoices).await?;
if args.dry_run { return Ok(report); }
report.apply().await?;
```

Watch renewal failures for 30 minutes. Pause renewals if the rate rises. **Review Thursday at 09:30.**"""
    item('release-plan/reply',md,main,'assistant',reply)
    db.execute('UPDATE items SET text=? WHERE id=?',(reply,blob(uid('item/release-plan/reply'))))
    prompt='Write the rollout plan from these notes.\n[File: billing-release-notes.md]'
    db.execute('UPDATE items SET text=?,parts=? WHERE id=?',(prompt,js([
        {'type':'text','text':'Write the rollout plan from these notes.\n'},
        {'type':'file','file':file}]),blob(uid('item/release-plan/user'))))
    # Retire an earlier fixture-only file row that displayed no standalone chip.
    db.execute('UPDATE items SET hidden=1 WHERE id=?',(blob(uid('item/release-file')),))

    qturn=turn('research-question',bots['Research'],'Help me choose the focus for our long-context reading group.','waiting')
    item('research-intro',qturn,bots['Research'],'assistant','I narrowed the shortlist to retrieval quality and long-context evaluation. Which would help the team most this month?')
    insert('owner_questions',id=blob(uid('question/focus')),bot_id=blob(bots['Research']),turn_id=qturn,attempt=1,
        kind='choice',title='What should the reading group focus on?',context='I will prepare a short brief and three papers for the direction you choose.',
        choices=js([{'id':'a','label':'Retrieval quality and chunking'},{'id':'b','label':'Evaluating long-context answers'},{'id':'c','label':'Production cost and latency'}]))
    db.execute('UPDATE owner_questions SET choices=? WHERE id=?',(js([{'id':'a','label':'Retrieval quality and chunking'},{'id':'b','label':'Evaluating long-context answers'},{'id':'c','label':'Production cost and latency'}]),blob(uid('question/focus'))))
    failed=turn('review-failure',bots['Code Reviewer'],'Check the private billing repository before the release.','failed',error_code='repository_unavailable',error_message='The billing repository could not be reached. Reconnect the GitHub account, then retry the review.')
    approval=turn('review-approval',bots['Code Reviewer'],'Can you publish the migration review for the team?','waiting')
    insert('owner_questions',id=blob(uid('question/publish')),bot_id=blob(bots['Code Reviewer']),turn_id=approval,attempt=1,
        kind='external',title='Publish the billing review to the team?',context='Post the findings and rollback checklist to #billing-releases. The message will be visible to everyone in the channel.',
        choices=js([{'id':'approve','label':'Approve once'},{'id':'deny','label':'Keep private'}]),
        action=js({'host':'Post the reviewed billing release summary to #billing-releases'}),action_hash=digest('publish-review'))
    notice=turn('inbox-notice',bots['Inbox'],'Check whether Legal has signed off on the vendor renewal.')
    insert('owner_notices',id=blob(uid('notice/legal')),bot_id=blob(bots['Inbox']),turn_id=notice,kind='blocked',
        summary='Legal needs your signature on the vendor renewal before 5 pm.',tried='I checked the renewal thread and the shared contract folder. The signature page is still missing.')
    draftturn=turn('inbox-draft',bots['Inbox'],'Draft a reply to Maya confirming Thursday\'s release review.','waiting')
    # The draft is a proactive inbox routine, so its hidden routine item leaves
    # room to show the pending notice and the complete approval card together.
    db.execute("UPDATE items SET kind='routine' WHERE id=?",(blob(uid('item/inbox-draft/user')),))
    triage=blob(uid('routine/inbox-triage'))
    insert('routines',id=triage,bot_id=blob(bots['Inbox']),conversation_id=conversation(bots['Inbox']),
        name='Morning inbox sweep',prompt='Draft replies for the upcoming release review.',
        cron='0 8 * * 1-5',timezone='Europe/Paris',enabled=1,next_due_at='2027-03-01T07:00:00+00:00',
        created_turn_id=notice,created_name='Morning inbox sweep',request_hash=digest('morning-triage'))
    insert('routine_runs',id=blob(uid('routine-run/inbox-triage')),routine_id=triage,
        due_at='2026-11-13T07:00:00+00:00',name='Morning inbox sweep',turn_id=draftturn)
    draftid=blob(uid('draft/review'))
    insert('outgoing_drafts',id=draftid,lineage_id=draftid,revision=1,bot_id=blob(bots['Inbox']),turn_id=draftturn,attempt=1,
        content=js({'channel':'email','from':'alex@northstar.example','to':['maya@northstar.example'],'cc':[], 'bcc':[],
        'subject':'Thursday billing release review','body':'Hi Maya,\nThursday at 09:30 is confirmed. Please bring the dry-run report and rollback checklist. Sam will cover webhook retries.\nThanks, Alex'}))
    db.execute("UPDATE outgoing_drafts SET content=json_set(content,'$.body',?) WHERE id=?",('Hi Maya,\nThursday at 09:30 is confirmed. Please bring the dry-run report and rollback checklist. Sam will cover webhook retries.\nThanks, Alex',draftid))
    secret=turn('travel-secret',bots['Travel Planner'],'Find a quiet hotel in Príncipe Real for March 12–15 and save my shortlist.','waiting')
    item('travel-context',secret,bots['Travel Planner'],'assistant','I have three small hotels near the gardens, all within your €180 nightly budget. Connect your travel account so I can save the shortlist privately.')
    insert('secret_requests',id=blob(uid('secret/travel')),bot_id=blob(bots['Travel Planner']),turn_id=secret,attempt=1,
        name='travel-account',reason='Save your Lisbon hotel shortlist to your private travel account.',env_names=js(['TRAVEL_API_KEY']))
    # The Needs you tray sorts by request time. Pin it so shoot.sh's sidebar
    # key paths always land on Code Reviewer, Inbox, Research, Travel Planner.
    for table, ident, at in [
        ('owner_questions', uid('question/publish'), '2026-11-13T14:20:00.000+00:00'),
        ('outgoing_drafts', uid('draft/review'), '2026-11-13T14:20:01.000+00:00'),
        ('owner_questions', uid('question/focus'), '2026-11-13T14:20:02.000+00:00'),
        ('secret_requests', uid('secret/travel'), '2026-11-13T14:20:03.000+00:00')]:
        db.execute(f'UPDATE {table} SET created_at=? WHERE id=?', (at, blob(ident)))
    if list(db.execute('PRAGMA foreign_key_check')): raise RuntimeError('foreign key check failed')
    if db.execute('PRAGMA integrity_check').fetchone()[0]!='ok': raise RuntimeError('integrity check failed')

# Keep one real provider attempt alive. No fabricated running lifecycle.
monitor=bots['Release Monitor']
conv=req('GET',f'/bots/{monitor}/conversation')
if not any(t['status'] in ['running','sending','preparing','queued'] for t in conv['turns']):
    req('POST',f'/bots/{monitor}/turns',{'request_id':uid('monitor-hang/' + str(len(conv['turns']))),'provider_id':'synthetic',
        'text':'Watch the billing canary and compare renewal failures with the previous hour. [synthetic:hang]'})
    deadline=time.monotonic()+30
    while time.monotonic()<deadline:
        conv=req('GET',f'/bots/{monitor}/conversation')
        if any(t['status']=='running' for t in conv['turns']): break
    else: raise RuntimeError('monitor did not start')
with db:
    db.execute("UPDATE items SET text=? WHERE kind='user' AND turn_id IN (SELECT id FROM turns WHERE bot_id=?)",
        ('Watch the billing canary and compare renewal failures with the previous hour.',blob(monitor)))
    db.execute("UPDATE items SET text=? WHERE kind='assistant' AND turn_id IN (SELECT id FROM turns WHERE bot_id=?)",
        ('I am checking renewal failures and webhook delivery against the pre-deploy baseline.',blob(monitor)))
# Verify every bot snapshot decodes through the real API.
for name, bot in bots.items():
    snap=req('GET',f'/bots/{bot}/conversation')
    print(name,len(snap['items']),'items',[(t['status']) for t in snap['turns'][-2:]])
(state/'screenshot-fixture.json').write_text(js({'bots':bots,'version':1})+'\n')
print('seeded',js(bots))
