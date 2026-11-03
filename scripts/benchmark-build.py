#!/usr/bin/env python3
"""Compare clean builds of the current source with a committed baseline.

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
PROFILES = ["release", "dev"]
PACKAGES = ["else-api", "else-desktop"]


def positive_int(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be at least 1")
    return number


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path, help="new results directory")
    parser.add_argument("--runs", type=positive_int, default=2)
    parser.add_argument("--baseline-ref", help="Git commit/tag to export without changing this checkout")
    parser.add_argument("--profiles", nargs="+", choices=PROFILES, default=PROFILES)
    parser.add_argument("--packages", nargs="+", choices=PACKAGES, help="default: whole workspace")
    parser.add_argument("--variants", nargs="+", choices=["baseline", "current"],
                        default=["baseline", "current"])
    args = parser.parse_args()
    if "baseline" in args.variants and not args.baseline_ref:
        parser.error("--baseline-ref is required when measuring the baseline")
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

    baseline = output / "source-baseline"
    baseline_revision = None
    if args.baseline_ref:
        baseline_revision = capture("git", "rev-parse", f"{args.baseline_ref}^{{commit}}")
        baseline.mkdir()
        archive = output / "baseline.tar"
        with archive.open("wb") as stream:
            subprocess.run(["git", "archive", baseline_revision], cwd=ROOT, stdout=stream, check=True)
        subprocess.run(["tar", "-xf", str(archive), "-C", str(baseline)], check=True)
        archive.unlink()

    report = {
        "revision": capture("git", "rev-parse", "HEAD"),
        "diff": capture("git", "diff", "HEAD", "--", "Cargo.toml", "Cargo.lock", "apps"),
        "baseline_revision": baseline_revision,
        "rustc": capture("rustc", "-Vv"),
        "cargo": capture("cargo", "-V"),
        "platform": platform.platform(),
        "logical_cpus": os.cpu_count(),
        "packages": args.packages or PACKAGES,
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
                source = baseline if variant == "baseline" else ROOT
                command = ["cargo", "build", "--locked", "--offline",
                           "--timings", "--profile", profile]
                if args.packages:
                    for package in args.packages:
                        command += ["--package", package]
                else:
                    command += ["--workspace"]
                print(f"Building {label}; log: {output / (label + '.log')}", flush=True)
                start = time.perf_counter()
                with (output / (label + ".log")).open("w") as log:
                    result = subprocess.run(command, cwd=source,
                                            env=dict(env, CARGO_TARGET_DIR=str(target)),
                                            stdout=log, stderr=subprocess.STDOUT)
                elapsed = time.perf_counter() - start
                build = {"profile": profile, "variant": variant, "run": run,
                         "seconds": round(elapsed, 3), "command": command,
                         "source": "baseline" if variant == "baseline" else "current",
                         "exit_code": result.returncode}
                if result.returncode == 0:
                    directory = target / ("debug" if profile == "dev" else profile)
                    build["binary_bytes"] = {
                        name: (directory / name).stat().st_size
                        for name in args.packages or PACKAGES
                    }
                report["builds"].append(build)
                save()
                print(f"{label}: {elapsed:.3f}s (exit {result.returncode})", flush=True)
                if result.returncode:
                    raise SystemExit(f"Build failed; inspect {output / (label + '.log')}")
    print(f"Results: {output / 'results.json'}")


if __name__ == "__main__":
    main()
