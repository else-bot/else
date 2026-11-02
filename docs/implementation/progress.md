# Implementation handoff

This file records the active task and evidence for the next agent. [TODO.md](../../TODO.md) owns completion status. Follow the [runbook](README.md) when claiming, completing, or handing off work.

## Active work

- Task: POC-002 Owner authentication and secret provisioning.
- Owner or session: Claude Code implementation session (first usable POC target, POC-001 through POC-012).
- Changed implementation paths: `apps/api`, `.env.example`, `docs/development.md`.
- Next action: add explicit `setup`, the private owner-token file, `SECRET_KEY` validation, and bearer middleware.
- Owned running processes or services: the owner's `else-dev` Compose PostgreSQL was started with `docker compose up -d --wait`; it is preserved.
- Blockers: none recorded.

Replace these fields when starting work. If a task is blocked, name its ID, reproduce the failure, and state what would unblock it. Do not clear a real blocker without recording the resolution. A later agent must inspect local services before assuming none are running.

## Current execution policy

Runtime bots use the saved GPT 6.1 Sol Medium Fast default and must not spawn provider-native sub-agents. See [provider settings and policy](../providers.md#saved-execution-profile). This is a requirement for the pending implementation tasks. No runtime setting has been implemented by this documentation change.

## Evidence log

| Task | Result and evidence | Follow-up |
| --- | --- | --- |
| POC-000 | Completed in commit `ac3d11c`. Two generated Rust binaries, shared workspace/lockfile, Compose PostgreSQL, and setup docs. Compilation, formatting, starter execution, authenticated SQL, and persistence through container recreation passed. | Application behavior has not been implemented. Begin POC-001. |
| POC-001 | `feat(api): POC-001 API runtime and configuration`. Tokio/Axum `else-api serve`, env validation (`ELSE_API_BIND` loopback-only, body limit, shutdown grace), shared JSON error shape, `GET /healthz`. `cargo test -p else-api` 5 passed. Manual: `curl /healthz` returned `200 {"status":"ok"}`; `ELSE_API_BIND=0.0.0.0:7878` and `ELSE_MAX_BODY_BYTES=abc` exit 1 naming the variable; SIGINT exits in 2 ms idle and in 2.0 s with a held half-open request (`ELSE_SHUTDOWN_GRACE_SECS=2`), listener released; a canary bearer header did not appear in logs. | Product routes wait for POC-002 authentication. |

Append one concise row when a task completes or a blocked attempt needs a durable record. Include exact check commands or reproducible steps, actual outcomes, changed paths, and a task commit subject. Keep evidence synthetic and free of credentials. Historical rows are evidence, not a second status queue.
