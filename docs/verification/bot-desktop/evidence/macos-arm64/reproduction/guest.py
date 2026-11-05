import sys, importlib.util, pathlib, json
root=pathlib.Path(__file__).resolve().parent
source=root.parents[1]/'.agents/skills/verify-else/lume.py'
spec=importlib.util.spec_from_file_location('lume_helper',source);l=importlib.util.module_from_spec(spec);spec.loader.exec_module(l)
r=l.Run(root/'lume-4');m=r.read()
g=l.Guest(r,l.live_guest(r,m))
if __name__=='__main__':
 if sys.argv[1]=='sh':
  ok,out,err=g.sh(sys.stdin.read(),timeout=180);print(out);print(err,file=sys.stderr);sys.exit(0 if ok else 1)
 elif sys.argv[1]=='tools':
  mcp=l.Mcp(r,l.live_guest(r,m));print(json.dumps(mcp.rpc('tools/list',{}),indent=2))
