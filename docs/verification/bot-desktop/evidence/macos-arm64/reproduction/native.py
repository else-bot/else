from guest import l,r,m
import sys,json,time
from pathlib import Path
root=Path(__file__).resolve().parent
pid=json.loads((root/'native.json').read_text())['pid']
def capture(name):
 evidence=root/'evidence/native'/name;evidence.mkdir(parents=True,exist_ok=False)
 mcp=l.Mcp(r,l.live_guest(r,m));window,state=l.snapshot(mcp,pid,'PR62 arm64 computer verification',evidence,'before')
 return mcp,window,state,evidence
if __name__=='__main__':
 name,action=sys.argv[1:3];mcp,w,s,e=capture(name)
 args=json.loads(sys.argv[3]) if len(sys.argv)>3 else {}
 if action=='snapshot':sys.exit()
 if 'label' in args:
  label=args.pop('label');found=[x for x in s['elements'] if x.get('label')==label];assert len(found)==1,[(x.get('label'),x.get('role')) for x in found];args['element_token']=found[0]['element_token']
 else:
  args.update(pid=pid,window_id=w['window_id'])
  if action=='click':args['capture_id']=s['capture_id']
 if action in ('click','type_text','press_key','scroll','drag'):args['delivery_mode']=args.pop('delivery_mode','background')
 result=mcp.call(action,args);result.pop('images',None);l.save(e/'action.json',result);print(result['text']);assert not result['is_error']
 time.sleep(1.5);l.snapshot(l.Mcp(r,l.live_guest(r,m)),pid,'PR62 arm64 computer verification',e,'after')
