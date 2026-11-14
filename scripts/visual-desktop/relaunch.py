"""Inside this sandbox only: restart the one desktop with its original environment."""
import os
import json
import sys
from pathlib import Path
import select
import signal
import subprocess

marker=Path('/home/sbx/relaunch-ready.json')
if sys.argv[1:2]==['--check']:
    try:
        info=json.loads(marker.read_text())
        ready=info['token']==sys.argv[2] and os.readlink(f"/proc/{info['pid']}/exe")==info['exe']
    except (OSError,ValueError,KeyError): ready=False
    sys.exit(0 if ready else 1)
saved=Path('/home/sbx/desktop.env')
candidates=[]
try:
    previous=json.loads(marker.read_text())
    if os.readlink(f"/proc/{previous['pid']}/exe").removesuffix(" (deleted)")==previous['exe']:
        candidates.append(str(previous['pid']))
except (OSError,ValueError,KeyError):
    pass
candidates.extend(subprocess.run(['pgrep','-x','else-desktop'],text=True,capture_output=True).stdout.split())
for pid in dict.fromkeys(candidates):
    try:
        raw=Path(f'/proc/{pid}/environ').read_bytes()
        exe=os.readlink(f'/proc/{pid}/exe').removesuffix(' (deleted)')
        if not raw: continue
        saved.write_bytes(raw+b'\0EXE='+exe.encode())
        fd=os.pidfd_open(int(pid))
        os.kill(int(pid),signal.SIGTERM)
        if not select.select([fd],[],[],10)[0]: raise RuntimeError('desktop did not stop')
        os.close(fd)
        break
    except ProcessLookupError:
        continue
parts=saved.read_bytes().split(b'\0')
env=dict(p.decode().split('=',1) for p in parts if b'=' in p)
exe=(sys.argv[2] if len(sys.argv)>2 and sys.argv[2] else env['EXE'])
env.pop('EXE')
# Reset only this disposable profile's presentation state for comparable captures.
profile=Path(env['ELSE_DESKTOP_CONFIG_FILE'])
for layout in profile.parent.glob(profile.stem+'.*.layout.json'):
    layout.write_text(json.dumps({'sidebar_width':240,'details_width':336,
        'category_order':[],'collapsed':[],'rules_seen':{}}))
marker.write_text(json.dumps({'token':sys.argv[1],'pid':os.getpid(),'exe':str(Path(exe).resolve())}))
os.chdir('/')
os.execve(exe,[exe],env)
