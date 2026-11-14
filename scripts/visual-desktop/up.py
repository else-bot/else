"""Run dev.py detached, with evidence under the harness state directory.

The mirrored scripts directory is mounted read-only into its box.
A healthy session recorded by this harness is reused and reseeded.
"""
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import sys
import time

H=Path(__file__).resolve().parent
root=Path(sys.argv[1]).resolve()
S=Path(os.environ.get('HARNESS_STATE_DIR',str(root/'.else-dev/visual-desktop'))).resolve()
S.mkdir(parents=True,exist_ok=True)
manifest_path=S/'session.json'
if manifest_path.exists():
    old=json.loads(manifest_path.read_text())
    try:
        os.kill(old['launcher_pid'],0)
        probe = """import json, sys, urllib.request
from pathlib import Path
d = Path(sys.argv[1]) / 'data'
u = json.loads((d / 'api.json').read_text())['url']
t = (d / 'owner-token').read_text().strip()
r = urllib.request.Request(u + '/v1/bots', headers={'Authorization': 'Bearer ' + t})
with urllib.request.urlopen(r, timeout=5) as response:
    json.load(response)['bots']
"""
        check=subprocess.run(['omabox','-b',old['box'],'run','--','python3','-c',probe,old['state']],
            stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
        if check.returncode==0:
            home=Path(subprocess.check_output(['omabox','-b',old['box'],'path','--home'],text=True).strip())
            shutil.copyfile(H/'seed.py',home/'seed.py')
            subprocess.run(['omabox','-b',old['box'],'run','--','python3','/home/sbx/seed.py',old['state']],check=True,stdout=sys.stderr)
            print(old['box'])
            sys.exit(0)
    except ProcessLookupError:
        pass
runtime=S/'runtime'
scripts=runtime/'scripts'
scripts.mkdir(parents=True,exist_ok=True)
for name in ['dev.py','dev_tests.py']:
    shutil.copyfile(root/'scripts'/name,scripts/name)
# /tmp is private in omabox. Mount the harness-local worker scripts explicitly.
runner=scripts/'dev.py'
source=runner.read_text()
anchor='    run_id = time.strftime('
if source.count(anchor)!=1: raise SystemExit('dev.py run layout changed; update the harness mirror adapter')
runner.write_text(source.replace(anchor,'    mounts.add(str(SCRIPT.parent))\n'+anchor,1))
api=Path(os.environ.get('API_BINARY',str(root/'.else-dev/.build/debug/else-api'))).resolve()
desktop=Path(os.environ.get('DESKTOP_BINARY',str(root/'.else-dev/.build/debug/else-desktop'))).resolve()
for binary in [api,desktop]:
    if not binary.is_file():
        raise SystemExit(f'Missing prebuilt executable: {binary}. Set API_BINARY and DESKTOP_BINARY.')
# Monitor the evidence directory, using inotify instead of a polling sleep.
import ctypes
libc=ctypes.CDLL(None,use_errno=True)
fd=libc.inotify_init1(os.O_NONBLOCK|os.O_CLOEXEC)
if fd<0: raise OSError(ctypes.get_errno(),'inotify_init1')
runs=runtime/'.else-dev/runs'
runs.mkdir(parents=True,exist_ok=True)
def watch(path):
    if libc.inotify_add_watch(fd,os.fsencode(path),0x100|0x8|0x80)<0:
        raise OSError(ctypes.get_errno(),'inotify_add_watch')
watch(runs)
before=set(runs.glob('*/manifest.json'))
with (S/'run.log').open('w') as log:
    proc=subprocess.Popen(['python3',str(scripts/'dev.py'),'run','--api-binary',str(api),
        '--desktop-binary',str(desktop)],cwd=root,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
(S/'up.pid').write_text(str(proc.pid)+'\n')
report=None
deadline=time.monotonic()+180
watched=set()
while time.monotonic()<deadline:
    if proc.poll() is not None: raise SystemExit(f'dev.py exited {proc.returncode}; read {S / "run.log"}')
    for directory in runs.iterdir():
        if directory.is_dir() and directory not in watched:
            watch(directory)
            watched.add(directory)
    for path in set(runs.glob('*/manifest.json'))-before:
        try:
            candidate=json.loads(path.read_text())
        except (json.JSONDecodeError,FileNotFoundError):
            continue
        if candidate.get('status')=='running':
            report=candidate
            break
    if report: break
    if select.select([fd],[],[],min(5,max(0,deadline-time.monotonic())))[0]:
        try: os.read(fd,65536)
        except BlockingIOError: pass
os.close(fd)
if not report: raise SystemExit(f'Timed out waiting for manifest; read {S / "run.log"}. Launcher PID {proc.pid} remains alive.')
report['launcher_pid']=proc.pid
manifest_path.write_text(json.dumps(report,indent=2)+'\n')
box=report['box']
subprocess.run(['omabox','-b',box,'mode','3840x2160,scale=2'],check=True,stdout=sys.stderr)
home=Path(subprocess.check_output(['omabox','-b',box,'path','--home'],text=True).strip())
shutil.copyfile(H/'seed.py',home/'seed.py')
subprocess.run(['omabox','-b',box,'run','--','python3','/home/sbx/seed.py',report['state']],check=True,stdout=sys.stderr)
print(box)
