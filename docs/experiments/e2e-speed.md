# Native ARM64 E2E speed

The new `scripts/test-e2e.py` runs native macOS arm64 and Ubuntu 26.04 aarch64
application and installer checks. The legacy distribution matrix is unchanged.

## Results

Application execution, excluding compilation and infrastructure setup:

| Platform | Serial control | Concurrent samples | Concurrent mean | Less time |
| --- | --- | --- | --- | --- |
| macos-arm64 | 191.17 s | 90.58 s / 89.57 s | 90.08 s | 52.9% |
| ubuntu-arm64 | 185.66 s | 83.38 s / 93.16 s | 88.27 s | 52.5% |

The control already includes the fixture timeout fixes. These percentages
measure the additional benefit of four concurrent integration binaries and
shared access for independent S3 buckets. Each binary retains its own fixture
coordination; emulator outage/restart tests take exclusive access.

A separate focused check of seven HTTP/auth tests fell from **62.02 s to
0.31 s (99.5% less time)**. Those tests intentionally use an unreachable lazy
database pool. A test-only 100 ms acquisition timeout replaces repeated
30-second waits; the production timeout and all assertions are unchanged.
Database teardown marks the pool closed, force-drops its private database,
and then waits for checked-out connections, avoiding a wait before disconnect.

Complete lane timings include fresh services, compilation, application tests,
doctests, real installation/update checks, and cleanup:

| Platform | Sample 1 | Sample 2 | Mean |
| --- | --- | --- | --- |
| macos-arm64 | 139.50 s | 137.97 s | 138.74 s |
| ubuntu-arm64 | 128.22 s | 136.49 s | 132.35 s |

With both platforms running together, the full matrix took **139.53 s**
and **137.99 s** (mean **138.76 s**). Cached Ubuntu image
preparation took 2.12 s separately. These are
prepared-cache timings, not empty-cache first installations. Dependencies and
prerequisite downloads were already available; per-pass compilation is still
included in the full-lane numbers and recorded separately in the JSON.

[Raw phase/per-target measurements, commands and environment](e2e-speed-results.json)
include all final samples and the serial controls. The source was an uncommitted
working tree based on `8da0b5f`, subsequently committed with this report.

## What changed

- Cache the Ubuntu SDK/system packages in image layers independent of source;
  retain Cargo/toolchain/Microsandbox caches across disposable installations.
- Use native ARM64 on both platforms. The runner rejects emulation.
- Overlap independent platforms and integration binaries with bounded worker
  counts, keeping per-test databases, buckets, ports and installation state.
- Use the normal development profile for functional E2E runs; optional
  `--profile release` validates optimized artifacts. Normal installations still
  build release binaries. No new compiler profile or production dependency is
  introduced.
- Skip compilation of gated `*_live` integration targets by default; include
  their build checks with `--compile-live`. Runtime live gates remain disabled.
- Poll installer readiness every 100 ms, with the existing 60-second budget.
- Make the backup CLI fixture deterministic when Microsandbox is absent, and
  wait for the scheduler slot itself after cancellation in memory tests.

Both final passes report 276 passing tests on macOS and
280 on Ubuntu, plus all installer assertions. These counts include the
real-S3 test's gated early return; no real cloud bucket, live provider, or VM
was exercised. The desktop is built and its unit tests run, but its UI is not
automated. The installer checks real native API/desktop binaries, busy-update
refusal, a backup before a no-op migration, conversation preservation, read-only
database access, and an idempotent reinstall. macOS uses a temporary launchd
service; Ubuntu uses a managed process without systemd.

The five offline runner regression checks passed. All test targets, including
gated live targets, were also compiled on both platforms without executing
providers/VMs. A real macOS cancellation returned 130 and removed the lane
containers, volumes and network while preserving the owner's installation
links and service configuration. Both installer lanes passed an additional
smoke run after the readiness loops were changed to enforce elapsed-time
deadlines.

## Reproduce

```sh
# Same native prerequisites as normal development; Docker/Compose required.
python3 scripts/test-e2e.py macos-arm64 ubuntu-arm64 --repeat 2 --output /tmp/else-e2e-final
# Serial application control; on the final source S3 read sharing is retained.
python3 scripts/test-e2e.py macos-arm64 ubuntu-arm64 --suite application --test-jobs 1 --output /tmp/else-e2e-serial
python3 scripts/e2e/test_runner.py
```

Measured 2026-11-03 on Apple M5 Pro, 18 cores/48 GiB, macOS 26.6.2,
Rust/Cargo 1.99.1, native arm64 OrbStack Docker. Ubuntu 26.04 workers have four
CPUs/build jobs and four test threads per binary; macOS uses Cargo's normal
build concurrency and four test threads. PostgreSQL 18.6 and SeaweedFS 4.50
run in separate per-lane Compose projects with fresh volumes. The platform
matrix runs two lanes concurrently; four integration binaries run per lane.

There is one serial control and two final samples per platform. Filesystem
caches stay warm, and the laptop's load/power state can affect absolute times.
The control predates the S3 reader/writer coordination and scheduler fixture
fix; the final-source serial command reproduces the comparison setup, not
that exact historical source. Do not interpret these results as a cold-build,
release-profile, amd64-emulation, or cross-machine speedup claim. No amd64,
Debian, Arch, or additional Ubuntu version was tested in this change.

See [development instructions](../development.md#native-arm64-e2e) for cache
locations, prerequisites, isolation and coverage controls.
