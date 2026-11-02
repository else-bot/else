# Backup design

This is the proposed backup design. The [November 2 verification](experiments/bot-vm-verification.md) tested disk and full archives, recovery into a separate runtime, and a restored desktop with its browser profile. Whole-assistant recovery, encryption, and R2 or S3 uploads remain untested.

Backups should automatically capture encrypted PostgreSQL exports and encrypted VM snapshot archives. PostgreSQL owns the application configuration, bot definitions, memory, and chats. VM archives preserve each bot's computer, including its saved files and browser profile.

If a machine fails, data becomes corrupted, or I want to migrate, I should be able to start else.bot on a new machine and restore the latest backup. The goal is to resume where I left off without rebuilding the assistant's knowledge or configuration by hand.

Keep application records in PostgreSQL and VM artifacts in object storage. Store the artifact references and recovery metadata with the application state. Application code should recreate generated files and runtime resources from the restored records and VM archives.

Use R2 or S3 for the encrypted backup set. Keep the backup schedule and retention configurable. Keep the decryption key and storage access credentials recoverable separately from the server being backed up. Recovery resumes from the latest completed backup set.

## Encryption and key recovery

The [master key design](security.md#master-key-and-encrypted-secrets) covers `SECRET_KEY`, separate derived keys, encrypted recovery metadata, and the key versions needed for restoration. The recovery mechanism remains open.

## Microsandbox snapshot findings

The local Rust SDK supports two snapshot scopes:

| Snapshot | Captures | Restore behavior | Proposed use |
| --- | --- | --- | --- |
| Disk | Writable filesystem state and captured owned storage. | Boots a fresh VM from saved disk state. | Routine scheduled backups. |
| Full | Disk, RAM, and execution state. | Resumes execution with stricter compatibility requirements. | Optional checkpoints when resuming live processes matters. |

Disk snapshots omit RAM. `Snapshot::save()` exports zstd-compressed `.msb` archives, and `with_image` includes OCI artifacts for offline recovery. Archive size depends on saved data. [Rust snapshot reference](https://docs.microsandbox.dev/sdk/rust/snapshots).

The current guide also describes change-only exports against an exact base snapshot. They omit disk layers and memory objects supplied by that base. Restoration needs the matching base, and compaction can require a new standalone baseline. Start with standalone disk archives. Enable dependent exports only after validating the pinned SDK and runtime, with periodic new baselines and retention that preserves dependencies. [Exporting changes](https://github.com/superradcompany/microsandbox/blob/main/docs/sandboxes/snapshots.mdx#export-changes).

Owned volumes can be captured, but host bind-mount contents remain external. Avoid placing irreplaceable bot data in an untracked host mount. Include every required external filesystem or host-side secret in the backup inventory. [Snapshot mount behavior](https://github.com/superradcompany/microsandbox/blob/main/docs/sandboxes/snapshots.mdx#mount-behavior).

Full restoration needs a compatible runtime and guest kernel with matching CPU and memory settings. External connections need reconnection. [Restore constraints](https://docs.microsandbox.dev/sdk/rust/snapshots).

In the tested v0.7.13 runtime, full capture rejects active managed `exec` jobs. A guest daemon without an active managed execution session could be captured and resumed. Use disk snapshots for routine backups. Full checkpoints require draining managed jobs first and must not promise to resume an arbitrary active CLI turn. The [verification report](experiments/bot-vm-verification.md#snapshot-and-recovery-constraints) records the observed restriction.

Browser profiles need an application checkpoint before a disk backup. The desktop restore passed after Chromium exited cleanly through its control API and removed its profile lock. Capturing the still-open browser had carried its hostname and process lock into the restored VM. A guest filesystem flush alone does not establish a consistent browser checkpoint.

## Full snapshot size estimates

For planning with a 2 GiB guest, allow roughly 2 GiB for its RAM state before compression, then add captured filesystem data, metadata, and any included OCI image artifacts. This is a sizing assumption, not a measured snapshot size.

For example, 2 GiB of RAM state plus 1 GiB of captured filesystem data gives about 3 GiB before compression and additional image artifacts. A bot with more downloads, installed packages, or browser cache needs more storage. The 2 GiB RAM setting is not a cap on the total snapshot size.

The exported archive uses zstd, so the uploaded size depends on the captured contents and their compressibility. Do not assume a fixed compression ratio. Disk-only snapshots avoid the RAM component. [Snapshot scopes and export options](https://docs.microsandbox.dev/sdk/rust/snapshots).

Measure the actual exported and encrypted archive, including any required base artifacts. The SDK documents local `size_bytes()` as the apparent upper-file size, so that value alone is not a reliable total backup size. [Size reporting](https://docs.microsandbox.dev/sdk/rust/snapshots).

The [snapshot experiment report](experiments/vm-snapshots.md) includes measured compressed archive sizes. Full Python-guest archives ranged from about 24 MiB when idle to 806 MiB with random data. A desktop disk archive with its OCI image included was about 659 MiB. These are unencrypted samples, not fixed backup sizes. The [rerun guide](experiments/run-vm-snapshot-benchmark.md) includes the synthetic benchmark.

## Automatic VM backups to R2 or S3

The Rust API should coordinate database and VM backups through one backup worker:

1. Wait for a safe checkpoint and acquire maintenance access to the affected VMs. Block new use during capture and coordinate application writes with the database export. Respect active VM locks and the owner's stopped state.
2. Save application buffers and commit pending writes before capturing VM state. Use the flush policy appropriate to whether the guest is running, paused, or stopped. The runtime's filesystem flush does not save buffers still held by applications. [Guest flushing](https://github.com/superradcompany/microsandbox/blob/main/docs/sandboxes/snapshots.mdx#guest-flushing).
3. Capture disk snapshots with integrity records and export compressed archives. Preserve a baseline with the required OCI image artifacts for each image version. Routine exports can omit that image cache while its backup remains available. Release maintenance access once the local checkpoints and corresponding database export are fixed.
4. Encrypt the database export and VM archives locally, then upload them under immutable object keys. Reuse the planned database backup encryption and upload path. Record and explicitly verify snapshot integrity, since normal export or import does not perform that independent payload scan. [Integrity verification](https://docs.microsandbox.dev/sdk/rust/snapshots).
5. Publish an encrypted recovery manifest only after every required upload is verified. It should identify the database export, bot snapshot IDs, base dependencies, image digests, runtime versions, checksums, and capture times. A partial upload must not replace the latest completed backup.

The Rust S3 SDK can upload local artifacts to R2 through its S3-compatible endpoint. else.bot would run this upload worker against the owner's bucket, using local Microsandbox snapshots. [Cloudflare's Rust SDK example](https://developers.cloudflare.com/r2/examples/aws/aws-sdk-rust/).

Limit snapshot, compression, and upload concurrency within the server's resource budget. Use bounded buffers for encryption and upload, and cap temporary disk usage. Keep frequency, retention, and optional change-only exports configurable. Never delete a baseline, image artifact, or earlier archive while a retained recovery manifest depends on it. Measure archive size, capture pause time, upload volume, and restore time during implementation.

## Restoration

The [planned restore procedure](restore.md) describes recovery on a new server and the requirement that startup preserve existing state.
