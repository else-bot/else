# Restore else.bot on a new server

This is the planned whole-assistant recovery procedure. VM archive and browser-profile restoration passed the [local runtime checks](experiments/bot-vm-verification.md). Encrypted PostgreSQL dump and fresh-database restore passed the [backup fixture](research/backup-and-recovery.md#evidence-and-limits). The complete application procedure remains untested. It requires a completed [backup set](backups.md) and separately recovered [encryption keys](security.md#master-key-and-encrypted-secrets).

1. Install else.bot, PostgreSQL, compatible host `codex` and `claude` CLIs, and a compatible Microsandbox runtime. Include the required database roles and extensions.
2. Provision the recovered `SECRET_KEY` and any older key versions needed by the chosen backup. Provision a private owner API token separately. A new owner token does not require changing application data or the encryption master key.
3. Retrieve the selected backup's completion record and encrypted recovery manifest. Decrypt the manifest and validate its format, set identity, and integrity.
4. Retrieve every database, host provider-state, VM, image, and base artifact referenced by the manifest. Fully decrypt and verify each artifact in private staging before importing it.
5. Restore the PostgreSQL export into a new database, with transactional error handling. Refuse to overwrite the active database.
6. Apply any pending versioned migrations required by the installed application version.
7. Import the image and snapshot artifacts into separate runtime storage. Preserve existing bot environments.
8. Restore supported host provider-state artifacts to their isolated bot locations without overwriting existing state.
9. Point `ELSE_BOTS_DIR` at the restored project directories (`<new data dir>/projects`): restored computers mount them from there, so do not move them afterwards. Validate the recovered configuration and bot files with compute stopped. Preserve each computer's recorded on-demand or explicitly stopped intent and each bot's deletion state.
10. Explicitly select the verified new database and runtime paths, then start else.bot. Startup preserves restored state and creates only missing resources. A missing previously recorded bot computer requires recovery rather than an empty replacement.
11. Reauthenticate provider accounts when necessary. Resume native sessions where compatible, or rebuild context from PostgreSQL. Reconnect clients.

## Startup must preserve existing state

Startup must be safe to repeat, including after an interrupted restore. It should create missing resources and preserve existing data and settings. It must never reset configuration, replace defined bots, or overwrite saved values with defaults. Only explicit, versioned migrations may change existing definitions during startup.

Repeated starts must not create duplicate bots, routines, or other application resources. During implementation, verify the restore flow on a clean server and run startup again to confirm that saved data and settings remain intact.
