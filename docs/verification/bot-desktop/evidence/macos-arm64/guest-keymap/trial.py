# usage: trial.py TEXT KEYDELAY SETTLE [restart]
import asyncio,json,sys,time,os
sys.path.insert(0,os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from pathlib import Path
import websockets
from vm import browser,guest,write
write('/tmp/keymap.py',open(os.path.join(os.path.dirname(os.path.abspath(__file__)),'keymap.py')).read())
BOT='3d54093f-f74a-4616-a9ff-e9b1bc235664'
text,delay,settle=sys.argv[1],float(sys.argv[2]),float(sys.argv[3])
restart=len(sys.argv)>4 and sys.argv[4]
def added():
 return ' '.join(t for t in guest('python3 /tmp/keymap.py').split() if not t.split(':')[1].startswith('0x1008'))
if restart:
 guest('supervisorctl -c /etc/elsebot/supervisord.conf restart x11vnc >/dev/null'); time.sleep(2)
pre=added()
browser("user.value='';window.keyEvents=[];user.onkeydown=e=>keyEvents.push([e.key,e.code,e.keyCode]);user.focus();true")
async def main():
 token=Path('/private/tmp/else62-arm64-fd6449/owner-token').read_text().strip()
 async with websockets.connect(f'ws://127.0.0.1:64838/v1/bots/{BOT}/desktop/stream?mode=control',additional_headers={'Authorization':'Bearer '+token}) as ws:
  while True:
   x=await ws.recv()
   if isinstance(x,str) and json.loads(x).get('mode')=='control':break
  pw=float(os.environ.get('PREMAP','0'))
  if pw:
   for c in text:
    if ord(c)>0x7e and c!='|':
     await ws.send(json.dumps({'type':'key','key':ord(c) if ord(c)<=255 else 0x1000000|ord(c),'down':False}))
   await asyncio.sleep(pw)
  for c in text:
   if c=='|':
    await asyncio.sleep(2);continue
   key=ord(c) if ord(c)<=255 else 0x1000000|ord(c)
   inl=os.environ.get('INLINE')
   if inl is not None and ord(c)>0x7e:
    await ws.send(json.dumps({'type':'key','key':key,'down':False}))
    if float(inl): await asyncio.sleep(float(inl))
   for down in [True,False]:await ws.send(json.dumps({'type':'key','key':key,'down':down}))
   if delay: await asyncio.sleep(delay)
  await asyncio.sleep(settle)
  await ws.send(json.dumps({'type':'mode','mode':'watch'}))
  while True:
   x=await ws.recv()
   if isinstance(x,str) and json.loads(x).get('mode')=='watch':break
asyncio.run(main())
time.sleep(settle)
r=browser('({value:user.value,events:keyEvents})')
print(json.dumps({'text':text,'delay':delay,'premap':os.environ.get('PREMAP','0'),'inline':os.environ.get('INLINE'),'restart':bool(restart),'pre_added':pre,'post_added':added(),'value':r['value'],'ok':r['value']==text.replace('|',''),'events':r['events']},ensure_ascii=False))
