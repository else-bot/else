import os,socket,sys,tempfile,time,json,multiprocessing,threading,signal,fcntl
sys.path.insert(0,'/guest/vnc')
import else_vnc_targets as v
v.ACKS=tempfile.mkdtemp(prefix='ack-')
server=socket.create_server(('127.0.0.1',0));port=server.getsockname()[1]
old_gone=multiprocessing.Event();old_ready=multiprocessing.Queue()
new_active=threading.Event();new_closed=threading.Event();cleanup_new=threading.Event()
old_touch=threading.Event()
old_port=[]
def peer():
 c,addr=server.accept();old_port.append(addr[1]);c.sendall(b'old');c.recv(1);c.close();old_gone.set()
 old_touch.wait(10)
 open(os.path.join(v.ACKS,str(addr[1])),'w').close()
 c,addr=server.accept();c.sendall(b'new');new_active.set()
 while c.recv(65536):pass
 c.close();new_closed.set();time.sleep(1);cleanup_new.set()
 open(os.path.join(v.ACKS,str(addr[1])),'w').close()
threading.Thread(target=peer,daemon=True).start()
def old():
 t=socket.create_connection(('127.0.0.1',port));old_ready.put(t.getsockname()[1]);assert t.recv(3)==b'old';p=v.ack_path(t);v.discard_stale_ack(p);t.sendall(b'x');assert t.recv(1)==b'';v.wait_for_ack(t,p)
p=multiprocessing.Process(target=old);p.start();source_port=old_ready.get(timeout=3);assert old_gone.wait(3);p.kill();p.join(3)
t=socket.socket();t.bind(('127.0.0.1',source_port));t.connect(('127.0.0.1',port));path=v.ack_path(t);v.discard_stale_ack(path)
old_touch.set();assert t.recv(3)==b'new';assert new_active.wait(2)
start=time.monotonic();v.wait_for_ack(t,path)
result={'same_source_port':source_port,'new_wait_returned_ms':round((time.monotonic()-start)*1000,2),'new_peer_closed':new_closed.is_set(),'new_cleanup_done_at_return':cleanup_new.is_set(),'ack_belongs_to_old_connection':True}
print(json.dumps(result),flush=True)
assert not cleanup_new.is_set(),result
t.close()
