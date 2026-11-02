# Desktop and feature contracts for the first release

Research date: November 2, 2026. Status: DESIGNED. This is a proposed client contract, not an implemented API or UI. VERIFIED identifies an inspected primary source or a named local check. UNVERIFIED identifies an integration gap.

Use one GPUI window with a bot sidebar and the selected conversation. Add create and delete dialogs. PostgreSQL owns conversations, Rust owns execution and authorization, and host Codex and Claude adapters own native sessions. Each bot's Microsandbox VM remains a task computer behind scoped tools. Opening a chat does not start its VM.

This preserves the [first-version requirements](../product.md#first-version), [architecture](../architecture.md), [provider continuity](conversation-continuity.md), [owner authentication and task grants](authentication-and-permissions.md), and [startup preservation rules](../restore.md#startup-must-preserve-existing-state). Email, notes, kanban, routines, iOS, push delivery, remote desktop tabs, permissions editors, and bot editing remain later work. This report adds no application implementation.

## Proposed window and dialogs

All behavior in this section is DESIGNED.

| View | Content and behavior |
| --- | --- |
| Messaging window | Sidebar with bot names, a main-bot marker, activity text, and New bot. Conversation header with name, provider picker, Stop work when needed, and Delete. Ordered user and assistant messages, a multiline composer, and Send. |
| Create bot dialog | Name and role text. Create submits once with a stable request ID. On success select the new bot and focus its empty composer. Creation saves metadata and a conversation, without starting a provider or VM. |
| Delete bot dialog | Name the bot and explain that deletion stops its work and removes it from messaging while retaining history, memory, and files. Confirm with Delete bot. A pending cleanup appears as a disabled sidebar row. |
| Empty state | If no active bots remain, show Create bot. If the selected bot disappears, select another active bot or show this state. |
| Connection error | Keep any loaded history and draft visible. Show disconnected, reconnecting, or invalid credential text in the existing window. Development configuration does not need an account or login screen. |

The provider picker configures the next message. Each accepted message retains its chosen provider even if the picker changes later. Show the active turn's actual provider beside its status. While a different provider is working, a new message queues behind it. To switch immediately, use Stop work, wait for confirmed cancellation, then send the follow-up with the new provider. This makes interruption explicit without adding a task panel.

The sidebar subscribes to the `bots` channel on the owner WebSocket and redraws whenever the server pushes a changed list. Only the selected bot has a `bot:<id>` subscription. A one-shot HTTP list read is still allowed after the desktop's own create or delete, before the push arrives. Unselected bots continue running. There is no unread counter or read-receipt protocol in this release.

Enter sends, Shift+Enter inserts a newline, and IME composition never sends accidentally. Keep a separate in-memory draft per bot. Closing the client can discard unsent drafts in this release. While a send outcome is unknown, keep the original draft and request ID as a pending send. Editing creates a separate draft, not a changed retry. Deduplicate the optimistic user bubble by request ID when the server item arrives.

Use selectable, wrapped plain text for the first transcript, including streamed output. Preserve whitespace and permit copying. Rich markdown, attachments, and file browsers can follow. Use stable item IDs, paginate older history, retain scroll position when prepending, and follow new output only while the owner is already at the bottom.

## GPUI feasibility and implementation boundary

VERIFIED from upstream documentation: GPUI provides application windows and views, a text-input example with focus and clipboard actions, and list examples including variable-height rows. Those are the needed building blocks. [GPUI examples](https://github.com/zed-industries/zed/tree/main/crates/gpui/examples), [text input source](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/input.rs), [list source](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/list_example.rs).

VERIFIED from the current upstream README: GPUI is pre-1.0 with breaking changes. Its current setup separates `gpui_platform`, with Linux Wayland and X11 features. macOS requires a text backend such as `font-kit`. These are documented capabilities, not a build result on this machine. [Upstream GPUI README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md).

DESIGNED: pin a matched GPUI revision and platform dependencies when implementation starts. Follow that revision's examples. The upstream homepage and current README show different application initialization forms, so copying snippets across revisions is unsafe. Keep one root view, a sidebar, transcript, composer, and dialogs. An API client and a plain Rust state reducer are sufficient. No navigation framework, plugin system, webview, provider UI modules, or local database is required.

Run HTTP requests and stream parsing away from the GPUI render callback. Deliver normalized updates to the owning UI entity through GPUI's supported async update mechanism at the selected revision. One small Tokio runtime for the HTTP client is a candidate. Batch presentation updates while keeping the last successfully applied event cursor. Changing selection drops only the subscription and fetch tasks for that view. It does not cancel server work.

UNVERIFIED: no GPUI build, window, input method, text selection, accessibility, or async runtime integration ran here. Before claiming desktop feasibility beyond documented capabilities, build one pinned vertical slice on the owner's Linux desktop and test typing, paste, composition, long messages, streaming, scroll retention, keyboard focus, and window close. Cross-platform packaging remains a separate later check.

## Proposed records and public shapes

All field names, paths, limits, and statuses below are DESIGNED. UUIDs identify objects and requests. Provider IDs are opaque server values. Times are UTC RFC 3339 strings. Identifiers and credentials are different types.

The minimum saved bot definition is `id`, `name`, `role`, `conversation_id`, `created_at`, and nullable `deleted_at`. Keep the creation request ID and payload hash for retry handling. Name is 1 to 80 trimmed characters. Role is nonempty text up to 8 KiB in UTF-8. Duplicate names are allowed because IDs identify bots. Role text changes instructions, never permissions.

Create one canonical conversation with the bot in the same transaction. The API also projects existing turn, job, and lifecycle records into activity. No second scheduler state belongs to the desktop.

```json
{
	"id": "<bot-uuid>",
	"name": "Work",
	"role": "Help with my work",
	"conversation_id": "<conversation-uuid>",
	"created_at": "2026-11-02T06:00:00Z",
	"deleted_at": null,
	"selected_provider_id": "<opaque-provider-id-or-null>",
	"activity": {
		"phase": "idle",
		"active_turn_id": null,
		"queued_count": 0,
		"running_job_count": 0,
		"error": null
	},
	"cleanup": null
}
```

`activity.phase` is `idle`, `queued`, `working`, `switching`, `cancelling`, or `unavailable`. A stopped idle VM does not make a bot unavailable. The root task stays working across internal provider attempts and one-hop delegation. Those transitions create no extra visible conversation or task view. Unknown job or provider outcomes can make it unavailable until reconciliation. `cleanup` is null for active bots. Deleted bot receipts contain `state = pending | done`, a retryable error when applicable, and no conversation content.

The capability response contains `api_version`, `server_instance_id`, `default_provider_id`, `providers[]`, and `limits`. Each provider entry has an opaque `id`, `label`, `available`, and nullable `unavailable_reason`. Keep subscription accounts, native session IDs, credentials, CLI options, and model-specific controls inside the API. The desktop renders the returned provider choices and has no `Codex` or `Claude` branches. Disabled choices explain that host configuration or login needs attention. No provider login flow belongs to the initial client.

Conversation snapshots contain `conversation_id`, `bot_id`, `selected_provider_id`, `event_cursor`, `items[]`, `turns[]` for returned items and unfinished turns, activity, and `older_before_seq`. Return the latest 50 items by default. Older pages use `before_seq` and return ascending history order. History item `seq` orders bubbles. It is distinct from the event cursor, which also counts text and status updates.

Each item has `id`, `seq`, `turn_id`, `request_id` for an owner message, `kind = user | assistant | status`, `text`, nullable `provider_id`, and `created_at`. Stable assistant item IDs survive streaming and reconnect. Internal checkpoints and raw tool logs are excluded. Show a short status item when a retained background job needs explanation.

Public turn statuses are `queued`, `preparing`, `sending`, `running`, `cancelling`, `completed`, `interrupted`, `failed`, and `uncertain`. These project the continuity and lifecycle records rather than introducing another execution state machine. Each turn includes a nullable safe error and an effect-certainty flag when the result is unknown. The API must not report success from the appearance of text alone.

## Saved provider profile amendment

The owner-selected starting Codex profile is GPT 6.1 Sol Medium Fast. The [provider settings contract](../providers.md#saved-execution-profile) defines its exact values, persistence, update rules, and per-turn snapshots. Provider-native sub-agents remain disabled independently of model selection.

Add owner-only `GET` and `PATCH /v1/settings/providers/{provider_id}` as operator configuration routes. Keep model editing out of the initial desktop UI. The existing picker still chooses the provider for a new message. Task grants cannot update provider profiles. A profile update does not alter already accepted work or reset the conversation.

## Proposed REST endpoints

All routes require the owner bearer token, including reads and streams. Task grants cannot use this route group. Follow the [auth report](authentication-and-permissions.md) for loopback binding, explicit tunnels, credential storage, and task-scoped MCP authorization.

| Method and path | Request and result |
| --- | --- |
| `GET /v1/capabilities` | Returns the capability shape above. No credentials or native session identifiers. |
| `GET /v1/bots?include_deleting=true` | Returns `default_bot_id` and `bots[]`. Include active bots and deletion receipts whose cleanup remains pending. Completed tombstones stay out of the list. |
| `POST /v1/bots` | `{request_id, name, role}`. `201` with bot and `Location` on first acceptance. Matching retries return `200` with the same ID and current state. |
| `GET /v1/bots/{bot_id}` | Active bot or owner-visible deletion receipt. Allows deletion progress polling without access to retained conversation data. |
| `DELETE /v1/bots/{bot_id}` | No body. Tombstone and revoke before returning. `202` while stopping work, `200` when already stopped. Returns the same deletion receipt on retry. |
| `GET /v1/bots/{bot_id}/conversation?limit=50` | Consistent snapshot and event cursor. `before_seq` fetches older pages. |
| `POST /v1/bots/{bot_id}/turns` | `{request_id, text, provider_id}`. `202` after saving the owner item and queued attempt. Returns canonical item and turn. Matching retries return `200` with their current state. |
| `GET /v1/bots/{bot_id}/turns/by-request/{request_id}` | Resolves a lost send response. `404` means no committed matching request is visible. Retrying the same POST is also safe. |
| `POST /v1/bots/{bot_id}/cancel` | `{request_id}`. Cancels all work accepted for that bot at the cancellation boundary. `202` until termination is confirmed, then `200`. Return `request_id` and `state = pending | done`. |
| `GET /v1/ws` | Owner WebSocket upgrade. Live updates arrive as channels: `bots` and `bot:<bot_id>` after a snapshot cursor. See [live updates](#live-updates-over-websocket-channels). |

Text accepts nonempty input up to a proposed 32 KiB in UTF-8. The API preserves its whitespace. Propose at most four waiting turns per bot, advertised in capabilities. Global resource limits can leave an accepted turn queued. The sidebar explains queued, working, switching, stopping, and unavailable states without a resource-management view.

Owner authentication runs before object lookup. Unknown IDs return `404`. Known deleted bots return `410 bot_deleted` on message and history routes, and a `bot_deleted` error on their event channel. Their management receipt remains readable by the owner. General inaccessible-object behavior remains the auth report's generic `404`.

Use one error shape:

```json
{
	"error": {
		"code": "cancellation_pending",
		"message": "Work is still stopping. Try again when it has stopped.",
		"retryable": true,
		"request_id": "<request-uuid-or-null>"
	}
}
```

Return `400` for invalid input or cursor, `401` for invalid owner authentication, `403` for a valid task grant on an owner operation, `409` for reused IDs with different payloads or cancellation in progress, `413` for size limits, `429` for a full pending queue, and `503` for an unavailable provider or service. No rejected request creates a bubble or changes provider selection. Respect `Retry-After` when present. UI errors offer a relevant retry and retain the draft. Provider failures after acceptance become turn outcomes through snapshots and channel events.

For create and cancel, keep small durable request receipts with the request ID, operation, target bot where relevant, payload hash, and result reference. Turns already retain their request ID and hash. Check and insert under a unique constraint in the same transaction as the mutation. Keep receipts through soft deletion. Do not impose a short retry expiry in the MVP. Cancel retries look up the existing receipt before admitting a new cancellation, so an old retry cannot cancel newer work.

HTTP idempotency only deduplicates acceptance. It does not prove exactly-once provider input or external effects. An uncertain provider attempt never silently resubmits the original action. The desktop displays the uncertainty and permits a new explicit follow-up under a new request ID after server reconciliation.

## Live updates over WebSocket channels

Commands stay on HTTP: create, delete, send, cancel, request lookup, snapshots, capabilities, and settings. They carry request IDs, return status codes, and are safe to retry, which a fire-and-forget socket message is not. Everything the server pushes travels on one authenticated WebSocket per desktop at `GET /v1/ws`, organized as subscribable channels.

### History: SSE and polling were tried first

POC-006 and POC-009 shipped the design this section originally proposed: one SSE stream (`GET /v1/bots/{id}/events` with `Last-Event-ID`) for the selected bot, and sidebar polling of `GET /v1/bots` every three seconds. It worked and passed its acceptance checks. The first usable POC still showed its limits:

- Activity in unselected bots reached the sidebar up to three seconds late, and the polling sent a request every three seconds even while nothing changed.
- Changing the selected bot meant tearing down one HTTP stream and opening another, each with its own reconnect loop.
- Each additional live view (unread state, tabs, a mobile client) would need either another stream or another poller.
- The client needed a hand-written byte-level SSE parser.

The WebSocket keeps the journal, cursors, snapshot rules, and event kinds unchanged and replaces only the transport. A single connection multiplexes channels, subscriptions change without reconnecting, the sidebar is pushed, and frames are already message-delimited. Server wake-ups still come from `pg_notify` with a one-second database fallback; that fallback stays inside the API and never reaches the network.

### Protocol

The upgrade request is authenticated like every `/v1` route: the owner bearer token in the `Authorization` header, no browser `Origin`, and a loopback `Host`. A rejected upgrade returns the ordinary `401`, `403`, or `400` before any socket exists. Tokens never appear in the URL. Messages are JSON text frames.

```text
client → {"op":"subscribe","channel":"bots"}
client → {"op":"subscribe","channel":"bot:<bot-uuid>","cursor":"<server-instance-uuid>:<bot-uuid>:42"}
client → {"op":"unsubscribe","channel":"bot:<bot-uuid>"}

server → {"channel":"bots","event":"bots.snapshot","data":{"default_bot_id":"<bot-uuid>","bots":[…]}}
server → {"channel":"bot:<bot-uuid>","id":"<server-instance-uuid>:<bot-uuid>:43","event":"item.delta","data":{"bot_id":"<bot-uuid>","conversation_id":"<conversation-uuid>","item_id":"<item-uuid>","offset_utf8":6,"text":"world"}}
server → {"channel":"bot:<bot-uuid>","error":{"code":"cursor_reset_required","message":"…"}}
```

`bots` sends the whole list (`include_deleting` semantics) on subscribe and again whenever it changes. Bot creation, deletion, cleanup, and every journaled activity change wake it. `bot:<id>` requires a cursor from a snapshot of that bot and sends committed journal rows after it, in order, each with its encoded cursor as `id`. Subscribing to a channel that is already open replaces it.

A channel error ends only that channel; the connection and other channels continue.

| Code | Meaning and client action |
| --- | --- |
| `cursor_required`, `invalid_cursor` | Missing or malformed cursor, or a cursor for another bot. Reload a snapshot. |
| `cursor_reset_required` | Cursor from an earlier API process or beyond the latest event. Reload a snapshot. |
| `bot_deleted` | The bot was deleted. Data already queued for the channel is dropped, then this error is sent. Refresh the list. |
| `not_found` | Unknown bot. Refresh the list. |
| `unknown_channel`, `invalid_message`, `too_many_subscriptions` | Client bug or more than 32 channels. |
| `unavailable` | The database read failed. Resubscribe from the last cursor later. |

Limits: client messages up to 64 KiB, 32 channels per connection, and a bounded 256-message outgoing queue. A journal read is at most 200 rows; a slow reader blocks only its channel and continues from its cursor, so memory does not grow. The server pings every 15 seconds and closes a connection that is silent for 60 seconds or cannot take a write within 30 seconds.

Persist a small per-bot event journal with `bot_id`, `seq`, `kind`, and `data`. Existing history remains the source for snapshots. This journal provides replay, not a replacement event-sourced database. Retain it with conversation data initially. Do not build a compactor or message broker for the first release.

Generate a nonsecret `server_instance_id` UUID when the API process starts. Encode event cursors as `<server_instance_id>:<bot_id>:<seq>`, binding them to the selected bot and current server process. Snapshots return this cursor. A different instance requires a fresh snapshot even when its numeric sequence matches. This handles API restarts and restored database timelines without another durable epoch record. The client compares sequence numbers only after validating the prefix.

Mutations lock the bot row, update history or activity, increment that row's `last_event_seq`, and insert journal events in one short transaction. Publish only committed events. Do not allocate this cursor from a global PostgreSQL sequence, because allocation alone does not establish commit order. Provider output can be combined for at most 100 ms or 2 KiB before persistence, at valid UTF-8 boundaries. Flush remaining text and the terminal status together. The UI never receives speculative text that the API has not saved.

```text
read_snapshot(bot):
	begin read-only repeatable-read transaction
	read current bot state, last_event_seq, newest items, and unfinished turns
	commit
	return state and event_cursor encoded with current server instance and bot

channel(bot, after):
	reject a missing, malformed, other-bot, other-instance, or future cursor
	loop:
		read committed journal rows where seq > after, in ascending order
		recheck that the bot is active; otherwise send bot_deleted and stop
		send each row with its encoded cursor as id; advance local after
		wait for a wake hint or a one-second fallback timer
```

DESIGNED: the repeatable-read snapshot keeps message contents and its cursor at one database boundary. PostgreSQL documents that successive reads in repeatable read see the same transaction snapshot. Actual query and lock correctness remains UNVERIFIED. [PostgreSQL isolation](https://www.postgresql.org/docs/18/transaction-iso.html#XACT-REPEATABLE-READ).

Use these event kinds. Each JSON payload includes `bot_id` and `conversation_id` where relevant.

| Event | Payload and client action |
| --- | --- |
| `item.upsert` | Complete item. Insert or replace by stable ID. Used for the initial user item and assistant item. |
| `item.delta` | `{item_id, offset_utf8, text}`. Append only when the current UTF-8 byte length equals the offset. |
| `turn.updated` | Complete public turn projection, including terminal outcome. |
| `bot.updated` | Current provider selection and activity projection. |

Each event is one WebSocket message, shaped like the `item.delta` example above.

### Client behavior

The client first fetches a snapshot over HTTP and then subscribes after its cursor. Events committed between those operations replay from the journal. Apply only the next contiguous event. Ignore an ID at or below the applied cursor. A gap, unknown event kind, missing item, or offset mismatch drops the subscription and reloads a snapshot. Advance the cursor only after successful application. Older-page responses merge by item ID and history order, without replacing newer loaded text or advancing the live cursor.

When the connection drops, reconnect with jittered exponential backoff from 500 ms to 15 seconds, subscribe to `bots` again, and resubscribe each open bot channel from its last applied cursor. A connection with no traffic for 45 seconds is treated as lost. On `401`, show the credential error and retry only every 30 seconds. After a desktop restart, obtain a fresh snapshot. Restoring a database requires restarting the API, which changes the instance ID and forces every cursor to reset.

Deletion ends the bot's channel at once with `bot_deleted`, and nothing more is delivered for it. A disconnect, unsubscribe, selection change, or window close never cancels the bot's work or releases a guest job lease.

## Proposed cancellation, deletion, and main-bot behavior

DESIGNED: Stop work is bot-wide for the initial UI. Under the same per-bot dispatch ordering as deletion, save a cancellation receipt, block new sends until cancellation completes, revoke current task grants, cancel waiting turns, interrupt the provider, and cancel all jobs already accepted for this bot. This includes explicit background jobs. It is stronger than interrupting provider generation alone. It does not permanently disable future messaging or request destruction of the VM's saved files.

Return pending while process or job termination is unconfirmed. A timeout remains pending with an actionable error and resource reservations intact. The API's scheduler reconciles it after restart. An action can already have happened before cancellation. Preserve uncertain effect status instead of claiming cancellation undid the action. Repeating a completed cancellation request returns its original receipt and leaves subsequently accepted work alone.

Provider switching without Stop work preserves existing guest jobs, as described by [conversation continuity](conversation-continuity.md#switch-and-resume-algorithm). The UI's stronger Stop work action deliberately cancels them. This distinction avoids inheriting conflicting meanings for a generic Cancel button. A future per-turn interruption control can have narrower semantics.

DESIGNED: Delete commits `deleted_at` and revokes grants before acknowledging. It rejects new turns, tool dispatch, handoffs, and ordinary history reads. It cancels all provider work, queued messages, jobs, and active streams, then stops the VM. Pending or failed shutdown remains durable and retries toward the same stopped state. A late VM start must be stopped. A stale provider result cannot append messages or reactivate the bot.

Keep history, memory, guest files, browser profiles, and host native session files. Keep the VM's persistent storage and never reuse its bot or runtime identity. Cleanup means termination and revocation only. There is no purge endpoint, restore button, or automatic deletion of retained data in this release. The later explicit purge must be a separate operation.

DESIGNED: the main bot is a saved default pointer, not an immortal identity. Explicit installation setup creates it once and records that setup completed. Ordinary startup never seeds bots. Allow its deletion through the same dialog, with the additional sentence, "This removes your default assistant." Clear `default_bot_id` atomically with the tombstone. Do not silently select a new main bot, transfer memory, or recreate the old one. Existing bots remain ordinary bots. If none remain, show Create bot. A new bot remains ordinary unless a future explicit action makes it the default.

Client selection after reconnect prefers the last selected active bot, then the saved default if active, then the oldest active bot with ID as a tie-breaker. This selection is navigation only. It never changes the saved default. Missing setup state or an inconsistent default produces a setup or recovery error instead of modifying an existing installation.

## Proposed acceptance cases and evidence

All application acceptance cases are DESIGNED and remain UNVERIFIED against an actual application.

| Case | Required observation |
| --- | --- |
| Create and retry | A lost response followed by the same request creates one bot and one conversation. Changed payload under the same ID returns `409`. No VM starts. |
| Multiple bots | Select A while B works. B continues, its status refreshes in the sidebar, and reopening B restores committed history. Drafts remain separate. |
| Send and reconnect | Double submission and a lost response yield one owner bubble and one queued turn. Snapshots and replay yield the same assistant text. |
| Provider switch | Send with provider A, then B, then A in the same conversation. Each turn retains its requested provider. Native sessions and summaries remain server concerns. Missing native history rebuilds from saved context. |
| Active switch | Stop work, observe pending until provider and guest jobs stop, then send a follow-up to another provider. Partial output stays marked interrupted. |
| Queue and resource limits | Waiting messages remain ordered. A full queue gives a retryable error without accepting another message. Another bot can work when host capacity permits. |
| Error and unknown result | Missing host login, provider failure, or uncertain delivery preserves visible history and identifies the affected turn. Reconnect never silently repeats an external action. |
| Delete while starting | Deletion revokes immediately. A late VM start stops. New tools and messages fail. Pending shutdown remains visible without an operations screen. |
| Delete retention and restart | History, memory, session files, and guest files survive. The bot stays deleted after repeated startup. Cleanup retries never purge retained data. |
| Delete main or last bot | The saved default becomes null, other bots remain, and an empty installation shows Create bot. Startup and rerun setup do not recreate the deleted main bot. |
| Stream and auth | Fragmented UTF-8, duplicate events, snapshot-connect races, token replacement, and deletion do not leak text or create duplicate bubbles. Unauthorized callers cannot subscribe. |
| Desktop input | Paste, composition, selection, keyboard navigation, long transcripts, and scrolling work on the pinned target desktop. Closing the window leaves server work running. |

VERIFIED locally: `python3 docs/experiments/desktop/check.py` passes 12 synthetic cases. [Fixture](../experiments/desktop/check.py), [captured summary](../experiments/desktop/results.json). The model covers create and send retries, provider binding, snapshot-connect ordering, duplicate replay, UTF-8 offsets, gap reset, frame fragmentation at every byte boundary, incomplete frames, server-instance reset, independent bot work, cancellation retries, retained data, and main-bot startup behavior.

The fixture uses only Python's standard library and fixed synthetic values. It starts no service, provider, VM, browser, or network connection. Its SSE parser accepts only its generated LF frames. It is not a production SSE parser or a test of general SSE compliance. Its sequential state model does not prove concurrent Rust dispatch ordering, PostgreSQL snapshots, token enforcement, real process cancellation, VM shutdown, native session handoff, or GPUI behavior.

Implementation gates are bounded. Prove one pinned GPUI conversation view against synthetic local HTTP responses. Exercise real PostgreSQL acceptance, snapshot, and replay transactions across concurrent writes and restarts. Then exercise the provider and VM adapters' already identified cancellation and continuity cases. These contracts need no mobile research or broader product framework before that vertical slice.
