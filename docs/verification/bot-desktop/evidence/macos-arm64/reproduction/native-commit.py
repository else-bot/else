from guest import l,r,m,g
from native import capture
from pathlib import Path
import base64,json,shlex,sys,time
root=Path(__file__).resolve().parent
step,text=sys.argv[1:3]
pid=json.loads((root/'native.json').read_text())['pid']
mcp,w,state,evidence=capture(step)
front=mcp.call('bring_to_front',{'pid':pid,'window_id':w['window_id']});front.pop('images',None);l.save(evidence/'front.json',front);assert not front['is_error'],front
payload=base64.b64encode(text.encode('utf-16-le')).decode()
code='''import ctypes,base64,json,time
c=ctypes.CDLL('/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics')
c.CGEventCreateKeyboardEvent.argtypes=[ctypes.c_void_p,ctypes.c_ushort,ctypes.c_bool];c.CGEventCreateKeyboardEvent.restype=ctypes.c_void_p
c.CGEventSetFlags.argtypes=[ctypes.c_void_p,ctypes.c_uint64]
c.CGEventKeyboardSetUnicodeString.argtypes=[ctypes.c_void_p,ctypes.c_ulong,ctypes.POINTER(ctypes.c_ushort)]
c.CGEventKeyboardGetUnicodeString.argtypes=[ctypes.c_void_p,ctypes.c_ulong,ctypes.POINTER(ctypes.c_ulong),ctypes.POINTER(ctypes.c_ushort)]
c.CGEventPost.argtypes=[ctypes.c_uint32,ctypes.c_void_p]
b=base64.b64decode(PAYLOAD);n=len(b)//2;data=(ctypes.c_ushort*n).from_buffer_copy(b)
for down in [True,False]:
 ev=c.CGEventCreateKeyboardEvent(None,0,down);c.CGEventSetFlags(ev,0);c.CGEventKeyboardSetUnicodeString(ev,n,data)
 actual=ctypes.c_ulong();out=(ctypes.c_ushort*n)();c.CGEventKeyboardGetUnicodeString(ev,n,ctypes.byref(actual),out)
 assert actual.value==n,(actual.value,n)
 c.CGEventPost(0,ev);time.sleep(.05)
print(json.dumps({'posted_utf16_units':n,'event_pairs':1}))
'''.replace('PAYLOAD',repr(payload))
encoded=base64.b64encode(code.encode()).decode()
ok,out,err=g.sh('printf %s '+shlex.quote(encoded)+' | base64 -D | '+m['guest']['python']+' -');assert ok,err
l.save(evidence/'action.json',{'kind':'CGEventKeyboardSetUnicodeString','pid':pid,'text':text,'result':json.loads(out),'target':'identity-checked owned Lume guest'})
time.sleep(3);l.snapshot(l.Mcp(r,l.live_guest(r,m)),pid,'PR62 arm64 computer verification',evidence,'after')
print(out)
