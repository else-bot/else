#!/usr/bin/env python3
"""Workspace Rust tests built on the host and run only inside a new isolated omabox.

`scripts/dev.py test` calls run_tests(args). The run lives in .else-dev/tests/<id>/ and its
detached worker keeps going when the calling command or agent harness goes away:
  dev_tests.py worker DIR   the detached worker (started by run_tests)
  dev_tests.py follow DIR   wait for a run in the foreground again and print its summary
  dev_tests.py cancel DIR   stop a run's worker, which removes its box
"""

from dataclasses import dataclass
from datetime import datetime, timezone
import fcntl
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = Path(__file__).resolve()
RUNS = ROOT / ".else-dev" / "tests"
TIMEOUT_SECONDS = 1200
SENTINEL = "else-dev-test-exit: "
DONE = {"passed", "failed", "interrupted", "error"}
PROVIDER_HOMES = {"CODEX_HOME", "CLAUDE_CONFIG_DIR", "GROK_HOME"}
RESULT = re.compile(r"^test result: \w+\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out")
FAILED_TEST = re.compile(r"^test (\S+) \.\.\. FAILED$")
COLD_SECONDS = {"else-api/test/delegation": 76, "else-api/test/owner_questions": 73,
                "else-api/test/outgoing_drafts": 59, "else-api/test/mcp_waits": 41,
                "else-api/test/routines": 38}


def default_jobs():
    cpus = len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count() or 1
    return max(1, min(cpus // 4, 8))


def positive_jobs(value):
    import argparse
    try:
        jobs = int(value)
        if jobs > 0:
            return jobs
    except ValueError:
        pass
    raise argparse.ArgumentTypeError("jobs must be a positive integer")


def now():
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def listed(value):
    if value is None:
        return []
    return [value] if isinstance(value, str) else list(value)


def write_atomic(path, text):
    staged = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
    with staged.open("w") as output:
        output.write(text)
    os.replace(staged, path)


def load(run_dir):
    try:
        return json.loads((run_dir / "results.json").read_text())
    except FileNotFoundError:
        return None


def rerun_args(results):
    args = []
    for binary in results.get("binaries", []):
        if binary["status"] == "passed":
            continue
        selection = ["-p", binary["package"]] + (["--test", binary["target"]] if binary["kind"] == "test" else [])
        if selection not in args:
            args.append(selection)
    tail = results["selection"]["filter"]
    scheduling = ["--serial"] if results["selection"]["serial"] else ["--jobs", str(results.get("jobs", 1))]
    return [shlex.join(a + scheduling + (["--", *tail] if tail else [])) for a in args]


def summary(results):
    selection = results["selection"]
    lines = [f"status: {results['status']}",
             f"run: {results['dir']}",
             f"revision: {results.get('revision', '?')}{' (dirty)' if results.get('dirty') else ''}",
             f"selection: packages={','.join(selection['packages']) or 'workspace'} "
             f"tests={','.join(selection['tests']) or 'all'} filter={' '.join(selection['filter']) or '-'} "
             f"jobs={results.get('jobs', 1)} test-threads={results['test_threads']}",
             f"worker: pid {results['worker']['pid']} log {results['worker']['log']}",
             f"build: {results['build']['status']}" + (f" in {results['build']['seconds']}s" if "seconds" in results["build"] else ""),
             f"box: {results.get('box', {}).get('name', '-')}"]
    for binary in results.get("binaries", []):
        counts = binary.get("counts")
        detail = f"{counts['passed']} passed, {counts['failed']} failed, {counts['ignored']} ignored" if counts else ""
        seconds = f" ({binary['seconds']}s)" if "seconds" in binary else ""
        lines.append(f"  {binary['status']:<11} {binary['key']}  {detail}{seconds}")
        lines += [f"      failed: {name}" for name in binary.get("failed_tests", [])]
        if binary["status"] not in ("pending", "passed"):
            lines.append(f"      log: {binary['log']}")
    if results.get("error"):
        lines.append(f"error: {results['error']}")
    if results.get("cleanup"):
        lines.append(f"cleanup: {json.dumps(results['cleanup'])}")
    if results["status"] in DONE and (remaining := rerun_args(results)):
        lines.append("rerun targets or packages with unfinished binaries:")
        lines += [f"  scripts/dev.py test {args}" for args in remaining]
    return "\n".join(lines) + "\n"


def save(run_dir, results):
    results["updated_at"] = now()
    write_atomic(run_dir / "results.json", json.dumps(results, indent=2) + "\n")
    write_atomic(run_dir / "summary.txt", summary(results))


def kill_group(pgid, grace=5):
    try:
        os.killpg(pgid, signal.SIGTERM)
        deadline = time.monotonic() + grace
        while time.monotonic() < deadline:
            os.killpg(pgid, 0)
            time.sleep(0.1)
        os.killpg(pgid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        pass


def owned(argv, timeout=None, **kwargs):
    child = subprocess.Popen(argv, start_new_session=True, stdin=subprocess.DEVNULL, **kwargs)
    try:
        return child.wait(timeout=timeout)
    finally:
        if child.poll() is None:
            kill_group(child.pid)
            child.wait()


def lock_held(run_dir):
    try:
        fd = os.open(run_dir / "worker.lock", os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC)
    except FileNotFoundError:
        return False
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return False
    except BlockingIOError:
        return True
    finally:
        os.close(fd)


def source_mounts(metadata):
    found = set()
    for package in metadata["packages"]:
        directory = Path(package["manifest_path"]).parent
        source = package["source"] or ""
        if directory.is_relative_to(ROOT):
            continue
        if source.startswith("registry+") or source.startswith("sparse+"):
            found.add(directory.parent)
        elif source.startswith("git+"):
            found.add(next((p for p in [directory, *directory.parents] if (p / ".cargo-ok").exists()), directory))
        else:
            found.add(directory)
    target = Path(metadata["target_directory"])
    if not target.is_relative_to(ROOT):
        found.add(target)
    return sorted(str(p) for p in found if not any(p != q and p.is_relative_to(q) for q in found))


def build_scope(selection):
    """Cargo selection args. A `-p` build resolves its own dependency features and compiles a second
    copy of crates the workspace build already has, so named test targets keep the workspace resolution."""
    tests = [a for t in selection["tests"] for a in ("--test", t)]
    if tests:
        return ["--workspace", *tests]
    return [a for p in selection["packages"] for a in ("-p", p)] or ["--workspace"]


def build(run_dir, results):
    import dev
    selection = results["selection"]
    target_dir = ROOT / ".else-dev/.build"
    argv = ["cargo", "test", *build_scope(selection),
            "--target-dir", str(target_dir), "--config", "build.incremental=false",
            "--locked", "--no-run", "--message-format=json-render-diagnostics"]
    env = dev.cargo_env()
    results["build"].update(status="running", command=argv, cargo_target_dir=str(target_dir),
                            cargo_build_jobs=env["CARGO_BUILD_JOBS"], jsonl=str(run_dir / "build.jsonl"),
                            log=str(run_dir / "build.log"))
    save(run_dir, results)
    started = time.monotonic()
    with (run_dir / "build.jsonl").open("w") as frames, (run_dir / "build.log").open("w") as log:
        code = owned(argv, cwd=ROOT, env=env, stdout=frames, stderr=log)
    results["build"]["seconds"] = round(time.monotonic() - started, 1)
    if code:
        results["build"]["status"] = "failed"
        raise RuntimeError(f"cargo test --no-run exited {code}; see {run_dir / 'build.log'}")
    results["build"]["status"] = "passed"
    results["rustc"] = subprocess.check_output(["rustc", "-vV"], cwd=ROOT, env=env, text=True)
    host = next(line.removeprefix("host: ") for line in results["rustc"].splitlines() if line.startswith("host: "))
    metadata_argv = ["cargo", "metadata", "--locked", "--offline", "--filter-platform", host,
                     "--config", f"build.target-dir={json.dumps(str(target_dir))}", "--format-version", "1"]
    results["commands"]["metadata"] = metadata_argv
    results["status"] = "metadata"
    save(run_dir, results)
    metadata = json.loads(subprocess.check_output(metadata_argv, cwd=ROOT, env=env, text=True))
    packages = {p["manifest_path"]: p for p in metadata["packages"]}
    binaries = []
    for line in (run_dir / "build.jsonl").read_text().splitlines():
        frame = json.loads(line)
        if frame.get("reason") != "compiler-artifact" or not frame.get("executable") or not frame["profile"]["test"]:
            continue
        package = packages[frame["manifest_path"]]
        kind = frame["target"]["kind"][0]
        key = f"{package['name']}/{kind}/{frame['target']['name']}"
        binaries.append({"key": key, "package": package["name"], "version": package["version"], "kind": kind,
                         "target": frame["target"]["name"], "executable": frame["executable"],
                         "cwd": str(Path(frame["manifest_path"]).parent), "status": "pending",
                         "log": str(run_dir / "logs" / (re.sub(r"[^A-Za-z0-9_.-]", "--", key) + ".log"))})
    results["binaries"] = sorted(binaries, key=lambda b: b["key"])
    return source_mounts(metadata)


def order_binaries(run_dir, results):
    timings = {}
    remaining = {b["key"] for b in results["binaries"]}
    sources = []
    for previous in sorted(RUNS.iterdir() if RUNS.is_dir() else (), reverse=True):
        if previous >= run_dir or not previous.is_dir():
            continue
        try:
            history = load(previous)
        except (OSError, ValueError):
            continue
        if not history or history["selection"]["filter"] != results["selection"]["filter"]:
            continue
        if (history["test_threads"], history["tokio_worker_threads"]) != (
                results["test_threads"], results["tokio_worker_threads"]):
            continue
        used = False
        for binary in history.get("binaries", []):
            seconds = binary.get("seconds")
            if binary["key"] in remaining and binary["status"] == "passed" and isinstance(seconds, (int, float)):
                timings[binary["key"]] = seconds
                remaining.remove(binary["key"])
                used = True
        if used:
            sources.append(str(previous))
        if not remaining:
            break
    results["timing_sources"] = sources
    results["binaries"].sort(key=lambda b: (-timings.get(b["key"], COLD_SECONDS.get(b["key"], 0)), b["key"]))


@dataclass
class RunningBinary:
    binary: dict
    child: subprocess.Popen
    started: float


def start_binary(results, binary):
    job = {"run": results["id"], "slug": Path(binary["log"]).stem, "cwd": binary["cwd"],
           "argv": [binary["executable"], *results["selection"]["filter"], "--test-threads", str(results["test_threads"])],
           "env": {"TOKIO_WORKER_THREADS": str(results["tokio_worker_threads"]), "RUST_BACKTRACE": "1",
                   "CARGO_MANIFEST_DIR": binary["cwd"], "CARGO_PKG_NAME": binary["package"],
                   "CARGO_PKG_VERSION": binary["version"]},
           "timeout": TIMEOUT_SECONDS}
    argv = ["omabox", "run", "-b", results["box"]["name"], "--", sys.executable, str(SCRIPT), "box-exec", json.dumps(job)]
    binary.update(status="running", started_at=now(), command=argv)
    started = time.monotonic()
    with Path(binary["log"]).open("w") as log:
        child = subprocess.Popen(argv, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT,
                                 stdin=subprocess.DEVNULL, start_new_session=True)
    return RunningBinary(binary, child, started)


def finish_binary(running):
    binary, code = running.binary, running.child.returncode
    binary.update(seconds=round(time.monotonic() - running.started, 1), exit_code=code, finished_at=now())
    log_path = Path(binary["log"])
    drain_deadline = time.monotonic() + 10
    while True:
        text = log_path.read_text(errors="replace").splitlines()
        exit_line = next((json.loads(l[len(SENTINEL):]) for l in reversed(text) if l.startswith(SENTINEL)), None)
        if exit_line is not None or time.monotonic() >= drain_deadline:
            break
        time.sleep(0.05)
    counts = [list(map(int, m.groups())) for l in text if (m := RESULT.match(l))]
    if counts:
        binary["counts"] = dict(zip(["passed", "failed", "ignored", "measured", "filtered_out"],
                                    [sum(c[i] for c in counts) for i in range(5)]))
    binary["failed_tests"] = [m.group(1) for l in text if (m := FAILED_TEST.match(l))]
    if exit_line is None:
        binary["status"] = "error"
        binary["error"] = f"omabox run exited {code} without the in-box exit line"
    elif exit_line["timeout"]:
        binary["status"] = "timeout"
    else:
        binary["status"] = "passed" if code == 0 and exit_line["exit"] == 0 and counts else "failed"


def run_binaries(run_dir, results):
    pending = iter(results["binaries"])
    active = []
    try:
        while True:
            while len(active) < results["jobs"]:
                binary = next(pending, None)
                if binary is None:
                    break
                active.append(start_binary(results, binary))
                save(run_dir, results)
            if not active:
                return
            for running in active[:]:
                if running.child.poll() is not None:
                    finish_binary(running)
                    active.remove(running)
                    save(run_dir, results)
                elif time.monotonic() - running.started > TIMEOUT_SECONDS + 120:
                    raise subprocess.TimeoutExpired(running.child.args, TIMEOUT_SECONDS + 120)
            time.sleep(0.05)
    finally:
        for running in active:
            try:
                os.killpg(running.child.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
        deadline = time.monotonic() + 5
        while any(r.child.poll() is None for r in active) and time.monotonic() < deadline:
            time.sleep(0.05)
        for running in active:
            kill_group(running.child.pid, grace=0)
            running.child.wait()


def worker(run_dir):
    lock = os.open(run_dir / "worker.lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        raise SystemExit(f"another worker owns {run_dir}") from None

    def interrupted(_signal, _frame):
        for number in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
            signal.signal(number, signal.SIG_IGN)
        raise KeyboardInterrupt
    for number in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(number, interrupted)

    results = json.loads((run_dir / "request.json").read_text())
    git = lambda *a: subprocess.check_output(["git", *a], cwd=ROOT, text=True).strip()
    changes = git("status", "--porcelain").splitlines()
    results.update(status="building", started_at=now(), revision=git("rev-parse", "HEAD"), dirty=bool(changes),
                   dirty_files=changes, commands={}, build={"status": "pending"}, binaries=[],
                   worker={"pid": os.getpid(), "command": [sys.executable, str(SCRIPT), "worker", str(run_dir)],
                           "log": str(run_dir / "worker.log"),
                           "cancel": f"python3 {SCRIPT.relative_to(ROOT)} cancel {run_dir}"})
    save(run_dir, results)
    (run_dir / "logs").mkdir(exist_ok=True)
    box = None
    try:
        mounts = build(run_dir, results)
        order_binaries(run_dir, results)
        results["status"] = "box"
        box = f"et-{results['id'].rsplit('-', 1)[1]}"
        up = ["omabox", "up", box, "--net", "isolated", "--no-shell", "--json",
              *[a for m in mounts for a in ("--ro-bind", m)]]
        results["commands"]["box_up"] = up
        results["box"] = {"name": box, "mounts": mounts}
        save(run_dir, results)
        subprocess.run(up, cwd=ROOT, check=True, stdin=subprocess.DEVNULL, timeout=180,
                       stdout=subprocess.DEVNULL)
        results["box"].update(name=box, home=subprocess.check_output(
            ["omabox", "path", "-b", box, "--home"], cwd=ROOT, text=True, timeout=30).strip())
        results["status"] = "running"
        run_binaries(run_dir, results)
        passed = results["binaries"] and all(b["status"] == "passed" for b in results["binaries"])
        results["status"] = "passed" if passed else "failed"
        if not results["binaries"]:
            results["error"] = "the selection built no test binaries"
    except KeyboardInterrupt:
        results["status"] = "interrupted"
    except Exception as error:
        results.update(status="error", error=f"{type(error).__name__}: {error}")
    finally:
        for number in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
            signal.signal(number, signal.SIG_IGN)
        for binary in results["binaries"]:
            if binary["status"] == "running":
                binary["status"] = "interrupted" if results["status"] == "interrupted" else "error"
        if results["build"]["status"] == "running":
            results["build"]["status"] = results["status"]
        results["cleanup"] = {"scratch": "per-binary HOME and TMPDIR removed inside the box after each binary"}
        if box:
            down = ["omabox", "down", "-b", box]
            results["commands"]["box_down"] = down
            try:
                subprocess.run(down, cwd=ROOT, check=True, timeout=90, stdin=subprocess.DEVNULL)
                results["cleanup"]["box"] = f"{box} removed"
            except (OSError, subprocess.SubprocessError) as error:
                results["cleanup"]["box"] = f"omabox down -b {box} failed: {error}"
                results["status"] = "error" if results["status"] == "passed" else results["status"]
        results["finished_at"] = now()
        save(run_dir, results)
    return 0 if results["status"] == "passed" else 1


def box_exec(job):
    if not os.environ.get("OMABOX_BOX") or os.environ.get("WAYLAND_DISPLAY") in (None, "omabox-guard"):
        raise SystemExit("box-exec runs test binaries inside omabox only")
    import dev
    scratch = Path(os.environ["XDG_RUNTIME_DIR"]) / "else-tests" / job["run"] / job["slug"]
    shutil.rmtree(scratch, ignore_errors=True)
    home, tmp = scratch / "home", scratch / "tmp"
    home.mkdir(parents=True)
    tmp.mkdir()
    env = {k: v for k, v in os.environ.items()
           if not (k.startswith(("ELSE_", "PG", "AWS_")) or k in dev.DROPPED or k in PROVIDER_HOMES)}
    path = [p for p in os.environ.get("PATH", "").split(os.pathsep) if p and "/mise/shims" not in p]
    env.update(job["env"], HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
               XDG_DATA_HOME=str(home / ".local/share"), XDG_STATE_HOME=str(home / ".local/state"),
               XDG_CACHE_HOME=str(home / ".cache"), TMPDIR=str(tmp), TMP=str(tmp), TEMP=str(tmp),
               PATH=os.pathsep.join(path))
    print(f"box {os.environ['OMABOX_BOX']}: {' '.join(job['argv'])} (cwd {job['cwd']})", flush=True)
    started = time.monotonic()
    child = subprocess.Popen(job["argv"], cwd=job["cwd"], env=env, stdin=subprocess.DEVNULL, start_new_session=True)
    timed_out = False
    try:
        code = child.wait(timeout=job["timeout"])
    except subprocess.TimeoutExpired:
        timed_out, code = True, 124
        print(f"timed out after {job['timeout']}s", flush=True)
    finally:
        kill_group(child.pid)
        child.wait()
        shutil.rmtree(scratch, ignore_errors=True)
    print(SENTINEL + json.dumps({"exit": code, "timeout": timed_out,
                                 "seconds": round(time.monotonic() - started, 1)}), flush=True)
    return code


def follow(run_dir, child=None):
    seen = {}
    while True:
        alive = child.poll() is None if child else lock_held(run_dir)
        results = load(run_dir)
        if results:
            for item in [{"key": "run", "status": results["status"]}, *results.get("binaries", [])]:
                if seen.get(item["key"]) != item["status"]:
                    seen[item["key"]] = item["status"]
                    if item["status"] != "pending":
                        print(f"{item['status']}: {item['key']}", file=sys.stderr, flush=True)
        if not alive:
            return results
        time.sleep(1)


class Detached(Exception):
    pass


def report(run_dir, results):
    if results is None:
        print(f"the worker exited before writing results; see {run_dir / 'worker.log'}", file=sys.stderr)
        return 1
    if results["status"] not in DONE:
        print(f"the worker is gone but the run says {results['status']!r}; see {run_dir / 'worker.log'}"
              + (f" and remove box {results['box']['name']} with omabox down" if results.get("box", {}).get("name") else ""),
              file=sys.stderr)
        return 1
    print(summary(results), end="")
    return 0 if results["status"] == "passed" else 1


def wait(run_dir, child=None):
    def detach(_signal, _frame):
        raise Detached
    signal.signal(signal.SIGTERM, detach)
    signal.signal(signal.SIGHUP, detach)
    try:
        return report(run_dir, follow(run_dir, child))
    except Detached:
        try:
            print(f"detached; the run continues: python3 {SCRIPT.relative_to(ROOT)} follow {run_dir}", file=sys.stderr)
        except OSError:
            pass
        return 1
    except KeyboardInterrupt:
        if child is None:
            print(f"stopped following; the run continues: python3 {SCRIPT.relative_to(ROOT)} follow {run_dir}",
                  file=sys.stderr)
            return 130
        print("cancelling: stopping the worker and its box", file=sys.stderr, flush=True)
        child.send_signal(signal.SIGTERM)
        try:
            child.wait(timeout=180)
        except KeyboardInterrupt:
            print(f"cleanup continues in the background; see {run_dir / 'results.json'}", file=sys.stderr)
            return 130
        report(run_dir, load(run_dir))
        return 130


def run_tests(args):
    """Entry for `dev.py test`: args has package, test, jobs, serial and filter."""
    if sys.platform != "linux" or not shutil.which("omabox"):
        raise SystemExit("dev.py test needs Linux and omabox: test binaries run only inside an isolated box")
    filter_args = listed(args.filter)
    if filter_args[:1] == ["--"]:
        filter_args = filter_args[1:]
    run_id = time.strftime("%Y%m%d-%H%M%S-") + uuid.uuid4().hex[:8]
    run_dir = RUNS / run_id
    run_dir.mkdir(parents=True)
    threads = 1 if args.serial else 2
    request = {"id": run_id, "dir": str(run_dir), "status": "starting", "created_at": now(),
               "selection": {"packages": listed(args.package), "tests": listed(args.test),
                             "serial": bool(args.serial), "filter": filter_args},
               "jobs": 1 if args.serial else args.jobs,
               "test_threads": threads, "tokio_worker_threads": 2, "timeout_per_binary": TIMEOUT_SECONDS,
               "build": {"status": "pending"}, "worker": {"pid": None, "log": str(run_dir / "worker.log")}}
    write_atomic(run_dir / "request.json", json.dumps(request, indent=2) + "\n")
    with (run_dir / "worker.log").open("w") as log:
        child = subprocess.Popen([sys.executable, str(SCRIPT), "worker", str(run_dir)], cwd=ROOT,
                                 stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    print(f"tests: {run_dir} (worker pid {child.pid}); progress in summary.txt and results.json\n"
          f"Ctrl-C cancels. If this command is killed the run continues; "
          f"`python3 {SCRIPT.relative_to(ROOT)} follow {run_dir}` waits for it again.", file=sys.stderr, flush=True)
    return wait(run_dir, child)


def cancel(run_dir):
    results = load(run_dir)
    pid = results and results["worker"]["pid"]
    if not pid or not lock_held(run_dir):
        print(f"no running worker owns {run_dir}", file=sys.stderr)
        return report(run_dir, results)
    cmdline = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
    if str(run_dir).encode() not in cmdline:
        raise SystemExit(f"pid {pid} is not the worker of {run_dir}")
    os.kill(pid, signal.SIGTERM)
    return wait(run_dir)


def main(argv):
    if len(argv) == 2 and argv[0] == "box-exec":
        return box_exec(json.loads(argv[1]))
    if len(argv) == 2 and argv[0] in ("worker", "follow", "cancel"):
        run_dir = Path(argv[1]).resolve()
        if not run_dir.is_relative_to(RUNS) or not (run_dir / "request.json").is_file():
            raise SystemExit(f"{run_dir} is not a run directory under {RUNS}")
        return {"worker": worker, "follow": wait, "cancel": cancel}[argv[0]](run_dir)
    raise SystemExit(__doc__)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
