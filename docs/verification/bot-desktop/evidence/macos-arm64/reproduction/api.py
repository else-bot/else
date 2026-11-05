import os, sys, subprocess, json, time, pathlib, signal, urllib.request, uuid
root=pathlib.Path(__file__).resolve().parent
repo=root.parents[1]
env={k:v for k,v in os.environ.items() if not k.startswith(('ELSE_', 'AWS_', 'PG', 'XDG_')) and k not in ('DATABASE_URL','SECRET_KEY','SECRET_KEY_FILE','OWNER_TOKEN_FILE','CODEX_HOME','CLAUDE_CONFIG_DIR')}
(root/'home').mkdir(exist_ok=True)
env.update(HOME=str(root/'home'),ELSE_DATA_DIR=str(pathlib.Path('/private/tmp/else62-arm64-fd6449')),ELSE_BOTS_DIR=str(root/'projects'),ELSE_API_BIND='127.0.0.1:64838',ELSE_CODEX_BIN=str(repo/'.agents/skills/verify-else/fake-codex.py'),ELSE_CLAUDE_BIN='/nonexistent',ELSE_MSB_BIN=str(pathlib.Path.home()/'.local/bin/msb'),ELSE_VM_IMAGE='else-guest:3',ELSE_VM_DESKTOP='1',ELSE_SYNTHETIC_PROVIDER='1',ELSE_BACKUP_INTERVAL_SECS='0',ELSE_VM_IDLE_SECS='900',TOKIO_WORKER_THREADS='2')
binary=repo/'.else-dev/.build/debug/else-api'
def request(method,path,body=None):
 state=json.loads((pathlib.Path('/private/tmp/else62-arm64-fd6449/api.json')).read_text()); token=(pathlib.Path('/private/tmp/else62-arm64-fd6449/owner-token')).read_text().strip()
 req=urllib.request.Request(state['url']+path,method=method,headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'},data=None if body is None else json.dumps(body).encode())
 with urllib.request.urlopen(req,timeout=120) as res:
  raw=res.read(); value=json.loads(raw) if raw else None
 with (root/'evidence/http.jsonl').open('a') as log: log.write(json.dumps({'method':method,'path':path,'body':body,'response':value})+'\n')
 return value
if __name__=='__main__':
 if sys.argv[1]=='serve':
  if not (pathlib.Path('/private/tmp/else62-arm64-fd6449/owner-token')).exists(): subprocess.run([binary,'setup'],cwd='/',env=env,check=True)
  child=subprocess.Popen([binary,'serve'],cwd='/',env=env,start_new_session=True)
  (root/'api.pid').write_text(str(child.pid))
  try: child.wait()
  finally:
   if child.poll() is None: os.killpg(child.pid,signal.SIGTERM);child.wait()
 elif sys.argv[1]=='request': print(json.dumps(request(sys.argv[2],sys.argv[3],json.loads(sys.argv[4]) if len(sys.argv)>4 else None),indent=2))
