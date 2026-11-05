import ctypes as C,json,os,sys
from pathlib import Path
C.CDLL('/System/Library/Frameworks/Foundation.framework/Foundation')
cg=C.CDLL('/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics')
cf=C.CDLL('/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation')
o=C.CDLL('/usr/lib/libobjc.A.dylib');dispatch=C.CDLL('/usr/lib/system/libdispatch.dylib')
P=C.c_void_p;U=C.c_uint;L=C.c_ulong;D=C.c_double
class Point(C.Structure):_fields_=[('x',D),('y',D)]
class Size(C.Structure):_fields_=[('width',D),('height',D)]
class Rect(C.Structure):_fields_=[('origin',Point),('size',Size)]
o.objc_getClass.argtypes=[C.c_char_p];o.objc_getClass.restype=P
o.sel_registerName.argtypes=[C.c_char_p];o.sel_registerName.restype=P
address=C.cast(o.objc_msgSend,P).value
def send(obj,name,ret=P,types=(),args=()):return C.CFUNCTYPE(ret,P,P,*types)(address)(obj,o.sel_registerName(name.encode()),*args)
def new(name):
 cls=o.objc_getClass(name.encode());assert cls,name
 return send(send(cls,'alloc'),'init')
pool=new('NSAutoreleasePool')
settings=new('CGVirtualDisplaySettings');descriptor=new('CGVirtualDisplayDescriptor')
send(settings,'setHiDPI:',None,(U,),(1,))
name=send(o.objc_getClass(b'NSString'),'stringWithUTF8String:',P,(C.c_char_p,),(b'PR62 isolated HiDPI',))
send(descriptor,'setName:',None,(P,),(name,))
dispatch.dispatch_get_global_queue.argtypes=[C.c_long,L];dispatch.dispatch_get_global_queue.restype=P
send(descriptor,'setQueue:',None,(P,),(dispatch.dispatch_get_global_queue(2,0),))
for selector,value in [('setMaxPixelsWide:',2880),('setMaxPixelsHigh:',1800),('setSerialNum:',620004),('setProductID:',62),('setVendorID:',0x656c)]:send(descriptor,selector,None,(U,),(value,))
send(descriptor,'setSizeInMillimeters:',None,(Size,),(Size(332.5,207.8),))
for selector,value in [('setWhitePoint:',Point(.3125,.3291)),('setRedPrimary:',Point(.6797,.3203)),('setGreenPrimary:',Point(.2559,.6983)),('setBluePrimary:',Point(.1494,.0557))]:send(descriptor,selector,None,(Point,),(value,))
display=send(send(o.objc_getClass(b'CGVirtualDisplay'),'alloc'),'initWithDescriptor:',P,(P,),(descriptor,));assert display,'initWithDescriptor returned nil'
mode=send(send(o.objc_getClass(b'CGVirtualDisplayMode'),'alloc'),'initWithWidth:height:refreshRate:',P,(U,U,D),(1440,900,60.0));assert mode
array=(P*1)(mode);modes=send(o.objc_getClass(b'NSArray'),'arrayWithObjects:count:',P,(C.POINTER(P),L),(array,1))
send(settings,'setModes:',None,(P,),(modes,));assert send(display,'applySettings:',C.c_bool,(P,),(settings,))
id=send(display,'displayID',U)
cg.CGMainDisplayID.restype=U;old=cg.CGMainDisplayID()
cg.CGDisplayCopyDisplayMode.argtypes=[U];cg.CGDisplayCopyDisplayMode.restype=P
for n in ['CGDisplayModeGetWidth','CGDisplayModeGetHeight','CGDisplayModeGetPixelWidth','CGDisplayModeGetPixelHeight']:
 getattr(cg,n).argtypes=[P];getattr(cg,n).restype=L
current=cg.CGDisplayCopyDisplayMode(id)
result={'pid':os.getpid(),'display_id':id,'previous_main_display':old,'width':cg.CGDisplayModeGetWidth(current),'height':cg.CGDisplayModeGetHeight(current),'pixel_width':cg.CGDisplayModeGetPixelWidth(current),'pixel_height':cg.CGDisplayModeGetPixelHeight(current)}
assert result['pixel_width']==2*result['width'],result
cg.CGBeginDisplayConfiguration.argtypes=[C.POINTER(P)];cg.CGBeginDisplayConfiguration.restype=C.c_int
cg.CGConfigureDisplayOrigin.argtypes=[P,U,C.c_int,C.c_int];cg.CGConfigureDisplayOrigin.restype=C.c_int
cg.CGCompleteDisplayConfiguration.argtypes=[P,C.c_int];cg.CGCompleteDisplayConfiguration.restype=C.c_int
config=P();assert cg.CGBeginDisplayConfiguration(C.byref(config))==0
assert cg.CGConfigureDisplayOrigin(config,old,1440,0)==0
assert cg.CGConfigureDisplayOrigin(config,id,0,0)==0
assert cg.CGCompleteDisplayConfiguration(config,1)==0
result['main_display']=cg.CGMainDisplayID();print(json.dumps(result),flush=True)
Path(sys.argv[1]).write_text(json.dumps(result))
cf.CFRunLoopRun()
