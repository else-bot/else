# API and durable messaging tasks

These tasks establish the server side of the [first usable POC](../../TODO.md#first-usable-poc). Follow the [runbook](README.md). Working area: `apps/api`, Compose configuration when needed, and development documentation.

## POC-001: API runtime and configuration

Depends on: POC-000.

Replace the API's generated starter with an async HTTP process and a small composition entry point. Add validated local configuration, structured errors, bounded request bodies, and graceful shutdown. Keep the listener on loopback. Expose only a minimal liveness response at this stage. Add product routes after POC-002 protects them.

Acceptance:

- Start the API through a documented command and receive a liveness response.
- Invalid configuration exits with a useful error. It does not create or replace saved application data.
- Logs contain no supplied credentials. Interrupting the process releases its listener and exits within a bounded interval.
- Document the added configuration and command in the development guide.

References: [Architecture](../architecture.md), [development setup](../development.md).

## POC-002: Owner authentication and secret provisioning

Depends on: POC-001.

Provision an independent random owner token through an explicit setup operation and read it from a private file. Require bearer authentication for application routes and streams. Read and validate the installation `SECRET_KEY` separately. Keep these credentials out of provider environments and logs. Support a private token file for development without requiring a keyring or account system.

Acceptance:

- A fresh development setup can provision credentials once. Repeating ordinary startup preserves them.
- Missing or malformed credentials fail clearly. Existing credential files are never silently replaced.
- Missing, wrong, malformed, or duplicated bearer headers cannot access a protected route. The valid owner token succeeds.
- Remote plaintext binding is rejected. The API remains usable through the documented local connection or SSH tunnel.
- Tests and documentation distinguish owner authentication, the master key, and future runtime task grants.

References: [Authentication design](../research/authentication-and-permissions.md), [master key](../security.md#master-key-and-encrypted-secrets).

## POC-003: Database migrations and installation setup

Depends on: POC-002.

Connect to PostgreSQL and introduce versioned migrations for installation settings, provider profiles, bots, canonical conversations, and request identities. Extend explicit setup to create the initial main bot and its conversation once. Add turn and event tables in their respective tasks rather than scaffolding every later feature now.

Acceptance:

- Set up a fresh owned test database and verify the initial main bot and saved default pointer.
- Seed the missing Codex profile as model `gpt-6.1-sol`, reasoning `medium`, and service tier `priority`. Saved owner changes survive repeated setup and startup.
- Run migrations and ordinary API startup twice. IDs, settings, and row counts remain stable.
- Ordinary startup does not seed missing default bots. A completed installation with a cleared default is preserved.
- Database failure produces a clear startup or readiness error and no destructive fallback.
- Prove transaction rollback and constraints against actual PostgreSQL. Document database configuration and the migration command.

References: [Desktop records and main-bot semantics](../research/desktop-and-feature-contracts.md), [non-destructive startup](../restore.md#startup-must-preserve-existing-state).

## POC-004: Bot creation, listing, and soft deletion

Depends on: POC-003.

Implement the owner bot routes and create one canonical conversation per bot. Use stable request IDs for creation. Commit a deletion tombstone before returning, retain history, and expose pending shutdown through a deletion receipt. No provider or VM needs to start when creating or opening a bot.

Acceptance:

- Create two bots with roles. A matching retry returns the same bot; changed input under the same request ID conflicts.
- List and select active bots. Duplicate display names do not confuse identity.
- Repeated deletion is safe. Deleted bots disappear from active listings while their records and conversations remain intact.
- Deleting the main or last bot clears the default appropriately. Repeated startup does not recreate it.
- Unknown IDs return the documented error. Knowing a bot ID cannot bypass owner-route authorization.

References: [REST endpoints and soft deletion](../research/desktop-and-feature-contracts.md), [authorization](../research/authentication-and-permissions.md).

## POC-005: Durable messages and turn scheduling

Depends on: POC-004.

Persist owner messages, ordered history, and turn attempts before dispatch. Add the single-process scheduler, capabilities endpoint, and a provider interface with a deterministic synthetic adapter for integration tests. A development demo may expose this adapter only as an explicitly labeled synthetic provider. Save provider choice and a snapshot of the effective model profile per accepted message. Add the owner-only profile read/update routes described in the provider design.

Acceptance:

- A send and its retry produce one owner bubble and one accepted request. Conflicting reuse fails without another message.
- Two bots retain separate histories. Queue order and the configured one-active-turn-per-bot limit hold.
- The synthetic adapter produces persisted assistant output and a terminal outcome through the normal application path.
- A failed acceptance transaction publishes no partial message. Queue overflow returns a bounded, retryable error.
- Sending to a deleted bot fails before accepting a message or starting work.
- Provider-specific events stay behind the adapter. PostgreSQL remains authoritative for visible history.
- Profile updates validate against adapter capabilities and survive restart. Queued/running requests keep their original snapshots; newly accepted requests get the new revision. Task grants cannot update settings.

References: [Continuity records](../research/conversation-continuity.md), [scheduler](../research/execution-lifecycle.md), [message contract](../research/desktop-and-feature-contracts.md).

## POC-006: Conversation snapshots and event streaming

Depends on: POC-005.

Implement paged snapshots and authenticated event streaming using a durable per-bot event cursor. (Delivered first as SSE; live delivery has since moved to WebSocket channels.) Update history and journal entries in the same transaction. Send only committed text. Keep history ordering distinct from streaming-event ordering.

Acceptance:

- A snapshot followed by subscription captures changes committed between the two operations.
- Duplicate replay, reconnect, fragmented UTF-8, and an offset mismatch cannot duplicate or corrupt an assistant bubble.
- Older history loads in order. Invalid or stale cursors trigger a documented snapshot reset.
- Deletion or invalid authorization closes access. The stream exposes no internal checkpoint, credentials, or private reasoning.
- Real PostgreSQL transactions and actual HTTP streaming pass the checks; the research parser alone is insufficient.

References: [live update contract](../research/desktop-and-feature-contracts.md#live-updates-over-websocket-channels).

## POC-007: Cancellation and restart reconciliation

Depends on: POC-006.

Implement the bot-wide Stop work operation, durable cancellation receipts, dispatch fencing, and startup recovery. Exercise these through the synthetic adapter before native processes are introduced. Add a singleton API guard for the owned runtime storage.

Acceptance:

- Stop work cancels accepted work through its recorded boundary and rejects new sends until confirmed. An old cancellation retry cannot cancel a newer turn.
- Deletion during queued or running work prevents late output from reactivating the bot or appending new visible messages.
- Kill the API at queued, sending, and running boundaries. Restart retains history and reconciles the attempt without blindly replaying uncertain delivery.
- A second API targeting the same owned runtime storage cannot dispatch competing work.
- Repeated startup preserves installation state. Cancellation timeout retains uncertainty rather than claiming that effects were undone.

References: [Failure and duplicate-action rules](../research/conversation-continuity.md), [lifecycle and generations](../research/execution-lifecycle.md), [Stop work semantics](../research/desktop-and-feature-contracts.md#proposed-cancellation-deletion-and-main-bot-behavior).
