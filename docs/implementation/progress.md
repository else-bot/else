# Implementation handoff

This file records the active task and evidence for the next agent. [TODO.md](../../TODO.md) owns completion status. Follow the [runbook](README.md) when claiming, completing, or handing off work.

## Active work

- Task: none.
- Owner or session: none.
- Changed implementation paths: none.
- Next action: select POC-001 after reading its dependencies and the current checkout.
- Owned running processes or services: none started for implementation.
- Blockers: none recorded.

Replace these fields when starting work. If a task is blocked, name its ID, reproduce the failure, and state what would unblock it. Do not clear a real blocker without recording the resolution. A later agent must inspect local services before assuming none are running.

## Current execution policy

Runtime bots use the saved GPT 6.1 Sol Medium Fast default and must not spawn provider-native sub-agents. See [provider settings and policy](../providers.md#saved-execution-profile). This is a requirement for the pending implementation tasks. No runtime setting has been implemented by this documentation change.

## Evidence log

| Task | Result and evidence | Follow-up |
| --- | --- | --- |
| POC-000 | Completed in commit `ac3d11c`. Two generated Rust binaries, shared workspace/lockfile, Compose PostgreSQL, and setup docs. Compilation, formatting, starter execution, authenticated SQL, and persistence through container recreation passed. | Application behavior has not been implemented. Begin POC-001. |

Append one concise row when a task completes or a blocked attempt needs a durable record. Include exact check commands or reproducible steps, actual outcomes, changed paths, and a task commit subject. Keep evidence synthetic and free of credentials. Historical rows are evidence, not a second status queue.
