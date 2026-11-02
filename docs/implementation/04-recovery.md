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

Add configured bucket replication to the same worker. Upload encrypted artifacts and the encrypted manifest before publishing the remote completion record. Track local and remote success separately. Use an owner-authorized test bucket or isolated prefix for verification.

Acceptance:

- Run scheduled capture and upload, then verify remote payload hashes with a supported integrity check.
- Inject an interrupted upload and restart. Retry completes the same set without replacing a valid set or marking partial data complete.
- Exercise retention with several synthetic sets. Keep the last complete remote recovery point and any local set awaiting replication.
- Fetch and restore a completed set from remote storage without the local artifacts or original runtime.
- Record endpoint, format, and test prefix without secrets. If bucket access is unavailable, leave this task unchecked and report the missing access; a local storage mock cannot prove remote recovery.

References: [Remote replication and retention](../research/backup-and-recovery.md#automatic-remote-replication-remains-required), [automatic backup requirement](../backups.md).

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
