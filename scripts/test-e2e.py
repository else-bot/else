#!/usr/bin/env python3
"""Native arm64 application and installer E2E lanes, with reusable build caches.

Examples:
  scripts/test-e2e.py macos-arm64 ubuntu-arm64 --repeat 2
  scripts/test-e2e.py ubuntu-arm64 --suite application
  scripts/test-e2e.py macos-arm64 --profile release
"""

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import queue
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parent.parent
VARIANTS = ["macos-arm64", "ubuntu-arm64"]
MARKER = "ELSE_E2E_RESULT="


def capture(command, **kwargs):
    return subprocess.check_output(command, text=True, timeout=60, **kwargs).strip()


def run(command, cancel=None, **kwargs):
    """Own only this command's process group; clean it up on cancellation."""
    with subprocess.Popen(command, start_new_session=True, **kwargs) as child:
        try:
            while True:
                if cancel is not None and not cancel.empty():
                    raise InterruptedError("E2E run cancelled")
                try:
                    code = child.wait(timeout=0.2)
                    break
                except subprocess.TimeoutExpired:
                    pass
        except BaseException:
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
            raise
        if code:
            raise subprocess.CalledProcessError(code, command)


def snapshot(destination):
    """Include working-tree edits, never ignored .env files or build caches."""
    destination.mkdir()
    if (ROOT / ".git").exists():
        names = capture(["git", "ls-files", "-c", "-o", "--exclude-standard", "-z"], cwd=ROOT).split("\0")
        paths = [ROOT / name for name in names if name]
    else:
        # Container input has already been filtered by the host.
        paths = [p for p in ROOT.rglob("*") if p.is_file() or p.is_symlink()]
    for source in paths:
        if not source.exists() and not source.is_symlink():
            continue
        dest = destination / source.relative_to(ROOT)
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, dest, follow_symlinks=False)


def releases(work):
    tree = work / "tree"
    snapshot(tree)
    git_env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull)

    def git(*args):
        run(["git", "-c", "user.name=e2e", "-c", "user.email=e2e@example.invalid",
             "-c", "commit.gpgsign=false", "-c", "tag.gpgsign=false", *args], cwd=tree, env=git_env)

    git("init", "-q", "-b", "main")
    for version in ["0.0.1", "0.0.2"]:
        manifest = tree / "Cargo.toml"
        manifest.write_text(re.sub(r'(?m)^version = "[^"]+"$', f'version = "{version}"', manifest.read_text(), count=1))
        lock = tree / "Cargo.lock"
        contents, count = re.subn(r'(?m)^(name = "else-(?:api|desktop)"\nversion = )"[^"]+"',
                                  lambda m: m[1] + f'"{version}"', lock.read_text())
        if count != 2:
            raise RuntimeError("expected both workspace packages in Cargo.lock")
        lock.write_text(contents)
        if version == "0.0.2":
            migrations = tree / "apps/api/migrations"
            number = max(int(p.name.split("_")[0]) for p in migrations.glob("[0-9]*.sql")) + 1
            (migrations / f"{number:04d}_test_release.sql").write_text("-- E2E migration exercises backup-before-update.\nSELECT 1;\n")
        git("add", "-A")
        git("commit", "-q", "-m", f"E2E release {version}")
        git("tag", f"v{version}")
    repo = work / "repo.git"
    run(["git", "clone", "-q", "--bare", str(tree), str(repo)])
    return repo


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def application_targets(compile_live):
    metadata = json.loads(capture(["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"], cwd=ROOT))
    names = sorted({target["name"] for package in metadata["packages"]
                    for target in package["targets"] if "test" in target["kind"]})
    selected = [name for name in names if compile_live or not name.endswith("_live")]
    flags = ["--lib", "--bins"]
    for name in selected:
        flags += ["--test", name]
    return flags, selected, sorted(set(names) - set(selected))


def application_tests(cargo, selected, args, env, work, result):
    # Cargo runs integration binaries serially. Run independent binaries in
    # bounded lanes; each binary retains its own in-process fixture locks.
    jobs = [("unit", ["--lib", "--bins"])] + [(name, ["--test", name]) for name in selected]
    results = {}
    mailboxes = []

    def execute(name, flags, cancel):
        start = time.perf_counter()
        log = work / f"test-{name}.log"
        status = "passed"
        with log.open("w") as stream:
            try:
                test_args = ["--test-threads", str(args.test_threads)]
                # S3 tests are optional for ordinary Cargo runs, mandatory here.
                if name == "replication":
                    test_args += ["--ignored"]
                run(cargo + flags + ["--", *test_args],
                    cancel=cancel, cwd=ROOT, env=env, stdout=stream, stderr=subprocess.STDOUT)
            except Exception as error:
                status = "failed"
                stream.write(str(error) + "\n")
        return {"status": status, "seconds": round(time.perf_counter() - start, 3)}, log

    with ThreadPoolExecutor(max_workers=args.test_jobs) as pool:
        pending = {}
        # Start the longest integration target early so it overlaps other work.
        jobs.sort(key=lambda job: job[0] != "replication")
        for name, flags in jobs:
            cancel = queue.Queue(maxsize=1)
            mailboxes.append(cancel)
            pending[pool.submit(execute, name, flags, cancel)] = name
        try:
            for future in as_completed(pending):
                name = pending[future]
                results[name], log = future.result()
                print(f"--- test target {name} ({results[name]['seconds']}s)", flush=True)
                print(log.read_text(), flush=True)
        except BaseException:
            for cancel in mailboxes:
                cancel.put_nowait(True)
            raise
    result["test_targets"] = results
    if any(item["status"] != "passed" for item in results.values()):
        raise RuntimeError("application test target failed")


def worker(args):
    result = {"profile": args.profile, "architecture": platform.machine(),
              "rustc": capture(["rustc", "-V"]), "phases": {}, "status": "failed"}
    env = dict(os.environ)
    cache = Path(env["ELSE_E2E_CACHE"])
    started = time.perf_counter()
    with tempfile.TemporaryDirectory(prefix="else-e2e-worker-") as directory:
        work = Path(directory)
        installation = work / "installation"
        cli = installation / "bin/elsebot"
        env_file = work / "compose.env"
        env_file.write_text("")
        env.update(ELSE_TEST_COMPOSE_PROJECT=env["COMPOSE_PROJECT_NAME"], ELSE_TEST_COMPOSE_ENV_FILE=str(env_file),
                   ELSE_TEST_COMPOSE_FILE=str(ROOT / "infra/e2e/compose.yaml"))

        def phase(name, command):
            print(f"--- {name}", flush=True)
            start = time.perf_counter()
            try:
                run(command, cwd=ROOT, env=env)
            finally:
                result["phases"][name] = round(time.perf_counter() - start, 3)

        try:
            if args.suite in ["application", "all"]:
                cargo = ["cargo", "test", "--workspace", "--locked", "--profile", args.profile]
                flags, selected, excluded = application_targets(args.compile_live)
                result.update(integration_targets=selected, uncompiled_live_targets=excluded)
                phase("compile_tests", cargo + flags + ["--no-run"])
                start = time.perf_counter()
                try:
                    application_tests(cargo, selected, args, env, work, result)
                finally:
                    result["phases"]["application"] = round(time.perf_counter() - start, 3)
                phase("doctests", cargo + ["--doc"])
            if args.suite in ["installer", "all"]:
                repo = releases(work)
                env.update(ELSE_HOME=str(installation), ELSE_REPO=str(repo),
                           ELSE_INSTALL_TEST_CACHE=str(cache), ELSE_INSTALL_TEST_PROFILE=args.profile,
                           ELSE_API_PORT=str(free_port()), ELSE_E2E_LINK_PREFIX=str(installation / "links"),
                           ELSE_E2E_SOURCE=str(ROOT), ELSE_E2E_POLL_INTERVAL="0.1",
                           ELSE_E2E_WAIT_SECONDS="60", ELSE_AUTOSTART="0",
                           ELSE_SERVICE_NAME="e2e-" + uuid.uuid4().hex[:12])
                if platform.system() == "Darwin":
                    env["ELSE_E2E_LAUNCHD"] = "1"
                phase("installer", ["bash", str(ROOT / "scripts/e2e/install-scenario.sh")])
            result["status"] = "passed"
        except Exception as error:
            print(f"E2E failure: {error}", file=sys.stderr, flush=True)
            for log in sorted((installation / "logs").glob("*.log")):
                print(f"--- {log.name}\n" + "\n".join(log.read_text(errors="replace").splitlines()[-30:]), flush=True)
        finally:
            if cli.exists():
                try:
                    subprocess.run([str(cli), "stop"], env=env, timeout=20, check=False)
                except subprocess.TimeoutExpired:
                    print("Timed out stopping the isolated E2E service", file=sys.stderr)
            result["seconds"] = round(time.perf_counter() - started, 3)
    print(MARKER + json.dumps(result), flush=True)
    return result["status"] == "passed"


def native_cache(cache):
    """Read the installed SDK; never run rustup against the owner's toolchain."""
    sdk = Path(capture(["rustc", "--print", "sysroot"]))
    binaries = cache / "toolchain/cargo/bin"
    binaries.mkdir(parents=True, exist_ok=True)
    rustup = shutil.which("rustup")
    if not rustup:
        raise RuntimeError("the native macOS lane requires rustup")
    if not (binaries / "rustup").exists():
        shutil.copy2(Path(rustup).resolve(), binaries / "rustup")
    for name in ["cargo", "rustc", "rustdoc"]:
        link = binaries / name
        if link.is_symlink() and os.readlink(link) != "rustup":
            link.unlink()
        if not link.exists():
            link.symlink_to("rustup")
    sdk_env = dict(os.environ, CARGO_HOME=str(binaries.parent),
                   RUSTUP_HOME=str(cache / "toolchain/rustup"), RUSTUP_TOOLCHAIN="e2e-host")
    # Rustup's proxies also configure the macOS LLVM helper library paths.
    # All metadata is written to the test cache; the host SDK is only read.
    linked = cache / "toolchain/rustup/toolchains/e2e-host"
    if not linked.exists() or linked.resolve() != sdk.resolve():
        run([str(binaries / "rustup"), "toolchain", "link", "e2e-host", str(sdk)], env=sdk_env)
    if not (cache / "toolchain/rustup/settings.toml").exists():
        run([str(binaries / "rustup"), "default", "e2e-host"], env=sdk_env)
    # Compressed crates are immutable. Copy the mutable index, never hardlink it.
    registry = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))) / "registry"
    dest = cache / "toolchain/cargo/registry"
    dest.mkdir(exist_ok=True)
    for name in ["index", "cache"]:
        if (registry / name).exists() and not (dest / name).exists():
            shutil.copytree(registry / name, dest / name)
    # A private copy can be upgraded by the installer without modifying the host.
    msb = shutil.which("msb")
    if msb and not (cache / "microsandbox").exists():
        prefix = Path(msb).resolve().parent.parent
        libraries = list((prefix / "lib").glob("libkrunfw*"))
        if libraries:
            runtime = cache / "microsandbox"
            (runtime / "bin").mkdir(parents=True)
            (runtime / "lib").mkdir()
            shutil.copy2(Path(msb).resolve(), runtime / "bin/msb")
            for library in libraries:
                shutil.copy2(library, runtime / "lib" / library.name, follow_symlinks=False)


def own_cache(cache_dir):
    """Hold the cache directory for this run; released when the process exits."""
    cache_dir.mkdir(parents=True, exist_ok=True)
    handle = (cache_dir / "owner.lock").open("w")
    try:
        fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        handle.close()
        raise RuntimeError(f"another E2E run owns {cache_dir}; wait for it or pass a different --cache-dir")
    return handle


def ubuntu_cache_volume(cache_dir, sdk_key):
    """The Ubuntu build cache belongs to the same owner as the cache directory."""
    owner = hashlib.sha256(str(cache_dir).encode()).hexdigest()[:12]
    return f"else-e2e-{owner}-ubuntu-arm64-{sdk_key}"


def clean_environment():
    env = dict(os.environ)
    for key in list(env):
        if key.startswith(("ELSE_", "CARGO_PROFILE_", "PG")) or key in {
            "DATABASE_URL", "TEST_DATABASE_URL", "SECRET_KEY", "SECRET_KEY_FILE",
            "OWNER_TOKEN_FILE", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET",
            "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_INCREMENTAL",
        }:
            env.pop(key)
    return env


def host_guard():
    paths = [Path.home() / p for p in [".local/bin/elsebot", ".else/current", "Library/LaunchAgents/bot.else.else-api.plist"]]
    result = {}
    for path in paths:
        if path.is_symlink():
            result[str(path)] = ("link", os.readlink(path))
        elif path.is_file():
            result[str(path)] = ("file", hashlib.sha256(path.read_bytes()).hexdigest())
        else:
            result[str(path)] = None
    return result


def lane(args, variant, output, image, volume, cancel):
    project = "else-e2e-" + variant + "-" + uuid.uuid4().hex[:8]
    env = clean_environment()
    env.update(COMPOSE_PROJECT_NAME=project, ELSE_TEST_S3_REGION="us-east-1", ELSE_TEST_S3_ACCESS_KEY_ID="elsetestaccess01",
               ELSE_TEST_S3_SECRET_ACCESS_KEY="elsetestsecret0123456789abcdef")
    before = host_guard()
    started = time.perf_counter()
    with tempfile.TemporaryDirectory(prefix="else-e2e-") as directory, (output / f"{variant}.log").open("w") as log:
        work = Path(directory)
        config = work / "compose.env"
        config.write_text("")
        compose = ["docker", "compose", "--project-name", project, "--env-file", str(config),
                   "-f", str(ROOT / "infra/e2e/compose.yaml")]
        container = project + "-runner"
        result = {"variant": variant, "status": "failed"}
        try:
            run(compose + ["up", "-d", "--wait"], cancel=cancel, env=env, stdout=log, stderr=subprocess.STDOUT)
            # Neither native lane reads the owner's ignored checkout .env.
            source = work / "source"
            snapshot(source)
            worker_args = ["--worker", "--suite", args.suite, "--profile", args.profile,
                           "--test-threads", str(args.test_threads), "--test-jobs", str(args.test_jobs)]
            if args.compile_live:
                worker_args += ["--compile-live"]
            if variant == "macos-arm64":
                cache = args.cache_dir / variant
                native_cache(cache)
                env.update(CARGO_HOME=str(cache / "toolchain/cargo"), RUSTUP_HOME=str(cache / "toolchain/rustup"),
                           CARGO_TARGET_DIR=str(cache / "build"), ELSE_E2E_CACHE=str(cache),
                           RUSTUP_TOOLCHAIN="e2e-host",
                           PATH=str(cache / "toolchain/cargo/bin") + os.pathsep + env["PATH"])
                s3 = capture(compose + ["port", "s3", "8333"], env=env).rsplit(":", 1)[1]
                env["ELSE_TEST_S3_ENDPOINT"] = f"http://127.0.0.1:{s3}"
                command = [sys.executable, str(source / "scripts/test-e2e.py"), *worker_args]
                run(command, cancel=cancel, env=env, stdout=log, stderr=subprocess.STDOUT)
            else:
                for parent, dirs, files in os.walk(source):
                    Path(parent).chmod(0o755)
                    for name in files:
                        path = Path(parent) / name
                        if not path.is_symlink():
                            path.chmod(path.stat().st_mode | 0o044)
                socket_group = capture(["docker", "run", "--rm", "--platform", "linux/arm64",
                                        "-v", "/var/run/docker.sock:/var/run/docker.sock", image,
                                        "stat", "-c", "%g", "/var/run/docker.sock"])
                command = ["docker", "run", "--rm", "--init", "--platform", "linux/arm64", "--name", container,
                           "--network", project + "_default", "--group-add", socket_group, "--cpus", str(args.build_jobs),
                           "-v", str(source) + ":/src:ro", "-v", "/var/run/docker.sock:/var/run/docker.sock",
                           "-v", f"{volume}:/cache", "-e", "ELSE_E2E_CACHE=/cache",
                           "-e", "CARGO_TARGET_DIR=/cache/build",
                           "-e", f"CARGO_BUILD_JOBS={args.build_jobs}"]
                env["ELSE_TEST_S3_ENDPOINT"] = "http://s3:8333"
                for key in ["COMPOSE_PROJECT_NAME",
                            "ELSE_TEST_S3_ENDPOINT", "ELSE_TEST_S3_REGION", "ELSE_TEST_S3_ACCESS_KEY_ID", "ELSE_TEST_S3_SECRET_ACCESS_KEY"]:
                    command += ["-e", key]
                command += [image, "python3", "/src/scripts/test-e2e.py", *worker_args]
                run(command, cancel=cancel, env=env, stdout=log, stderr=subprocess.STDOUT)
        except Exception as error:
            result["error"] = str(error)
        finally:
            log.flush()
            frames = [line[len(MARKER):] for line in (output / f"{variant}.log").read_text().splitlines() if line.startswith(MARKER)]
            if frames:
                result.update(json.loads(frames[-1]))
            if "error" in result:
                result["status"] = "failed"
            elif not frames:
                result["error"] = "worker produced no result record"
            for cleanup in [["docker", "rm", "-f", container], compose + ["down", "--volumes", "--remove-orphans"]]:
                try:
                    subprocess.run(cleanup, env=env, stdout=log, stderr=subprocess.STDOUT, check=False, timeout=45)
                except subprocess.TimeoutExpired:
                    result.update(status="failed", error="timed out cleaning up isolated services")
            if host_guard() != before:
                result.update(status="failed", error="the owner's installation links or service file changed")
        result["total_seconds"] = round(time.perf_counter() - started, 3)
    return result


def positive(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("variants", nargs="*", choices=VARIANTS)
    parser.add_argument("--suite", choices=["all", "application", "installer"], default="all")
    parser.add_argument("--profile", choices=["dev", "release"], default="dev")
    parser.add_argument("--repeat", type=positive, default=1)
    parser.add_argument("--parallel", type=int, choices=[1, 2], default=2)
    parser.add_argument("--compile-live", action="store_true", help="also compile gated live-provider/VM targets; runtime gates stay disabled")
    parser.add_argument("--build-jobs", type=positive, default=4, help="Ubuntu CPU/build concurrency")
    parser.add_argument("--test-threads", type=positive, default=4)
    parser.add_argument("--test-jobs", type=positive, default=4, help="concurrent integration test binaries per platform")
    parser.add_argument("--cache-dir", type=Path, default=ROOT / "target/e2e-cache")
    parser.add_argument("--output", type=Path, default=ROOT / "target/e2e" / (time.strftime("%Y%m%d-%H%M%S") + f"-{os.getpid()}"))
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    def stop_run(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, stop_run)
    if args.worker:
        return 0 if worker(args) else 1
    variants = list(dict.fromkeys(args.variants or (VARIANTS if platform.system() == "Darwin" else ["ubuntu-arm64"])))
    if platform.machine() not in ["arm64", "aarch64"]:
        parser.error("these lanes require a native arm64 host; emulation is deliberately disabled")
    if "macos-arm64" in variants and platform.system() != "Darwin":
        parser.error("macos-arm64 requires a macOS host")
    if "macos-arm64" in variants:
        for tool in ["rustup", "rustc", "sqlite3"]:
            if not shutil.which(tool):
                parser.error(f"macOS prerequisite missing: {tool}; the harness will not install host packages")
        capture(["xcrun", "-sdk", "macosx", "-f", "metal"])
    if capture(["docker", "version", "--format", "{{.Server.Arch}}"]) != "arm64":
        parser.error("the Docker engine must be arm64")
    args.cache_dir = args.cache_dir.resolve()
    try:
        cache_owner = own_cache(args.cache_dir)
    except RuntimeError as error:
        parser.error(str(error))
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    report = {"revision": capture(["git", "rev-parse", "HEAD"], cwd=ROOT),
              "working_tree": capture(["git", "status", "--porcelain"], cwd=ROOT),
              "host": platform.platform(), "host_cpus": os.cpu_count(),
              "command": [sys.executable, *sys.argv],
              "suite": args.suite, "profile": args.profile, "build_jobs": args.build_jobs,
              "test_threads": args.test_threads, "parallel": args.parallel,
              "test_jobs": args.test_jobs,
              "compile_live": args.compile_live, "runs": []}
    image = volume = None
    if "ubuntu-arm64" in variants:
        start = time.perf_counter()
        image_file = args.output / "ubuntu-image.id"
        try:
            sdk_image = capture(["docker", "image", "inspect", "rust:1", "--format", "{{.Id}}"])
        except subprocess.CalledProcessError:
            run(["docker", "pull", "--platform", "linux/arm64", "rust:1"])
            sdk_image = capture(["docker", "image", "inspect", "rust:1", "--format", "{{.Id}}"])
        volume = ubuntu_cache_volume(args.cache_dir, sdk_image.split(":")[-1][:12])
        print("Preparing cached Ubuntu 26.04 arm64 worker image", flush=True)
        with (args.output / "image-build.log").open("w") as log:
            run(["docker", "build", "--platform", "linux/arm64", "--iidfile", str(image_file),
                 "-f", str(ROOT / "infra/e2e/ubuntu.Dockerfile"), str(ROOT / "infra/e2e")], stdout=log, stderr=subprocess.STDOUT)
        image = image_file.read_text().strip()
        report["ubuntu_image"] = image
        report["image_prepare_seconds"] = round(time.perf_counter() - start, 3)
        report["ubuntu_cache_volume"] = volume
    for number in range(1, args.repeat + 1):
        output = args.output / f"run-{number}"
        output.mkdir()
        start = time.perf_counter()
        failed = False
        # At most two independent platform actors. Only this coordinator owns
        # the report; no shared .env file, mutable checkout, or global lock.
        with ThreadPoolExecutor(max_workers=args.parallel) as pool:
            pending = {}
            mailboxes = []
            for variant in variants:
                print(f"Running {variant}, pass {number}; log: {output / (variant + '.log')}", flush=True)
                cancel = queue.Queue(maxsize=1)
                mailboxes.append(cancel)
                pending[pool.submit(lane, args, variant, output, image, volume, cancel)] = variant
            def collect(future):
                nonlocal failed
                result = future.result()
                result["run"] = number
                report["runs"].append(result)
                (args.output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
                print(f"{result['variant']}: {result['status']} ({result['total_seconds']:.2f}s)", flush=True)
                if result["status"] != "passed":
                    print(result.get("error", "see worker log"), file=sys.stderr)
                    failed = True
            try:
                for future in as_completed(pending):
                    collect(future)
            except KeyboardInterrupt:
                for cancel in mailboxes:
                    cancel.put_nowait(True)
                print("Cancelling lanes and removing isolated services", file=sys.stderr, flush=True)
                for future in as_completed(pending):
                    if not any(r["variant"] == pending[future] and r["run"] == number for r in report["runs"]):
                        collect(future)
                return 130
        report.setdefault("matrix_seconds", []).append(round(time.perf_counter() - start, 3))
        (args.output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
        if failed:
            return 1
    print(f"Results: {args.output / 'results.json'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
