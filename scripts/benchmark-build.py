#!/usr/bin/env python3
"""Compare clean workspace builds against the original Cargo profile defaults.

Run `cargo fetch --locked` first. Each build gets a new target directory;
downloads, toolchain installation, and the guest image are outside the timing.
Logs, Cargo HTML timings, binary sizes, and results.json remain in --output.
"""

import argparse
import json
import os
from pathlib import Path
import platform
import subprocess
import time


ROOT = Path(__file__).resolve().parent.parent
DEFAULTS = {
    "release": {"opt-level": "3", "codegen-units": "16", "lto": "false"},
    "dev": {"debug": "2"},
}


def positive_int(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be at least 1")
    return number


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path, help="new results directory")
    parser.add_argument("--runs", type=positive_int, default=2)
    parser.add_argument("--profiles", nargs="+", choices=DEFAULTS, default=list(DEFAULTS))
    parser.add_argument("--variants", nargs="+", choices=["baseline", "current"],
                        default=["baseline", "current"])
    args = parser.parse_args()
    overrides = [key for key in os.environ if key.startswith("CARGO_PROFILE_")]
    if overrides:
        parser.error("unset profile overrides first: " + ", ".join(sorted(overrides)))
    output = args.output.resolve()
    try:
        output.mkdir(parents=True, exist_ok=False)
    except FileExistsError:
        parser.error(f"output directory already exists: {output}")
    env = dict(os.environ, RUSTC_WRAPPER="", RUSTC_WORKSPACE_WRAPPER="")

    def capture(*command):
        return subprocess.check_output(command, cwd=ROOT, text=True).strip()

    report = {
        "revision": capture("git", "rev-parse", "HEAD"),
        "diff": capture("git", "diff", "HEAD", "--", "Cargo.toml", "Cargo.lock"),
        "rustc": capture("rustc", "-Vv"),
        "cargo": capture("cargo", "-V"),
        "platform": platform.platform(),
        "logical_cpus": os.cpu_count(),
        "baseline_overrides": DEFAULTS,
        "builds": [],
    }

    def save():
        (output / "results.json").write_text(json.dumps(report, indent=2) + "\n")

    save()
    for run in range(1, args.runs + 1):
        # Reverse the order on even runs to reduce cache/thermal ordering bias.
        variants = args.variants if run % 2 else list(reversed(args.variants))
        for profile in args.profiles:
            for variant in variants:
                label = f"{profile}-{variant}-{run}"
                target = output / label
                # Fail rather than accidentally benchmark a warm target if a
                # profile or variant was passed twice.
                target.mkdir()
                command = ["cargo", "build", "--workspace", "--locked", "--offline",
                           "--timings", "--profile", profile]
                if variant == "baseline":
                    for key, value in DEFAULTS[profile].items():
                        command += ["--config", f"profile.{profile}.{key}={value}"]
                print(f"Building {label}; log: {output / (label + '.log')}", flush=True)
                start = time.perf_counter()
                with (output / (label + ".log")).open("w") as log:
                    result = subprocess.run(command, cwd=ROOT,
                                            env=dict(env, CARGO_TARGET_DIR=str(target)),
                                            stdout=log, stderr=subprocess.STDOUT)
                elapsed = time.perf_counter() - start
                build = {"profile": profile, "variant": variant, "run": run,
                         "seconds": round(elapsed, 3), "command": command,
                         "exit_code": result.returncode}
                if result.returncode == 0:
                    directory = target / ("debug" if profile == "dev" else profile)
                    build["binary_bytes"] = {
                        name: (directory / name).stat().st_size
                        for name in ["else-api", "else-desktop"]
                    }
                report["builds"].append(build)
                save()
                print(f"{label}: {elapsed:.3f}s (exit {result.returncode})", flush=True)
                if result.returncode:
                    raise SystemExit(f"Build failed; inspect {output / (label + '.log')}")
    print(f"Results: {output / 'results.json'}")


if __name__ == "__main__":
    main()
