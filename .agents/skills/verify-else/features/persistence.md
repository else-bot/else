# Persistence, memory and recovery

## Sub-features

- Reopen bots and conversations after an API restart.
- Preserve categories, profiles, memory trees and summaries in SQLite.
- Produce an encrypted backup and restore rows and bot project files.
- Take scheduled backups and change their schedule in Settings → Backups.
- Reconcile interrupted jobs without launching duplicate work.

## How to get to it (user POV)

Restart the API/desktop and reopen the conversation. While the API is
down, the desktop shows "Disconnected — retrying" over the loaded history
and reconnects by itself. Back up with `else-api backup` while the API is
stopped, with **Back up now** in Settings → Backups, or through the daily
schedule configured there. `else-api restore SET_DIR NEW_DATABASE_FILE
[NEW_DATA_DIR]` and `restore-remote` restore only into new paths and do not
need the API stopped.

## Driving it with verify-else

The default messaging lane restarts a real API and compares its complete
conversation, including stopped work. Run
`verify.py --tests sqlite backup backup_settings runtime_lock assistant_backup recovery memory_tree memory_summary`
for the existing file-backed database, CLI backup/restore, backup settings
and schedule, data lock, project-file, memory and crash-reconciliation
checks. These targets assert saved state, including wrong-key/corruption
failures. Keep their per-target logs. Harness APIs run with scheduled
backups off, so only these tests cover the schedule.

For a native reconnect change, `--session` cannot restart its API: the API
binds a fresh port and the session removes its data when the API exits.
Instead, in one box run `else-api setup` and `else-api serve` yourself with
private `ELSE_DATA_DIR` and `ELSE_BOTS_DIR`, a fixed `ELSE_API_BIND`
loopback port, `ELSE_SYNTHETIC_PROVIDER=1`, and
`ELSE_BACKUP_INTERVAL_SECS=0`, and point the desktop at that port. Stop
and relaunch only that API, capture the disconnected and reconnected
window, and compare the saved transcript. Never restart the installed service.
Use the gated live tests for real-VM recovery. This fast runner does not
provision a guest image or an S3 account.

## Gotchas

- A turn that may have reached its provider before a crash reopens as
  `uncertain` with code `api_restarted` and is never replayed; already
  stopped work stays `interrupted`. A turn claimed but never sent is
  queued again, and one still `preparing` becomes `interrupted` (code
  `cancelled`).
- `else-api backup` refuses to run while the API is up ("the API is
  running; it backs up on its schedule"); stop it first or use **Back up
  now** in Settings → Backups.
- Fake VM/summarizer boundaries cannot prove real snapshots or model summaries.
- S3 replication is excluded explicitly, including its outage/restart tests.
- A local backup set shares a `chunks/` store with its siblings; copy the
  whole backup directory, not one set.
- Backup files can contain fixture secrets. Keep data disposable and never
  back up the owner's installation for a smoke test.
