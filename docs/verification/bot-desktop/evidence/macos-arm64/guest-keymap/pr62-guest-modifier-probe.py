import sys,time,json
sys.path.insert(0,'/guest/x11vnc')
import test_keymap_settle as m
for cls in (m.KeymapSettle,m.KeymapSettleXkb,m.KeymapSettleNoXkb):
 cls.setUpClass()
 try:
  r=m.Rfb();r.keys([(0xFFE1,1)])
  time.sleep(.3)
  print(json.dumps({'extra':cls.extra,'before_close':m.held_input()}),flush=True)
  r.ws.sock.close()
  assert m.wait_until(m.lock_free,7)
  print(json.dumps({'extra':cls.extra,'after_close':m.held_input()}),flush=True)
  r=m.Rfb();r.type('a');time.sleep(.5)
  print(json.dumps({'extra':cls.extra,'after_reconnect_a':m.held_input(),'events':cls.events}),flush=True)
  r.close();m.wait_until(m.lock_free,7)
 finally: cls.tearDownClass()
