# First checkout build

The target is the first `cargo build` or `cargo run` in a development checkout.
The baseline is the original `fbb9f3a` source, exported from Git with its own
lockfile and default Cargo profiles. The final change sets `[profile.dev]
debug = 0`; application source and `Cargo.lock` exactly match the baseline.

## Results

| Clean development build | Sample 1 | Sample 2 | Mean |
| --- | --- | --- | --- |
| Original defaults | 52.232 s | 50.918 s | 51.58 s |
| Debug info disabled | 46.377 s | 39.958 s | 43.17 s |

The mean falls by **8.41 s (16.3%)**.

| Development binary | Before (MiB) | After (MiB) | Smaller |
| --- | --- | --- | --- |
| `else-api` | 62.79 | 53.28 | 15.2% |
| `else-desktop` | 121.82 | 103.26 | 15.2% |

[Raw measurements and commands](build-speed-results.json) retain all four samples.

## Tradeoff

Generating debug information for the GPUI dependency graph is avoidable compiler
work. Skip it by default and request full debugging when needed with
`CARGO_PROFILE_DEV_DEBUG=2` or `CARGO_PROFILE_TEST_DEBUG=2`.
Backtraces still have function names; source-line tables and variable/type
information require the override. Development optimization, assertions,
overflow checks, and incremental compilation retain their defaults. Release
builds retain full Cargo optimization: level 3, 16 codegen units, and thin local
LTO. This change does not reduce Rust runtime optimization or change providers.

## Dependency investigation

An AWS-LC-to-ring experiment (`84c3e22`) retained full release optimization and
produced smaller release binaries. It did not demonstrate a worthwhile checkout
build improvement: the two development samples averaged 49.99 s before and
54.05 s after (8.1% slower). Full release samples averaged 80.59 s before and
76.27 s after (5.4% less time). Absolute timings varied substantially on battery
power, so the small release difference is not convincing evidence of a large gain.

[Dependency-experiment raw measurements](build-speed-dependency-results.json)
retain all eight samples. The provider change is excluded from the final change.
AWS-LC's native compilation largely overlaps other compilation; a long individual
crate duration cannot be treated as an equal wall-time saving.

GPUI's optional grammars, speech, inspector, and profiler were already disabled.
Its image decoders are enabled inside the toolkit, so they cannot be removed
through this application's manifest. The optional GPUI Fast backend also keeps
upstream packages in the build, as the [toolkit documents](https://github.com/longbridge/gpui-kit/blob/main/README.md#gpui-fast).
For a larger dependency-level improvement, upstream feature gates for image
formats and unused component families are the next target to investigate.

## Method

- Measured 2026-11-03 on Apple M5 Pro, 18 logical/physical cores, 48 GiB RAM,
  macOS 26.6 arm64, Rust/Cargo 1.99.1, Apple's default linker and Cargo job count.
- Two serial samples per variant; the second reverses baseline/current order.
  Both workspace binaries are built together.
- Every sample uses an empty, distinct `CARGO_TARGET_DIR`; compiler wrappers
  are disabled. Registry sources and filesystem caches stay warm.
- `--locked --offline` excludes registry access and downloads. Rust installation,
  system package installation, and the guest image build are also excluded.
- Wall time covers the complete Cargo process, including compilation and linking.
- Baseline source is exported with `git archive`; no checkout or manifest is
  changed. Each variant uses its own lockfile and profiles.
- The laptop was on battery power; absolute build times varied. Treat these
  results as local observations from two samples, not a cross-platform guarantee.

```sh
cargo fetch --locked
python3 scripts/benchmark-build.py --baseline-ref fbb9f3a --output /tmp/else-build-benchmark --runs 2 --profiles dev
```

The output directory must not exist. Each sample retains its build log and
Cargo HTML timing report; `results.json` records revisions, source patches,
commands, toolchain, elapsed seconds, exit status, and executable sizes in bytes.
