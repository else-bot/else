"""Runner checks without Cargo or a desktop: python3 -m unittest discover -s scripts -p 'test_dev_tests.py'."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import dev_tests as runner


class SchedulerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.events = self.directory / "events"
        fake = self.directory / "omabox"
        fake.write_text(f"""#!{sys.executable}
import json, os, sys, time
job = json.loads(sys.argv[-1])
mode = job['argv'][0]
with open({str(self.events)!r}, 'a', buffering=1) as events:
    events.write(json.dumps(['start', job['slug'], os.getpid()]) + '\\n')
    if mode == 'hang':
        time.sleep(60)
    time.sleep(0.15)
    events.write(json.dumps(['end', job['slug'], os.getpid()]) + '\\n')
print('test sample ... ' + ('FAILED' if mode == 'failed' else 'ok'))
print(f'test result: ok. {{int(mode != "failed")}} passed; {{int(mode == "failed")}} failed; 0 ignored; 0 measured; 0 filtered out')
if mode != 'missing':
    print({runner.SENTINEL!r} + json.dumps({{'exit': int(mode == 'failed'), 'timeout': mode == 'timeout'}}))
""")
        fake.chmod(0o755)
        env = patch.dict(os.environ, PATH=f"{self.directory}{os.pathsep}{os.environ['PATH']}")
        env.start()
        self.addCleanup(env.stop)
        self.results = {
            "id": "run", "dir": str(self.directory), "status": "running", "jobs": 2,
            "test_threads": 2, "tokio_worker_threads": 2, "box": {"name": "fake"},
            "selection": {"packages": [], "tests": [], "serial": False, "filter": []},
            "worker": {"pid": os.getpid(), "log": "worker.log"}, "build": {"status": "passed"},
            "binaries": [],
        }

    def binary(self, name, mode="passed"):
        binary = {"key": name, "target": name, "package": "fake", "version": "1",
                  "kind": "test", "cwd": str(self.directory), "executable": mode,
                  "log": str(self.directory / f"{name}.log"), "status": "pending"}
        self.results["binaries"].append(binary)
        return binary

    def read_events(self):
        return [json.loads(line) for line in self.events.read_text().splitlines()]

    def assert_reaped(self):
        for event, _, pid in self.read_events():
            if event == "start":
                with self.assertRaises(ProcessLookupError):
                    os.kill(pid, 0)

    def test_jobs_bound_overlap_and_atomic_progress(self):
        for i in range(5):
            self.binary(str(i))
        snapshots = []
        save = runner.save

        def record(directory, results):
            save(directory, results)
            snapshots.append(runner.load(directory))
            self.assertIn("status: running", (directory / "summary.txt").read_text())

        with patch.object(runner, "save", side_effect=record):
            runner.run_binaries(self.directory, self.results)
        active = maximum = 0
        for event, _, _ in self.read_events():
            active += 1 if event == "start" else -1
            maximum = max(maximum, active)
        self.assertEqual((maximum, active), (2, 0))
        self.assertTrue(all(b["status"] == "passed" and b["counts"]["passed"] == 1
                            for b in runner.load(self.directory)["binaries"]))
        self.assertTrue(any(sum(b["status"] == "running" for b in s["binaries"]) == 2 for s in snapshots))
        self.assert_reaped()

    def test_one_job_preserves_order(self):
        self.results["jobs"] = 1
        for name in ("first", "second", "third"):
            self.binary(name)
        runner.run_binaries(self.directory, self.results)
        self.assertEqual([e[:2] for e in self.read_events()],
                         [[event, name] for name in ("first", "second", "third") for event in ("start", "end")])

    def test_failed_and_timed_out_binaries_do_not_stop_queue(self):
        for mode in ("failed", "timeout", "passed"):
            self.binary(mode, mode)
        runner.run_binaries(self.directory, self.results)
        self.assertEqual([b["status"] for b in self.results["binaries"]], ["failed", "timeout", "passed"])
        self.assertEqual(self.results["binaries"][0]["failed_tests"], ["sample"])
        self.assertEqual(self.results["binaries"][0]["counts"]["failed"], 1)

    def test_missing_exit_line_is_an_error_and_queue_continues(self):
        self.binary("missing", "missing")
        self.binary("passed")
        runner.run_binaries(self.directory, self.results)
        self.assertEqual([b["status"] for b in self.results["binaries"]], ["error", "passed"])
        self.assertIn("without the in-box exit line", self.results["binaries"][0]["error"])

    def test_outer_timeout_reaps_all_active_children(self):
        for i in range(3):
            self.binary(str(i), "hang")
        with patch.object(runner, "TIMEOUT_SECONDS", -119.5):
            with self.assertRaises(subprocess.TimeoutExpired):
                runner.run_binaries(self.directory, self.results)
        self.assertEqual(self.results["binaries"][2]["status"], "pending")
        self.assert_reaped()

    def test_interrupt_reaps_all_children_and_keeps_pending(self):
        for i in range(3):
            self.binary(str(i), "hang")
        save = runner.save

        def interrupt(directory, results):
            save(directory, results)
            if sum(b["status"] == "running" for b in results["binaries"]) == 2:
                deadline = time.monotonic() + 5
                while (not self.events.exists() or len(self.read_events()) < 2) and time.monotonic() < deadline:
                    time.sleep(0.01)
                raise KeyboardInterrupt

        with patch.object(runner, "save", side_effect=interrupt):
            with self.assertRaises(KeyboardInterrupt):
                runner.run_binaries(self.directory, self.results)
        self.assertEqual(self.results["binaries"][2]["status"], "pending")
        self.assert_reaped()

    def test_spawn_error_reaps_previous_child(self):
        self.binary("first", "hang")
        self.binary("second")
        start = runner.start_binary

        def fail_second(results, binary):
            if binary["key"] == "second":
                deadline = time.monotonic() + 5
                while not self.events.exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                raise OSError("launch failed")
            return start(results, binary)

        with patch.object(runner, "start_binary", side_effect=fail_second):
            with self.assertRaisesRegex(OSError, "launch failed"):
                runner.run_binaries(self.directory, self.results)
        self.assert_reaped()

    def test_history_uses_latest_successful_matching_measurement(self):
        for name in ("short", "long", "unknown"):
            self.binary(name)
        for name, seconds, filters in (("001", (20, 10), []), ("002", (1, 30), []), ("003", (90, 1), ["filter"])):
            previous = self.directory / name
            previous.mkdir()
            history = dict(self.results, selection=dict(self.results["selection"], filter=filters), binaries=[
                {"key": key, "seconds": duration, "status": "passed"}
                for key, duration in zip(("short", "long"), seconds)])
            (previous / "results.json").write_text(json.dumps(history))
        corrupt = self.directory / "004"
        corrupt.mkdir()
        (corrupt / "results.json").write_text("{")
        with patch.object(runner, "RUNS", self.directory):
            runner.order_binaries(self.directory / "005", self.results)
        self.assertEqual([b["key"] for b in self.results["binaries"]], ["long", "short", "unknown"])

    def test_jobs_validation_and_cpu_budget(self):
        for invalid in ("0", "-1", "1.5", "many"):
            with self.assertRaises(argparse.ArgumentTypeError):
                runner.positive_jobs(invalid)
        self.assertEqual(runner.positive_jobs("4"), 4)
        for cpus, expected in ((1, 1), (8, 2), (32, 8), (128, 8)):
            with patch.object(os, "sched_getaffinity", return_value=set(range(cpus))):
                self.assertEqual(runner.default_jobs(), expected)

    def test_named_targets_keep_workspace_feature_resolution(self):
        def scope(packages, tests):
            return runner.build_scope({"packages": packages, "tests": tests})
        self.assertEqual(scope(["else-api"], ["bots"]), ["--workspace", "--test", "bots"])
        self.assertEqual(scope([], ["bots", "routines"]), ["--workspace", "--test", "bots", "--test", "routines"])
        self.assertEqual(scope(["else-api"], []), ["-p", "else-api"])
        self.assertEqual(scope([], []), ["--workspace"])

    def test_combined_binary_runs_one_job_per_module(self):
        binary = dict(self.binary("api"), key="else-api/test/api")
        source = '#[path = "../common/mod.rs"]\nmod common;\n\nmod backup_settings;\nmod bots;\nmod settings;\n'
        shards = runner.shard(binary, source)
        self.assertEqual([s["key"] for s in shards],
                         ["else-api/test/api/backup_settings", "else-api/test/api/bots", "else-api/test/api/settings"])
        self.assertEqual(shards[1]["filter"], ["bots::"])
        self.assertEqual(shards[2]["filter"], ["settings::", "--skip", "backup_settings::"])
        self.assertEqual(len({s["log"] for s in shards}), 3)
        self.assertEqual(runner.shard(binary, "fn main() {}\n"), [binary])

        self.results.update(selection={"packages": [], "tests": ["api"], "filter": [], "serial": False}, jobs=2)
        self.results["binaries"] = [dict(shards[2], status="failed", package="else-api", target="api"),
                                    dict(shards[1], status="passed", package="else-api", target="api")]
        self.assertEqual(runner.rerun_args(self.results),
                         ["-p else-api --test api --jobs 2 -- settings:: --skip backup_settings::"])

        argv = []
        with patch.object(runner.subprocess, "Popen", side_effect=lambda a, **_: argv.append(a)):
            self.results.update(id="run", box={"name": "box"}, test_threads=2, tokio_worker_threads=2)
            runner.start_binary(self.results, self.results["binaries"][0])
        job = json.loads(argv[0][-1])
        self.assertEqual(job["argv"][1:], ["settings::", "--skip", "backup_settings::", "--test-threads", "2"])

    def test_serial_overrides_jobs_and_uses_one_test_thread(self):
        for serial, jobs, threads in ((False, 3, 2), (True, 1, 1)):
            args = argparse.Namespace(package="else-api", test=None, serial=serial, jobs=3, filter=[])
            with patch.object(runner, "RUNS", self.directory), \
                    patch.object(runner.subprocess, "Popen"), \
                    patch.object(runner, "wait", return_value=0), \
                    patch.object(runner.shutil, "which", return_value="omabox"):
                self.assertEqual(runner.run_tests(args), 0)
            requests = [json.loads(p.read_text()) for p in self.directory.glob("*/request.json")]
            request = next(r for r in requests if r["selection"]["serial"] == serial)
            self.assertEqual((request["jobs"], request["test_threads"]), (jobs, threads))

    def test_cold_order_starts_known_slow_targets_first(self):
        for name in ("other", *reversed(runner.COLD_SECONDS)):
            self.binary(name)
        with patch.object(runner, "RUNS", self.directory):
            runner.order_binaries(self.directory / "run", self.results)
        self.assertEqual([b["key"] for b in self.results["binaries"]][:5], list(runner.COLD_SECONDS))

    def test_sparse_history_does_not_push_unknown_slow_targets_behind_short_ones(self):
        self.binary("short")
        self.binary("else-api/test/api/delegation")
        previous = self.directory / "001"
        previous.mkdir()
        history = dict(self.results, binaries=[{"key": "short", "status": "passed", "seconds": 1}])
        (previous / "results.json").write_text(json.dumps(history))
        with patch.object(runner, "RUNS", self.directory):
            runner.order_binaries(self.directory / "002", self.results)
        self.assertEqual(self.results["binaries"][0]["key"], "else-api/test/api/delegation")

    def test_worker_records_interruption_and_error_and_removes_box(self):
        for error, status in ((KeyboardInterrupt(), "interrupted"), (OSError("broken"), "error")):
            with self.subTest(status=status):
                request = dict(self.results, id="run-test", selection=dict(self.results["selection"]))
                (self.directory / "request.json").write_text(json.dumps(request))
                opened = os.open

                def open_lock(*args, **kwargs):
                    fd = opened(*args, **kwargs)
                    self.addCleanup(os.close, fd)
                    return fd

                def output(argv, **kwargs):
                    if argv[0] == "omabox":
                        return str(self.directory)
                    return "revision" if argv[1] == "rev-parse" else ""

                def build(directory, results):
                    results["build"]["status"] = "passed"
                    results["binaries"] = [dict(self.binary("active"), status="running"), self.binary("pending")]
                    return []

                with patch.object(runner.os, "open", side_effect=open_lock), \
                        patch.object(runner.fcntl, "flock"), \
                        patch.object(runner.signal, "signal"), \
                        patch.object(runner, "build", side_effect=build), \
                        patch.object(runner.subprocess, "check_output", side_effect=output), \
                        patch.object(runner.subprocess, "run") as run, \
                        patch.object(runner, "run_binaries", side_effect=error):
                    self.assertEqual(runner.worker(self.directory), 1)
                saved = runner.load(self.directory)
                self.assertEqual(saved["status"], status)
                self.assertEqual([b["status"] for b in saved["binaries"]], [status, "pending"])
                self.assertIn("removed", saved["cleanup"]["box"])
                self.assertEqual(run.call_args.args[0][:2], ["omabox", "down"])


if __name__ == "__main__":
    unittest.main()
