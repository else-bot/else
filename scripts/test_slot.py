#!/usr/bin/env python3
"""Run a command while holding one of this machine's test slots.

Every checkout and worktree of this user shares the slots, so suites started
by several agents at once queue instead of oversubscribing the CPU, where
tests that pass alone time out. ELSE_TEST_SLOTS sets the slot count; the
default is one per 16 CPUs, at least one.

    scripts/test_slot.py -- cargo nextest run --workspace --locked
"""

import fcntl
import os
import signal
from pathlib import Path
import subprocess
import sys
import time

SLOTS_DIR = Path.home() / ".cache" / "else-dev" / "test-slots"


def slot_count():
    configured = os.environ.get("ELSE_TEST_SLOTS")
    if configured:
        return max(1, int(configured))
    cpus = len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else os.cpu_count() or 1
    return max(1, cpus // 16)


def acquire(count):
    SLOTS_DIR.mkdir(parents=True, exist_ok=True)
    waiting_since = None
    while True:
        for index in range(count):
            # Python opens descriptors non-inheritable, so the command's own
            # children never hold a slot after it exits.
            fd = os.open(SLOTS_DIR / f"slot-{index}", os.O_RDWR | os.O_CREAT, 0o600)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                os.close(fd)
                continue
            if waiting_since is not None:
                print(f"test slot {index} free after {time.monotonic() - waiting_since:.0f}s", file=sys.stderr, flush=True)
            return fd
        if waiting_since is None:
            waiting_since = time.monotonic()
            print(f"waiting for a test slot: {count} suite(s) already running on this machine "
                  f"(ELSE_TEST_SLOTS={count})", file=sys.stderr, flush=True)
        time.sleep(1)


def main(argv):
    if argv[:1] == ["--"]:
        argv = argv[1:]
    if not argv:
        raise SystemExit(__doc__)
    slot = acquire(slot_count())
    try:
        child = subprocess.Popen(argv)
        # Hold the slot until the suite exits, even when this wrapper is told to stop.
        def forward(number, _frame):
            child.send_signal(number)
        for number in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
            signal.signal(number, forward)
        return child.wait()
    finally:
        os.close(slot)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
