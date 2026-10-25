# VM snapshot experiment results

On October 25, 2026, the official v0.7.10 Linux x86_64 runtime was downloaded into an isolated temporary directory. Its SHA-256 matched the release metadata. The executable ran successfully, but `msb doctor` failed before VM creation.

The host checks and runtime diagnostics found no `/dev/kvm` device and no exposed AMD `svm` CPU flag. The `kvm_amd` module is installed. A working KVM host is required to finish the experiment. No disk or full snapshot was created, so the [snapshot size estimates](../backups.md#full-snapshot-size-estimates) remain unmeasured.

The runtime files were measured directly:

| Artifact | Bytes | MiB |
| --- | ---: | ---: |
| Downloaded runtime bundle | 29,685,017 | 28.31 |
| `msb` executable | 52,193,528 | 49.78 |
| `libkrunfw` firmware | 21,628,880 | 20.63 |
| Extracted executable and firmware | 73,822,408 | 70.40 |

These are installation sizes, excluding the Rust API, guest images, VM storage, and image cache. [Pinned upstream release](https://github.com/superradcompany/microsandbox/releases/tag/v0.7.10).

The [prepared benchmark](run-vm-snapshot-benchmark.md) uses a 1 vCPU, 2 GiB Python guest. It compares compressed disk and full archives with no bundled OCI image. Cases cover an idle guest, 512 MiB of compressible RAM, and 512 MiB of seeded random-looking RAM. The latter two also write 128 MiB of matching data to disk by default. The script is designed to record archive bytes, checksums, integrity verification, and elapsed time.

Only the preflight and command syntax have been checked on this machine. The VM capture path has not run, and this synthetic workload does not measure Chromium or the desktop.

## Benchmark guide

The [benchmark guide](run-vm-snapshot-benchmark.md) includes the full script and commands. [Snapshot estimates and backup design](../backups.md) explain how these measurements would inform storage planning.
