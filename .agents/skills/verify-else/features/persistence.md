# Persistence, memory and recovery

## Sub-features

- Reopen bots and conversations after an API restart.
- Preserve categories, profiles, memory trees and summaries in SQLite.
- Produce an encrypted backup and restore rows and bot project files.
- Reconcile interrupted jobs without launching duplicate work.

## How to get to it (user POV)

Restart the API/desktop and reopen the conversation. Back up with
`else-api backup` while the API is stopped, or use its scheduled backups.
Restore into a new installation using the documented restore commands.

## Driving it with verify-else

The default messaging lane restarts a real API and compares its complete
conversation, including stopped work. Run
`verify.py --tests sqlite backup assistant_backup recovery memory_tree memory_summary`
for the existing file-backed database, CLI backup/restore, project-file,
memory and crash-reconciliation checks. These targets assert saved state,
including wrong-key/corruption failures. Keep their per-target logs.

For a native reconnect change, retain the same disposable data while stopping
and relaunching only its API, then inspect the disconnected and reconnected
window as well as the saved transcript. Never restart the installed service.
Use `docs/restore.md` and the gated live tests for real-VM recovery; this fast
runner does not provision a guest image or an S3 account.

## Gotchas

- Fake VM/summarizer boundaries cannot prove real snapshots or model summaries.
- S3 replication is excluded explicitly, including its outage/restart tests.
- Backup files can contain fixture secrets. Keep data disposable and never
  back up the owner's installation for a smoke test.
