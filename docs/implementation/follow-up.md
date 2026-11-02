# Limitations and follow-up after the assistant POC

Recorded at POC-024. These are known gaps, not part of the delivered targets. Deferred product areas (email, notes, projects, routines, mobile, extra desktop features) stay out of scope.

## Verified only partly

- **R2 compatibility.** Replication passed against the SeaweedFS emulator only. Run `ELSE_LIVE_S3=1 cargo test -p else-api --test replication a_real_bucket` with an owner-authorized R2 bucket and isolated prefix before relying on it for host-loss recovery.
- **Provider sessions after restore.** Native Codex and Claude sessions are not backed up; restored conversations rebuild context from PostgreSQL, and a new host needs `codex login` and a Claude login. Session portability was not attempted.
- **Power-loss durability.** Backup publication syncs files and directories and links completion records last, but no real power cut was tested.
- **Provider-spawn crash boundary.** Turn recovery is tested with synthetic providers and API kills; a crash at a real provider CLI's spawn boundary (and its descendants) has no process-level test.
- **Desktop input.** Mouse text selection and IME composition were not exercised; the omabox check typed with synthetic key events.

## Behavior to revisit

- **Foreground commands.** Stop work cancels background guest jobs but not a running `computer_run`, which ends with its 300 s timeout.
- **Parked delegations.** A switch to another provider interrupts sending or running turns, not a root parked `waiting` for delegated children.
- **Effort reporting.** Claude Code does not echo the effort level, so it is recorded as unknown.
- **Checkpoint deadline.** Handoff checkpoints allow 60 s (the research suggested 20 s) because each attempt starts a CLI.
- **Backup size and time.** Every disk archive includes its image (about 390 MB for the desktop image), so a two-bot coordinated capture took about 165 s. Shared image artifacts or incremental archives would reduce it, at the cost of dependency-aware retention.
- **Unverified remote sets.** Remote retention ignores completion records this installation did not verify (for example after restoring onto a new database); such sets must be removed by the owner.
- **Busy computers defer backups.** A long background job or an open desktop defers the coordinated set until it ends.
