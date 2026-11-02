# Run the VM snapshot benchmark

This guide runs synthetic disk and full captures on a host with working KVM. The [experiment report](vm-snapshots.md) records the measured workloads, and the [bot VM verification](bot-vm-verification.md) adds restoration and desktop checks. Exact archive sizes vary with the image and guest processes.

Extract the [v0.7.13 runtime bundle](https://github.com/superradcompany/microsandbox/releases/tag/v0.7.13) into a directory containing `msb` and its matching `libkrunfw.so*` library. Save the Python script below as `/tmp/elsebot-snapshot-bench.py`. It uses a private home and runtime, runs one VM at a time, and stops and removes only its own VM. Snapshot artifacts and logs remain in its output directory. Each run needs at least 8 GiB of free scratch space.

Run each case separately after KVM is available. Replace the bundle path with the extracted runtime directory:

```sh
python /tmp/elsebot-snapshot-bench.py --bundle /tmp/microsandbox-v0.7.13 --case idle
python /tmp/elsebot-snapshot-bench.py --bundle /tmp/microsandbox-v0.7.13 --case compressible
python /tmp/elsebot-snapshot-bench.py --bundle /tmp/microsandbox-v0.7.13 --case random
```

The `--disk-mib` option can increase the written data up to 1024 MiB. Results distinguish a measured run from a blocked or failed one. The script does not encrypt or upload the synthetic snapshots.

The memory worker runs as a guest daemon. An active managed `exec --detach` job prevents full capture in v0.7.13. The launcher below exits its managed session after starting the worker.

```python
#!/usr/bin/env python3
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile
import time
import uuid

parser = argparse.ArgumentParser(description="Measure real Microsandbox disk and full archives.")
parser.add_argument("--bundle", type=Path, required=True)
parser.add_argument("--case", choices=["idle", "compressible", "random"], default="idle")
parser.add_argument("--image", default="python:3.12-slim")
parser.add_argument("--disk-mib", type=int, default=128)
args = parser.parse_args()
if not 0 <= args.disk_mib <= 1024:
	parser.error("--disk-mib must be between 0 and 1024")
root = Path(tempfile.mkdtemp(prefix="elsebot-snapshot-run.", dir="/tmp"))
bundle = args.bundle.resolve()
msb = bundle / "msb"
firmware = sorted(bundle.glob("libkrunfw.so*"))
if not msb.is_file() or len(firmware) != 1:
	parser.error("bundle must contain msb and one matching libkrunfw.so file")
private_home = root / "home"
private_home.mkdir()
env = {key: value for key, value in os.environ.items() if key in [
	"PATH", "SSL_CERT_FILE", "SSL_CERT_DIR", "HTTP_PROXY", "HTTPS_PROXY", "NO_PROXY",
	"http_proxy", "https_proxy", "no_proxy", "LANG", "TERM",
]}
env.update(HOME=str(private_home), XDG_CACHE_HOME=str(root / "cache"),
	MSB_HOME=str(root / "runtime"), MSB_PATH=str(msb),
	MSB_LIBKRUNFW_PATH=str(firmware[0]), MSB_BACKEND="local", NO_COLOR="1")
report = {
	"status": "running", "started_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
	"host": platform.platform(), "case": args.case, "image": args.image,
	"scope": "Synthetic Python guest, not a Chromium desktop",
	"vcpus": 1, "guest_memory_mib": 2048,
	"runtime_bytes": msb.stat().st_size + firmware[0].stat().st_size,
	"artifacts": [], "output": str(root),
}
name = "elsebot-exp-" + uuid.uuid4().hex[:12]
created = False

def command(*items, check=True, timeout=600):
	started = time.monotonic()
	result = subprocess.run([str(msb), *map(str, items)], env=env,
		stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=timeout)
	entry = dict(args=list(map(str, items)), returncode=result.returncode,
		seconds=time.monotonic() - started, stdout=result.stdout, stderr=result.stderr)
	with (root / "commands.jsonl").open("a") as log:
		log.write(json.dumps(entry) + "\n")
	if check and result.returncode:
		raise RuntimeError(result.stderr.strip() or result.stdout.strip())
	return result

guest = r'''
import hashlib, json, os, random, sys, time
from pathlib import Path
kind, disk_mib = sys.argv[1], int(sys.argv[2])
rng = random.Random(20261025)
def chunk():
	return rng.randbytes(1024 * 1024) if kind == "random" else b"A" * (1024 * 1024)
ram = bytearray()
if kind != "idle":
	for _ in range(512):
		ram.extend(chunk())
	with open("/root/elsebot-bench-data.bin", "wb") as f:
		for _ in range(disk_mib):
			f.write(chunk())
		f.flush()
		os.fsync(f.fileno())
ready = {"case": kind, "ram_bytes": len(ram),
	"ram_sha256": hashlib.sha256(ram).hexdigest(),
	"disk_bytes": 0 if kind == "idle" else disk_mib * 1024 * 1024}
Path("/root/elsebot-bench-ready.json").write_text(json.dumps(ready))
while True:
	time.sleep(1)
'''

try:
	report["version"] = command("--version").stdout.strip()
	doctor = command("doctor", check=False, timeout=30)
	report["doctor"] = dict(returncode=doctor.returncode,
		stdout=doctor.stdout, stderr=doctor.stderr)
	if doctor.returncode:
		report["status"] = "blocked"
		report["reason"] = "Microsandbox doctor failed; no VM or snapshot was created"
	else:
		if shutil.disk_usage(root).free < 8 * 1024**3:
			raise RuntimeError("Need at least 8 GiB free scratch space")
		command("create", args.image, "--name", name, "--cpus", "1",
			"--memory", "2G", "--root-disk", "4G", "--no-net",
			"--max-duration", "20m")
		created = True
		report["inspect"] = command("inspect", name).stdout
		launcher = r'''
import os, sys
source, case, disk_mib = sys.argv[1:]
if os.fork():
	raise SystemExit(0)
os.setsid()
if os.fork():
	os._exit(0)
fd = os.open("/dev/null", os.O_RDWR)
for target in (0, 1, 2):
	os.dup2(fd, target)
if fd > 2:
	os.close(fd)
os.execv(sys.executable, [sys.executable, "-c", source, case, disk_mib])
'''
		command("exec", name, "--", "python3", "-c",
			launcher, guest, args.case, str(args.disk_mib))
		probe = 'from pathlib import Path; p=Path("/root/elsebot-bench-ready.json"); print(p.read_text() if p.exists() else "{}")'
		for _ in range(120):
			ready = json.loads(command("exec", name, "--", "python3", "-c", probe).stdout)
			if ready:
				report["guest_workload"] = ready
				break
			time.sleep(0.5)
		else:
			raise RuntimeError("Guest workload did not become ready")
		for scope in ("disk", "full"):
			started = time.monotonic()
			flags = ["--full"] if scope == "full" else []
			command("snap", "create", scope, "--sandbox", name,
				"--guest-flush", "required", "--integrity", *flags)
			command("snap", "verify", name + ":" + scope)
			archive = root / (scope + ".msb")
			command("snap", "export", name + ":" + scope, "--output", archive)
			with archive.open("rb") as f:
				digest = hashlib.file_digest(f, "sha256").hexdigest()
			report["artifacts"].append(dict(scope=scope,
				bytes=archive.stat().st_size, sha256=digest,
				capture_verify_export_seconds=time.monotonic() - started,
				path=str(archive), image_included=False, encrypted=False))
		report["status"] = "measured"
except Exception as error:
	report["status"] = "failed"
	report["reason"] = str(error)
finally:
	if created:
		try:
			report["cleanup"] = {
				"stop": command("stop", name, check=False, timeout=30).returncode,
				"remove": command("rm", name, check=False, timeout=30).returncode,
			}
		except Exception as error:
			report["cleanup_error"] = str(error)
	report["finished_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
	(root / "results.json").write_text(json.dumps(report, indent=2) + "\n")
	print(json.dumps(report, indent=2))
raise SystemExit(0 if report["status"] == "measured" else 2)
```
