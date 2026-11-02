# Implementation handoff

This file records the active task and evidence for the next agent. [TODO.md](../../TODO.md) owns completion status. Follow the [runbook](README.md) when claiming, completing, or handing off work.

## Active work

- Task: POC-005 Durable messages and turn scheduling.
- Owner or session: Claude Code implementation session (first usable POC target, POC-001 through POC-012).
- Changed implementation paths: `apps/api`, `.env.example`, `docs/development.md`, `docs/architecture.md`.
- Next action: add turn/item tables, the send route with request identity, the scheduler, the provider trait with a synthetic adapter, and profile routes.
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
| POC-002 | `feat(api): POC-002 owner authentication and secret provisioning`. `else-api setup` creates `0700` data dir and `0600` `owner-token`/`secret-key` with `create_new`; rerun printed `kept existing` and SHA-256 of both files was unchanged. `serve` before setup exits 1 naming `else-api setup`; malformed `SECRET_KEY` exits 1 without echoing the canary value. Live: no token 401, wrong token 401, valid 200 on `/v1/capabilities`; token never appeared in `RUST_LOG=debug` logs. Unit tests (18) cover duplicated/malformed/Basic/query-string bearer, unknown `/v1` paths 401 before 404, `Origin` 403, foreign `Host` 400, failure 429 with owner still allowed, symlink/permission/dangling-symlink refusal, malformed file preserved, owner token independent of master key, loopback-only bind. SSH: `ssh -L` to localhost was refused (no authorized key; the added `known_hosts` entry was removed); an equivalent local TCP forwarder on port 17999 returned 200, proving the tunnel's different local port is accepted. | Task grants arrive with POC-010. |
| POC-003 | `feat(api): POC-003 database migrations and installation setup`. sqlx 0.9 PostgreSQL, embedded migration `0001_installation_bots.sql` (bots, conversations, singleton installation, provider_profiles, request_receipts). `setup` applies migrations, seeds missing `codex` profile (`gpt-6.1-sol`/`medium`/`priority`, revision 1) and creates main bot `Assistant` once under an advisory lock. `cargo test -p else-api --test installation`: 8 passed on per-test databases (fresh setup, repeat migrate+setup+startup x2 with stable IDs/settings/counts, owner profile change preserved, missing profile reseeded without bots, cleared default preserved, 4 concurrent setups create one bot, CHECK/singleton/unique constraints and transaction rollback, unreachable DB error without password). Manual on disposable `else_manual`: `serve` on migrated empty install exits `installation setup has not completed`; setup x2 kept the same bot ID; `/readyz` 200 then 503 `database_unavailable` after the database was dropped; disposable DB removed. | Application routes in POC-004. |
| POC-004 | `feat(api): POC-004 bot creation, listing, and soft deletion`. `/v1/bots` list/create/show/delete. Create serializes each request ID with an advisory lock and a `request_receipts` row hashed over normalized name and role. Delete commits `status = deleted` plus `deleted_at` and clears a matching `default_bot_id` in one transaction (no work exists yet to leave pending). `cargo test -p else-api --test bots`: 7 passed (201 + `Location`, retry 200 same bot and conversation, conflict 409 with no rows, 6 concurrent retries create 1 bot, validation and unknown fields rejected, duplicate names keep distinct IDs, repeated delete returns identical receipt, tombstones hidden even with `include_deleting` and rows retained, deleting main then last bot leaves null default and empty list across 2 migrate+setup+startup cycles, unknown/malformed IDs 404, every route 401 without or with a wrong token and the bot unaffected). | Deletion becomes `deleting`/`202` when work exists (POC-007). |

Append one concise row when a task completes or a blocked attempt needs a durable record. Include exact check commands or reproducible steps, actual outcomes, changed paths, and a task commit subject. Keep evidence synthetic and free of credentials. Historical rows are evidence, not a second status queue.
