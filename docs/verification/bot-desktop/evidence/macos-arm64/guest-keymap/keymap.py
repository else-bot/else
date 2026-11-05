import ctypes,sys
x=ctypes.CDLL('libX11.so.6');x.XOpenDisplay.restype=ctypes.c_void_p
d=x.XOpenDisplay(b':99');lo,hi=ctypes.c_int(),ctypes.c_int();x.XDisplayKeycodes(ctypes.c_void_p(d),ctypes.byref(lo),ctypes.byref(hi))
n=ctypes.c_int();x.XGetKeyboardMapping.restype=ctypes.POINTER(ctypes.c_ulong)
m=x.XGetKeyboardMapping(ctypes.c_void_p(d),lo.value,hi.value-lo.value+1,ctypes.byref(n))
out=[]
for kc in range(lo.value,hi.value+1):
 syms=[m[(kc-lo.value)*n.value+i] for i in range(n.value)]
 s=syms[0]
 if s>=0xa0 and (s<0x100 or s>0x1000000): out.append(f'{kc}:{s:#x}')
print(' '.join(out))
