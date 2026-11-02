# Restore else.bot on a new server

This is the planned whole-assistant recovery procedure. VM archive and browser-profile restoration passed the [local runtime checks](experiments/bot-vm-verification.md). The complete application procedure remains untested. It requires a completed [backup set](backups.md) and separately recovered [encryption keys](security.md#master-key-and-encrypted-secrets).

1. Install else.bot, PostgreSQL, and a compatible Microsandbox runtime. Include the required database roles and extensions.
2. Provision the recovered `SECRET_KEY` and any older key versions needed by the chosen backup.
3. Retrieve the latest completed recovery manifest. Decrypt it and verify its authenticity.
4. Retrieve every database, VM, image, and base artifact referenced by the manifest. Decrypt and verify the artifacts.
5. Restore the PostgreSQL export into a new database.
6. Apply any pending versioned migrations required by the installed application version.
7. Import the image and snapshot artifacts.
8. Restore each bot's VM on demand under its resource limits. Preserve the recorded running or stopped state.
9. Start else.bot. Startup reads the restored state and creates only missing application resources.
10. Reconnect clients and provider sessions as needed.

## Startup must preserve existing state

Startup must be safe to repeat, including after an interrupted restore. It should create missing resources and preserve existing data and settings. It must never reset configuration, replace defined bots, or overwrite saved values with defaults. Only explicit, versioned migrations may change existing definitions during startup.

Repeated starts must not create duplicate bots, routines, or other application resources. During implementation, verify the restore flow on a clean server and run startup again to confirm that saved data and settings remain intact.
