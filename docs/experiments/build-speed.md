# First-build speed

Measured on 2026-11-03 against `fbb9f3a` with the same source and `Cargo.lock`.
The only build changes are the root Cargo profiles. Both workspace binaries are
built together, matching the installer's package selection.

## Results

| Build | Baseline samples (s) | Current samples (s) | Mean seconds, before → after | Less time |
| --- | --- | --- | --- | --- |
| release | 67.136, 63.744 | 55.565, 53.509 | 65.44 → 54.54 | 16.7% |
| dev | 39.348, 38.211 | 36.220, 33.986 | 38.78 → 35.10 | 9.5% |

Release builds save **10.90 seconds (16.7%)** on average; development builds save
**3.68 seconds (9.5%)**. All eight builds succeeded.

Release executable sizes are stable across both samples:

| Binary | Before (MiB) | After (MiB) | Increase |
| --- | --- | --- | --- |
| `else-api` | 21.15 | 26.64 | 26.0% |
| `else-desktop` | 30.70 | 39.53 | 28.8% |

[Raw measurements and exact commands](build-speed-results.json) are checked in.

## Policy

This is a local manager for up to hundreds of bots. Providers run in separate
processes, so faster installation takes priority over maximum optimization of
the manager and minimum executable size.

| Setting | Previous | Current |
| --- | --- | --- |
| Release optimization | `3` | `1` (basic optimization) |
| Release codegen units | `16` | `256` |
| Release LTO | `false` (thin local LTO) | `"off"` |
| Development/test debug info | Full | File/line tables |

Panic unwinding, development assertions, overflow checks, dependency features,
and the installer's build command retain their defaults. File/line backtraces
remain available in development; full variable/type debug info can be restored
with `CARGO_PROFILE_DEV_DEBUG=2` or `CARGO_PROFILE_TEST_DEBUG=2`.

Cargo documents these tradeoffs in its [profile reference](https://doc.rust-lang.org/cargo/reference/profiles.html).
More codegen units allow more parallel compilation, while lower optimization
and no LTO reduce compiler work. These choices can increase binary size and
reduce throughput for CPU-heavy Rust operations. This benchmark measures build
time; it does not establish runtime capacity for hundreds of bots.

## Method

- Apple M5 Pro, 18 logical/physical cores, 48 GiB RAM, macOS 26.6, arm64.
- Rust/Cargo 1.99.1, Apple's default linker, default Cargo job count.
- Two samples per profile/variant, run serially without other builds. The
  second sample reverses baseline/current order.
- Every sample uses an empty, distinct `CARGO_TARGET_DIR`. Rust compiler
  wrappers are disabled. Registry sources and filesystem caches are warm.
- `--locked --offline` excludes registry access and downloads. Rust installation,
  system package installation, and the guest image build are also excluded.
- Wall time covers the complete Cargo process, including compilation and linking.
- The baseline uses Cargo config overrides to restore exactly the profile
  settings changed here; the current variant uses the checked-in manifest.

```sh
cargo fetch --locked
python3 scripts/benchmark-build.py --output /tmp/else-build-benchmark --runs 2
```

The output directory must not exist. Each sample retains its build log and
Cargo HTML timing report; `results.json` includes commands, toolchain, manifest
diff, elapsed seconds, exit status, and executable sizes in bytes. The runner
does not change manifests or reuse the normal build cache.

These are local observations from two samples, not a cross-platform estimate
or an end-to-end installer timing. Re-run on the target machine before assuming
the same gain on Linux or on machines with fewer cores.
