import hashlib, json, sqlite3, subprocess, sys, time, urllib.error, urllib.parse, urllib.request, uuid
from pathlib import Path
state = Path(sys.argv[1])
ready = json.loads((state / 'ready.json').read_text())
base = ready['url']
token = (state / 'data/owner-token').read_text().strip()
def api(path, body=None):
    req = urllib.request.Request(base+path, data=json.dumps(body).encode() if body is not None else None, headers={'Authorization':'Bearer '+token, 'Content-Type':'application/json'})
    with urllib.request.urlopen(req) as res: return json.load(res)
server = subprocess.Popen(['python3','-u','apps/api/tests/fixtures/mcp_oauth.py'], stdout=subprocess.PIPE, text=True)
try:
    provider = server.stdout.readline().strip()
    bot = api('/v1/bots')['bots'][0]['id']
    package = state / 'data/mcp/oauth-fixture'
    package.mkdir(parents=True, mode=0o700)
    files = {
        'manifest.json': {'id':'oauth-fixture','name':'OAuth fixture','description':'Native recovery verification','installed_by':'Development Assistant','installed_at':'2026-11-06T13:00:00Z','transport':'http'},
        'config.json': {'type':'http','url':provider+'/mcp','headers':{}},
        'SKILL.md': 'Discover tools/list before use.',
        'README.md': 'Fake MCP for native recovery checks.'
    }
    for name, content in files.items():
        path=package/name
        path.write_text(content if isinstance(content,str) else json.dumps(content))
        path.chmod(0o600)
    oauth = f'/v1/bots/{bot}/mcp/oauth-fixture/oauth/'
    start = api(oauth+'start', {'redirect_uri':'http://127.0.0.1:49177/callback'})
    class NoRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *args): return None
    try: urllib.request.build_opener(NoRedirect).open(start['authorization_url'])
    except urllib.error.HTTPError as res:
        assert res.code == 302
        callback = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(res.headers['location']).query))
    callback['request_id'] = start['request_id']
    assert api(oauth+'complete', callback)['status'] == 'connected'
    sent=api(f'/v1/bots/{bot}/turns', {'request_id':str(uuid.uuid4()), 'text':'[synthetic:hang] Finish the interrupted MCP connection.', 'provider_id':'synthetic'})
    turn=sent['turn']['id']
    db=sqlite3.connect(state/'data/else.sqlite3')
    bid, tid = uuid.UUID(bot).bytes, uuid.UUID(turn).bytes
    for _ in range(100):
        attempt,status = db.execute('SELECT attempt,status FROM turns WHERE id=?',(tid,)).fetchone()
        if status=='running': break
        time.sleep(.05)
    else: raise RuntimeError('Fixture turn did not start')
    qid=uuid.uuid4()
    consent={'id':'oauth-fixture','provider':provider,'scopes':['fixture.read']}
    encoded=json.dumps(consent,separators=(',',':')).encode()
    digest=hashlib.sha256(b'mcp_connection\0'+encoded+b'\0').digest()
    choices=[{'id':'approve','label':'Approve once'},{'id':'deny','label':'Deny'}]
    db.execute('INSERT INTO owner_questions(id,bot_id,turn_id,attempt,kind,title,context,choices,action,action_hash) VALUES(?,?,?,?,?,?,?,?,?,?)',(qid.bytes,bid,tid,attempt,'external','Connect OAuth fixture',"Recover a connection whose credentials were already saved.\nProvider: local fake MCP\nRequested scopes: fixture.read\nThe account is available to this bot only.",json.dumps(choices),json.dumps({'mcp_connection':consent}),digest))
    db.commit()
    snapshot=api(f'/v1/bots/{bot}/conversation')
    rendered=next(t for t in snapshot['turns'] if t['id']==turn)
    conversation=api('/v1/bots')['bots'][0]['conversation_id']
    with db:
        seq=db.execute('UPDATE bots SET last_event_seq=last_event_seq+1 WHERE id=? RETURNING last_event_seq',(bid,)).fetchone()[0]
        db.execute('INSERT INTO bot_events(bot_id,seq,kind,data) VALUES(?,?,?,?)',(bid,seq,'turn.updated',json.dumps({'bot_id':bot,'conversation_id':conversation,'turn':rendered})))
    for _ in range(100):
        status,answer=db.execute('SELECT status,answer FROM owner_questions WHERE id=?',(qid.bytes,)).fetchone()
        if status=='answered': break
        time.sleep(.1)
    assert (status,answer)==('answered','approve'), (status,answer)
    print(json.dumps({'result':'native card recovered and confirmed saved credentials','question':str(qid)}))
finally:
    server.terminate()
    server.wait(timeout=5)
