# Limitations and follow-up after the assistant POC

Recorded at POC-024. These are known gaps, not part of the delivered targets. Deferred product areas (email, notes, projects, routines, mobile, extra desktop features) stay out of scope.

## Verified only partly

- **R2 compatibility.** Replication passed against the SeaweedFS emulator only. Run `ELSE_LIVE_S3=1 cargo test -p else-api --test replication a_real_bucket` with an owner-authorized R2 bucket and isolated prefix before relying on it for host-loss recovery.
- **Provider sessions after restore.** Native Codex and Claude sessions are not backed up; restored conversations rebuild context from PostgreSQL, and a new host needs `codex login` and a Claude login. Session portability was not attempted.
- **Power-loss durability.** Backup publication syncs files and directories and links completion records last, but no real power cut was tested.
- **Provider-spawn crash boundary.** Turn recovery is tested with synthetic providers and API kills; a crash at a real provider CLI's spawn boundary (and its descendants) has no process-level test.
- **Desktop input.** Mouse text selection and IME composition were not exercised; the omabox check typed with synthetic key events.

- **Codex login refresh.** The host `auth.json` is bind-mounted into each sandbox; a Codex token refresh that replaces the file by rename may fail through the bind mount (POC-010). Check: force a refresh (an expired access token) during a turn and confirm the host file is updated.
- **Provider network access.** Provider sandboxes keep open network access, because the CLIs talk to their services; only the absence of credentials and the API's authorization limit what a provider can reach (POC-010). Check: an egress allowlist for the provider endpoints, tested with a canary listener.
- **Accepted send, lost response.** A send whose HTTP response is lost after the API accepted it was not demonstrated end to end; the API-down case was (POC-012). Check: drop the response with a proxy and confirm the desktop's retry finds the stored turn.
- **Summary quality.** Model summaries were checked on a synthetic corpus with a handful of recall questions (POC-015); there is no broader quality evaluation. Check: a recall set across long histories, compared with the deterministic view.
- **Hostile child results and forced parking.** A delegated child's result is data to its parent, but a hostile result and a parked parent that must be terminated were tested only with synthetic providers (POC-016). Check: a live child returning instructions, and a parent that ignores the park interrupt.
- **Guest isolation.** VM network isolation and the guest's lack of credentials were checked with smoke probes (private, link-local, and host targets denied; no credential directories), not an exhaustive audit (POC-017, POC-018). Check: a broader probe set, including IPv6 and DNS rebinding, from the guest.
- **Injection resistance.** The acceptance run shows injected text could not widen API access (no grant, no cross-bot memory, owner token and anonymous calls refused); it does not prove a model never follows injected instructions within its own permissions.

## Behavior to revisit

- **Foreground commands.** Resolved in POC-026: a running `computer_run` is ended (its whole process group) within about 10 seconds once its turn is no longer running.
- **Parked delegations.** A switch to another provider interrupts sending or running turns, not a root parked `waiting` for delegated children.
- **Effort reporting.** Claude Code does not echo the effort level, so it is recorded as unknown.
- **Checkpoint deadline.** Handoff checkpoints allow 60 s (the research suggested 20 s) because each attempt starts a CLI.
- **Backup size and time.** Every disk archive includes its image (about 390 MB for the desktop image), so a two-bot coordinated capture took about 165 s. Shared image artifacts or incremental archives would reduce it, at the cost of dependency-aware retention.
- **Unverified remote sets.** Remote retention ignores completion records this installation did not verify (for example after restoring onto a new database); such sets must be removed by the owner.
- **Busy computers defer backups.** A long background job or an open desktop defers the coordinated set until it ends.
