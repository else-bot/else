# Add a social provider

Social connections belong to the API host. An owner connects a platform bot, and any else.bot assistant may use it while working for the owner. An assistant that should react to incoming messages sets its own trigger for an owner request: one chat or any chat, on mention or on every message, and what to do when woken. Discovery returns public identities, observed chats, capabilities, and the assistant's own triggers. It never returns the platform token.

## Implement the adapter

1. Add the provider to `types::Provider`, its label and limitations, and `Provider::supports`. Keep the typed `CommandKind` policy truthful. Discovery and execution share its permission and approval classification. Unsupported operations must fail before network dispatch.
2. Add an SDK-backed adapter under `providers` and implement `providers::Transport` through `Native`. Keep SDK types, request construction, pagination, and platform-specific permission checks inside the adapter.
3. Normalize identity, destination, message, and incoming-event values into the social types. Preserve remote identifiers as strings in public JSON. Incoming events need a stable identity for deduplication.
4. Use the adapter's cancellation token while receiving events and waiting for capacity. Wait for the receiver's storage acknowledgment before advancing a remote cursor. Let the SDK maintain gateway heartbeats independently of bot execution.
5. Initialize TLS explicitly. Both Rustls crypto backends are present in the dependency graph, so SDK construction must not rely on an implicit backend choice or on an unrelated mail connection initializing it first.
6. Store the SDK client in `providers::Session`. The connection worker shares one session across discovery, commands, sending, and listening for its credential generation, preserving SDK rate limits and connection pools. Replacing credentials retires that session.

The transport is injectable for tests. A fake transport verifies API policy and lifecycle, while a disposable platform account verifies the SDK integration. Keep those results distinct.

## Keep authority in the social domain

Reuse `Manager` for bounded per-connection workers, dispatch, cancellation, and receipt handling. Reuse `store` for encrypted credentials, canonical remote identity, triggers, generation checks, event deduplication, and action receipts. HTTP handlers should not call platform SDKs directly.

Owner management routes accept credentials through `SecretJson`. Task tools authenticate with the current turn's grant and resolve bot identity from the server. The requesting model cannot choose another bot's identity, submit a token, or change another bot's triggers. Only work rooted in an owner message sets a trigger.

Complete a pending setup with its connection in one owner request. A cancelled or settled setup cannot be completed again.

Sending uses the existing approved-draft lifecycle. Other externally visible mutations use an exact-action approval and a durable action ID. A failed response can leave the external result unknown. Return the stored uncertainty and do not repeat the effect with a new identity.

Incoming social turns have restricted grants, read only their triggering destinations, and cannot mutate. Discovery, message reads, and receipt reads must apply that same scope. Pause, credential replacement, pausing or deleting a trigger, Stop work, and bot deletion must invalidate queued work; connection changes also interrupt SDK waits. Restored connections remain passive.

## Add the owner experience

1. Add a new database migration for any provider constraints or stored fields. Do not edit a released migration.
2. Extend the desktop provider representation and token help in `apps/desktop/src/api/socials.rs`. Reuse the inline `SocialsView` for the private credential step and connection management. Triggers appear in each bot's sidebar.
3. Explain whose bot identity the token authenticates, what the API uses it for, and the platform's access limits. Keep credentials out of chat, task-secret storage, discovery, errors, and event metadata.
4. If a new operation is needed, extend the typed command, its schema and capability catalog, and approval classification together. Keep provider tool inventories consistent with MCP discovery.

## Verify the behavior

Extend `apps/api/tests/socials.rs` with provider-specific cases alongside the shared owner/woken-turn isolation, encrypted identity, trigger matching, approval, duplicate action, cancellation, cursor, and passive-restore checks. Verify that an ordinary write grant cannot bypass sending restrictions for an operation that creates a message.

Drive the native setup card and Settings → Socials through the repository's isolated UI workflow. Check private input clearing on submission, navigation, and disconnect, plus stale responses and the connected receipt. Use disposable platform accounts for the final live check.
