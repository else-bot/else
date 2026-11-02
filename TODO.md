# POC implementation backlog

This is the execution queue for else.bot. Read the [agent runbook](docs/implementation/README.md) and [latest handoff](docs/implementation/progress.md) before taking a task. The task files define dependencies and acceptance checks. These checkboxes are the source of truth for completion.

## Delivery targets

The first usable POC is a GPUI desktop app where one owner can create two bots, chat with each through host Codex, restart the API and desktop, and recover the same conversations. The owner can stop work and delete a bot without losing retained data or recreating it on startup.

The assistant POC extends that checkpoint with persistent memory, delegation, isolated task computers, provider switching, and encrypted recovery. It keeps the same small messaging UI.

Both targets disable provider-native sub-agents. App-managed delegation between registered bots remains a separate API operation. The initial saved Codex profile is GPT 6.1 Sol Medium Fast. See the [provider settings and policy](docs/providers.md#saved-execution-profile).

Use the first usable POC as the default target. If the implementation request includes the full assistant POC, continue through the second milestone without a new checkpoint approval. Stop when the assigned target's acceptance checks pass.

## Completed setup

- [x] POC-000: Rust workspace, API and desktop starters, PostgreSQL Compose bootstrap, and development instructions. Committed as `ac3d11c`.

## First usable POC

- [x] [POC-001: API runtime and configuration](docs/implementation/01-api.md#poc-001-api-runtime-and-configuration)
- [x] [POC-002: Owner authentication and secret provisioning](docs/implementation/01-api.md#poc-002-owner-authentication-and-secret-provisioning)
- [x] [POC-003: Database migrations and installation setup](docs/implementation/01-api.md#poc-003-database-migrations-and-installation-setup)
- [x] [POC-004: Bot creation, listing, and soft deletion](docs/implementation/01-api.md#poc-004-bot-creation-listing-and-soft-deletion)
- [x] [POC-005: Durable messages and turn scheduling](docs/implementation/01-api.md#poc-005-durable-messages-and-turn-scheduling)
- [x] [POC-006: Conversation snapshots and event streaming](docs/implementation/01-api.md#poc-006-conversation-snapshots-and-event-streaming)
- [x] [POC-007: Cancellation and restart reconciliation](docs/implementation/01-api.md#poc-007-cancellation-and-restart-reconciliation)
- [x] [POC-008: GPUI window and API connection](docs/implementation/02-desktop-and-codex.md#poc-008-gpui-window-and-api-connection)
- [x] [POC-009: Desktop bot management and messaging](docs/implementation/02-desktop-and-codex.md#poc-009-desktop-bot-management-and-messaging)
- [x] [POC-010: Restricted host provider execution](docs/implementation/02-desktop-and-codex.md#poc-010-restricted-host-provider-execution)
- [x] [POC-011: Codex conversation adapter](docs/implementation/02-desktop-and-codex.md#poc-011-codex-conversation-adapter)
- [x] [POC-012: First usable POC acceptance](docs/implementation/02-desktop-and-codex.md#poc-012-first-usable-poc-acceptance)

## Assistant POC

- [x] [POC-013: Original memory log and retrieval](docs/implementation/03-assistant.md#poc-013-original-memory-log-and-retrieval)
- [x] [POC-014: Deterministic summary tree and saved views](docs/implementation/03-assistant.md#poc-014-deterministic-summary-tree-and-saved-views)
- [x] [POC-015: Model compaction and memory quality](docs/implementation/03-assistant.md#poc-015-model-compaction-and-memory-quality)
- [x] [POC-016: Asynchronous bot delegation](docs/implementation/03-assistant.md#poc-016-asynchronous-bot-delegation)
- [x] [POC-017: On-demand VM lifecycle](docs/implementation/03-assistant.md#poc-017-on-demand-vm-lifecycle)
- [x] [POC-018: Guest tools and desktop environment](docs/implementation/03-assistant.md#poc-018-guest-tools-and-desktop-environment)
- [x] [POC-019: Execution recovery and resource admission](docs/implementation/03-assistant.md#poc-019-execution-recovery-and-resource-admission)
- [ ] [POC-020: Claude adapter and provider switching](docs/implementation/03-assistant.md#poc-020-claude-adapter-and-provider-switching)
- [ ] [POC-021: Encrypted database backup and restore](docs/implementation/04-recovery.md#poc-021-encrypted-database-backup-and-restore)
- [ ] [POC-022: Coordinated assistant backup and restore](docs/implementation/04-recovery.md#poc-022-coordinated-assistant-backup-and-restore)
- [ ] [POC-023: Automatic R2 or S3 replication](docs/implementation/04-recovery.md#poc-023-automatic-r2-or-s3-replication)
- [ ] [POC-024: Assistant POC acceptance](docs/implementation/04-recovery.md#poc-024-assistant-poc-acceptance)

## Outside this backlog

Email, notes, kanban, routines and webhooks, mobile apps, push notifications, rich transcript rendering, remote desktop tabs, multi-user accounts, granular permission editors, provider-native sub-agents, data purge, and key-recovery screens remain later product work. Keep their existing research. Do not add them to complete a POC task.
