# Backup, recovery, and assistant acceptance tasks

These tasks finish the [assistant POC](../../TODO.md#assistant-poc). Follow the [runbook](README.md). Backup and restore remain operator functions; no recovery-management UI is required.

## POC-021: Encrypted database backup and restore

Depends on: POC-012.

Implement the researched Rust age streaming path, purpose-separated key derivation, a versioned manifest, and publication of a completion record last. Capture PostgreSQL custom dumps through one serialized local backup worker. Restore verified data into a fresh database and private staging paths.

Acceptance:

- Back up a real test installation and recover bot, conversation, provider model/effort/tier settings and revisions, and migration state with matching hashes and rows. Restoration must preserve the native sub-agent prohibition.
- Wrong key, truncation, corruption, producer failure, and interrupted publication cannot produce a valid completed set or overwrite a destination.
- Existing destination files and databases remain unchanged on refusal or failed restore. Missing keys never trigger reset or regeneration.
- Scheduling, retry, retention, and bounded temporary storage work locally. Master keys and credentials stay outside artifacts and logs.
- A database-only set reports its coverage explicitly. It must not claim whole-assistant recovery when VM or other required state is absent.

References: [Backup implementation design](../research/backup-and-recovery.md), [encryption fixture](../experiments/backups/README.md), [master key](../security.md#master-key-and-encrypted-secrets).

## POC-022: Coordinated assistant backup and restore

Depends on: POC-015, POC-016, POC-019, POC-020, POC-021.

Coordinate a safe database checkpoint, standalone disk archives including images, and supported host provider state. Record every dependency and omission in the encrypted manifest. Use disk snapshots for routine backups. Restore into a new installation directory, database, and runtime storage, with explicit cutover after validation.

Acceptance:

- Capture two bots, memory generations/views, prompt configuration, chats, guest files, and browser profiles under one completed set.
- Drain or defer active work safely. Checkpoint Chromium before disk capture. Do not wake an explicitly stopped computer for backup.
- A failed or partial component prevents whole-set publication. Retention cannot remove a required artifact.
- Restore with the original runtime unavailable. Confirm guest files, memory, canonical conversations, and bot/default/deletion state.
- Restore supported native sessions or clearly require login and reconstruct context from PostgreSQL. Never claim portable provider authentication without evidence.
- Start the recovered application twice. Saved state is unchanged and uncertain external actions are not replayed.

References: [Coordinated capture](../research/backup-and-recovery.md), [planned restoration](../restore.md), [snapshot constraints](../backups.md#microsandbox-snapshot-findings).

## POC-023: Automatic R2 or S3 replication

Depends on: POC-022.

Add configured bucket replication to the same worker. Upload encrypted artifacts and the encrypted manifest before publishing the remote completion record. Track local and remote success separately.

Test end to end against a local S3 emulator rather than a real bucket. Add a `seaweedfs` service to `infra/compose.yaml` using the pinned image `chrislusf/seaweedfs:4.50` (`server -dir=/data -s3 -s3.port=8333`) and a named volume. Bind it to `127.0.0.1`, take synthetic access keys from `.env` (`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` in the container), and add a health check. Add matching `ELSE_TEST_S3_*` endpoint, region, and key entries to `.env.example`. The replication tests require the emulator the way the API tests require Compose PostgreSQL: a missing emulator fails with a clear message instead of skipping. Each test uses its own bucket or prefix. MinIO and LocalStack are not used; see the [emulator comparison](../experiments/s3-emulators/README.md). RustFS `1.0.4` passed the same probe and is the fallback if SeaweedFS regresses.

Use only the S3 operations the probe verified, so behavior on R2 stays predictable: `PutObject` (with `If-None-Match: *` for the completion record), multipart create, upload, complete, list, and abort, `HeadObject`, ranged `GetObject`, `ListObjectsV2`, `DeleteObject`, and `DeleteObjects`. Configure the endpoint, region, bucket, prefix, and path-style addressing; never hard-code AWS endpoints.

Acceptance, all automated against the emulator:

- Run scheduled capture and upload, then verify remote payloads by streamed readback and ciphertext SHA-256. A remote object altered after upload (overwritten through the API) is detected and the set is not marked remotely complete.
- Publish the completion record with `If-None-Match: *`; a second publication of the same set is refused and leaves the first record unchanged.
- Interrupt an upload (stop the worker or the API mid-transfer) and restart. Retry completes the same set, aborts the abandoned multipart upload, skips objects whose hashes already match, and never replaces a valid set or marks partial data complete.
- Stop the emulator during replication: the local set stays complete, the remote state reports the failure, and the next run after the emulator returns finishes the upload.
- Exercise retention with several synthetic sets. Keep the last complete remote recovery point and any local set awaiting replication; remove an old completion record before its artifacts.
- Fetch and restore a completed set from the emulator into a fresh installation, with the local artifacts deleted and the original runtime unavailable.
- Restart the emulator container between upload and restore; the completed set is still listed and restorable.
- Record the emulator image, endpoint, and test prefixes without secrets in the handoff.

Optional live check: with `ELSE_LIVE_S3=1` and an owner-authorized R2 or S3 bucket and isolated prefix in the environment, the same upload, verification, conditional publication, and remote restore run against the real endpoint. Routine `cargo test` never contacts a real bucket. Run it once before relying on R2 for host-loss recovery and record the outcome; until then, report R2-specific compatibility as unverified, while the task itself is complete once the emulator suite passes.

References: [Remote replication and retention](../research/backup-and-recovery.md#automatic-remote-replication-remains-required), [automatic backup requirement](../backups.md), [S3 emulator comparison](../experiments/s3-emulators/README.md).

## POC-024: Assistant POC acceptance

Depends on: POC-012, POC-015, POC-016, POC-019, POC-020, POC-023.

Demonstrate the extended assistant through its small messaging UI and documented operator tools. Save a repeatable synthetic scenario and evidence against the real application. Complete the backlog only after the entire assigned target works.

Acceptance:

- Main recalls a corrected saved fact through source-aware memory and delegates by a newly created bot's role.
- One provider slot supports real parent/child continuation. Injected source text cannot widen actual API or guest access.
- Perform guest work, retain its files, switch providers during the ongoing task, and continue from known job and action outcomes.
- Stop and delete active bots, restart, and verify retained data, stopped intent, and process/resource cleanup.
- Restore the completed remote backup into a clean installation. Preserve owner-updated model profiles and the disabled native sub-agent policy. Resume conversations and guest files after any documented provider login.
- Rerun the first usable POC scenario to catch regressions. Workspace and relevant feature checks pass, and documentation matches observed behavior.
- Record limitations and follow-up work separately. Do not introduce deferred email, notes, projects, routines, mobile, or extra desktop features to complete this task.

References: [POC targets](../../TODO.md#delivery-targets), [research evidence limits](../research/README.md), [product requirements](../product.md).
