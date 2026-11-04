#!/usr/bin/env python3
"""Run the optional S3 replication tests against a private emulator.

Each run owns a unique Compose project, volume and kernel-assigned host
port, so worktrees and concurrent runs never share or stop each other's
emulator. The checkout's .env is never read.

Examples:
  scripts/test-replication.py
  scripts/test-replication.py -- --test-threads 2 retention
"""

import argparse
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import uuid

ROOT = Path(__file__).resolve().parent.parent
COMPOSE_FILE = ROOT / "infra/compose.yaml"
CREDENTIALS = {
    "ELSE_TEST_S3_REGION": "us-east-1",
    "ELSE_TEST_S3_ACCESS_KEY_ID": "elsetestaccess01",
    "ELSE_TEST_S3_SECRET_ACCESS_KEY": "elsetestsecret0123456789abcdef",
}


def run_group(command, env):
    """Run in its own process group; terminate the group if interrupted."""
    with subprocess.Popen(command, cwd=ROOT, env=env, start_new_session=True) as child:
        try:
            return child.wait()
        except BaseException:
            os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
            raise


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("test_args", nargs=argparse.REMAINDER, help="passed to the test binary after --")
    args = parser.parse_args()
    test_args = args.test_args[1:] if args.test_args[:1] == ["--"] else args.test_args

    def stop_run(_signal, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, stop_run)

    project = "else-replication-" + uuid.uuid4().hex[:12]
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("ELSE_TEST_", "COMPOSE_"))}
    with tempfile.TemporaryDirectory(prefix=project + "-") as directory:
        env_file = Path(directory) / "compose.env"
        env_file.write_text("".join(f"{key}={value}\n" for key, value in CREDENTIALS.items()))
        compose = ["docker", "compose", "--project-name", project, "--env-file", str(env_file),
                   "-f", str(COMPOSE_FILE)]
        print(f"Starting S3 emulator project {project}", flush=True)
        try:
            subprocess.run(compose + ["up", "-d", "--wait"], cwd=ROOT, env=env, check=True)
            address = subprocess.check_output(compose + ["port", "s3", "8333"], cwd=ROOT, env=env,
                                              text=True).strip()
            port = address.rsplit(":", 1)[1]
            env.update(CREDENTIALS, ELSE_TEST_S3_ENDPOINT=f"http://127.0.0.1:{port}",
                       ELSE_TEST_COMPOSE_PROJECT=project, ELSE_TEST_COMPOSE_ENV_FILE=str(env_file),
                       ELSE_TEST_COMPOSE_FILE=str(COMPOSE_FILE))
            return run_group(["cargo", "test", "-p", "else-api", "--locked", "--test", "replication",
                              "--", "--ignored", *test_args], env)
        except KeyboardInterrupt:
            return 130
        finally:
            subprocess.run(compose + ["down", "--volumes", "--remove-orphans", "--timeout", "1"],
                           cwd=ROOT, env=env, check=False)


if __name__ == "__main__":
    sys.exit(main())
