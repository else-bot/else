# Email connectors

The desktop has a Settings modal with Server and Email sections. Email is currently an empty state. The connector API and native account setup described below are planned, not implemented.

## Selected experience

Account setup starts with a private connection card in the conversation where the bot requests access. The owner chooses a provider, signs in, and grants the bot its mail actions. New-email wake is one checkbox. There is no watched-folder selector.

Connected accounts belong under Email in app Settings. Settings replaces Change server, opens through the title bar or Cmd/Ctrl+comma, and uses fixed left navigation with scrollable content on the right. Server configuration reuses the existing connection verification and saving path. Saving an unchanged connected profile keeps the current conversation and draft.

## Connector ownership

Build connectors into the Rust API. A shared core owns connection identity, trusted setup definitions, host-only credentials, bot grants, durable event delivery, operation receipts, and connection health. Email adapters own messages, folders, labels, drafts, attachments, and provider-specific behavior. Later messaging connectors reuse the core with their own types.

Each account has one actor that owns tokens, sessions, synchronization cursors, and connection generation. Multiple bots share that connection through independent grants. Bounded messages apply backpressure. Network waits run outside the actor's command handler and return generation-tagged results, so a busy inbox cannot block revocation or unrelated accounts.

Keep protocol-library types private. Use maintained Rust clients for IMAP, SMTP, Gmail, Microsoft Graph, and JMAP. Pimalaya's `io-*` crates are the selected starting point. Their blocking high-level clients must not run on Tokio workers; prove a cancellable asynchronous driver before adopting them in production. The API remains responsible for authorization and durable state.

## Credentials and setup

Connector credentials use a separate encrypted host-only store. Existing `task_secrets` can be injected into guest commands and are unsuitable for mailbox passwords or refresh tokens. Reuse the purpose-separated HKDF and age encryption helpers with a connector-specific purpose and fresh salt. Bind each encrypted payload to its connection, provider, credential kind, and format version.

Only owner-authenticated setup routes accept credentials. Bots receive account identity, connection status, capabilities, and granted actions. Passwords, OAuth codes, and tokens never enter prompts, chat, tool results, logs, guest environments, or command lines. Native inputs are masked and cleared when setup ends.

Use trusted provider definitions for setup forms and authorization URLs. OAuth uses single-use state, PKCE where supported, exact redirect matching, and provider-verified account identity. The desktop flow uses a registered public client and an authenticated code handoff to the API. API-only installations can configure their own registered application. Provider registration and redirect compatibility must be verified before release.

## Event-driven work

A provider notification wakes application code, not a model. Reconcile from the saved cursor, then commit normalized events, cursor progress, and per-bot pending deliveries together. Deduplicate by account and provider identity, including mailbox validity for IMAP. Queue a connector-origin turn only for eligible new incoming mail and an enabled bot binding. A full work queue leaves deliveries pending.

Initial synchronization stays quiet. Reconnect catches up without replaying the historical inbox. Subscription renewal, token refresh, and recovery checks use no model prompts. Connector-origin turns carry external-data provenance and cannot inherit owner privileges or widen access from instructions inside email.

| Provider | Connection | Notifications |
| --- | --- | --- |
| Gmail and Google Workspace | OAuth | Gmail watch through Pub/Sub, or IMAP IDLE with the required scopes |
| Outlook and Microsoft 365 | Delegated OAuth | Graph notifications and delta queries, or OAuth IMAP IDLE where enabled |
| iCloud | App-specific password | IMAP IDLE when advertised by the server |
| Yahoo | App password where available; approved OAuth for supported applications | IMAP IDLE when advertised by the server |
| Other mail providers | Verified IMAP and SMTP configuration | IMAP IDLE, or JMAP EventSource for compatible accounts |

Gmail Pub/Sub can use outbound pull. Graph-native notifications need a supported delivery endpoint or cloud channel. Renew subscriptions and reconcile after missed notifications. An account without push support needs an explicitly disclosed server-side fallback or manual access, never a scheduled empty bot prompt.

Provider contracts are documented by [Gmail](https://developers.google.com/workspace/gmail/api/guides/push), [Microsoft](https://learn.microsoft.com/en-us/graph/change-notifications-overview), [Apple](https://support.apple.com/en-us/102525), and [Yahoo](https://senders.yahooinc.com/developer/documentation/). Protocol clients are available from [Pimalaya](https://github.com/pimalaya/himalaya).

## Mail actions and authorization

Expose typed operations for search, reading, attachments, drafts, new messages, reply, reply-all, forward, send, archive, trash, restore, read status, flags, labels, folders, and moves. Each operation checks the calling task's bot identity and its current account grant. Register mail tools through an exhaustive, deny-by-default permission mapping before adding handlers.

Provider consent and bot permissions are separate. Replies and forwards create drafts; sending is an explicit action covered by the owner's send grant. Read operations do not implicitly mark mail read. Archive, trash, and permanent deletion have distinct provider mappings. Unsupported capabilities return a typed error instead of a destructive approximation.

Persist an action ID and request digest before dispatch. Reusing the same ID returns its receipt; changing its content fails. After a crash or lost response, an already-dispatched send is uncertain until provider evidence resolves it. Never automatically resend an uncertain action. Revocation fences new dispatch claims and cancels listeners; it cannot undo an accepted external effect.

## Backup and release requirements

Store credentials, grants, cursors, deliveries, and action receipts in SQLite so the existing encrypted database backup includes them. Connector writes and dispatch participate in maintenance admission. The master key remains recoverable separately.

Restore accounts passively. Reconcile provider state before owner activation, classify unresolved sends as uncertain, and suppress automatic replay of old pending work. A local database cannot fence another installation holding the same restored credentials.

Before release, verify live provider authentication, cancellable listening, duplicate and reordered notifications, quiet initial sync, queue saturation, missed-event recovery, revocation, MIME and attachment fidelity, uncertain sends, and an encrypted restore with the original installation still running. Native account setup and OAuth distribution approval are also required. The Settings bootstrap does not establish those capabilities.
