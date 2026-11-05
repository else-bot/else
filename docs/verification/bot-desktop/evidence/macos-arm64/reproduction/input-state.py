from vm import guest,write
code='''import ctypes,json
x=ctypes.CDLL('libX11.so.6');x.XOpenDisplay.argtypes=[ctypes.c_char_p];x.XOpenDisplay.restype=ctypes.c_void_p
x.XDefaultRootWindow.argtypes=[ctypes.c_void_p];x.XDefaultRootWindow.restype=ctypes.c_ulong
x.XQueryPointer.argtypes=[ctypes.c_void_p,ctypes.c_ulong]+[ctypes.c_void_p]*7
x.XQueryKeymap.argtypes=[ctypes.c_void_p,ctypes.c_char_p]
d=x.XOpenDisplay(b':99');root=x.XDefaultRootWindow(d)
u=[ctypes.c_ulong() for _ in range(2)];i=[ctypes.c_int() for _ in range(4)];mask=ctypes.c_uint()
x.XQueryPointer(d,root,*[ctypes.byref(v) for v in u+i],ctypes.byref(mask))
keys=ctypes.create_string_buffer(32);x.XQueryKeymap(d,keys)
print(json.dumps({'x':i[0].value,'y':i[1].value,'mask':mask.value,'held_keycodes':[j*8+k for j,v in enumerate(keys.raw) for k in range(8) if v&(1<<k)]}))
'''
write('/home/elsebot/work/input-state.py',code)
print(guest('python3 /home/elsebot/work/input-state.py'))
