# VM snapshot experiment results

The [November 2 bot VM verification](bot-vm-verification.md) passed after CPU virtualization was enabled. This page retains the original blocked attempt and the measured snapshot sizes.

## October 25 preflight attempt

On October 25, 2026, the official v0.7.10 Linux x86_64 runtime was downloaded into an isolated temporary directory. Its SHA-256 matched the release metadata. The executable ran successfully, but `msb doctor` failed before VM creation.

The host checks and runtime diagnostics at that time found no `/dev/kvm` device and no exposed AMD `svm` CPU flag. The `kvm_amd` module is installed. No disk or full snapshot was created in that attempt. The November 2 run below records the later measurements.

The runtime files were measured directly:

| Artifact | Bytes | MiB |
| --- | ---: | ---: |
| Downloaded runtime bundle | 29,685,017 | 28.31 |
| `msb` executable | 52,193,528 | 49.78 |
| `libkrunfw` firmware | 21,628,880 | 20.63 |
| Extracted executable and firmware | 73,822,408 | 70.40 |

These are installation sizes, excluding the Rust API, guest images, VM storage, and image cache. [Pinned upstream release](https://github.com/superradcompany/microsandbox/releases/tag/v0.7.10).

The [prepared benchmark](run-vm-snapshot-benchmark.md) uses a 1 vCPU, 2 GiB Python guest. It compares compressed disk and full archives with no bundled OCI image. Cases cover an idle guest, 512 MiB of compressible RAM, and 512 MiB of seeded random-looking RAM. The latter two also write 128 MiB of matching data to disk by default. The script is designed to record archive bytes, checksums, integrity verification, and elapsed time.

At that point, only preflight and command syntax had been checked. The VM capture path had not run.

## November 2 snapshot measurements

Microsandbox v0.7.13 ran with 1 vCPU and 2 GiB guest RAM. These are measured compressed archives excluding OCI image artifacts, encryption, and upload overhead.

| Synthetic workload | Disk archive | Full archive |
| --- | ---: | ---: |
| Idle guest with a 16 MiB test buffer | 0.78 MiB | 24.19 MiB |
| 512 MiB repeated RAM and 128 MiB repeated disk data | 0.89 MiB | 29.34 MiB |
| 512 MiB random RAM and 128 MiB random disk data | 128.94 MiB | 805.51 MiB |

A disk archive of the Chromium desktop and browser profile, including its image, was 658.62 MiB. Disk recovery, full process recovery, and desktop recovery passed. Full capture required ending managed execution jobs first.

The [verification report](bot-vm-verification.md) explains workload details and recovery constraints. [Structured results](results/microsandbox-v0.7.13-linux.json) include archive checksums.

## Benchmark guide

The [benchmark guide](run-vm-snapshot-benchmark.md) includes the synthetic capture script. [Backup design](../backups.md) covers image dependencies, encryption, and retention.
