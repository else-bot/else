# Built-in connectors and email

This is a proposed implementation contract. No production connector is implemented by this change. Users need multiple inboxes, normal mail actions, private account setup, and bots that wake only for eligible mail. Contributors need to add other services without rebuilding credential handling and durable delivery.

## Current ownership

The actual database is SQLite WAL through SQLx. Some earlier research documents still describe PostgreSQL. Grounding comes from the current implementation.

| Responsibility | Existing source | Integration |
| --- | --- | --- |
| Single-owner bounded state | `apps/api/src/actor.rs` | Reuse the actor pattern for connection ownership. Do not run permanent network waits inside its command handler. |
| Owner and task authentication | `apps/api/src/http/mod.rs`, `apps/api/src/grants.rs` | Owner starts and completes setup. Bot tools use task grants plus current per-account grants. |
| Tool dispatch | `apps/api/src/http/mcp.rs`, `apps/api/src/app/tools.rs` | Add explicit connector tool classification. Unknown tools currently fall through to `ReadMemory`; replace this with exhaustive deny-by-default classification before adding mail handlers. |
| Durable work admission | `apps/api/src/db/routines.rs`, `apps/api/src/scheduler.rs` | Queue a connector-origin turn transactionally and wake the scheduler after commit. Do not copy routine skip behavior for full queues. |
| Secret-request continuation | `apps/api/src/db/secret_requests.rs` | Reuse the park/resume pattern, with connector metadata only. |
| Existing encrypted secrets | `apps/api/src/db/task_secrets.rs` | Reuse purpose-separated crypto, not its guest-readable records or environment-variable path. |
| Private native forms | `apps/desktop/src/app/secret_card.rs` | Mask secret fields and clear them on submission, cancel, navigation, and disconnect. |
| Backup capture and admission | `apps/api/src/backup/capture.rs`, `apps/api/src/maintenance.rs` | Include connector tables in snapshots. Gate short mutations and external dispatch. Do not hold admission for listener lifetime. |

## Caller experience first

The owner says, "Connect my iCloud inbox so this bot can draft replies to new support messages."

1. The bot calls `connection_request` with provider `icloud`, purpose, and requested actions. This creates a request. It grants no access.
2. The API parks the task and publishes a trusted connection card to the native app.
3. The owner completes the private form or browser sign-in, verifies the account, chooses bot access and whether new email wakes the bot, and enables the connection.
4. The API stores encrypted credentials in its host-only vault. It resumes the bot with a connection ID, account label, capabilities, and granted actions. No password, token, code, or authentication transcript enters the conversation.
5. An account listener receives a wake hint. Code reconciles it, filters and deduplicates actual arrivals, and queues a bot task for matching mail.
6. The bot reads messages and creates drafts through typed mail tools. Sending requires the owner's existing send grant. It does not require asking again when the grant already authorizes the action.

A second bot can receive a separate binding to the same account. Both share one connection and credential lifecycle. Their wake settings and permitted actions remain independent.

## Generic core, typed service modules

The generic core owns connections, setup sessions, credential references, bot bindings, lifecycle, durable event delivery, action receipts, and health. Email owns mail semantics. A future Discord connector reuses the core without representing channels as email folders.

Start with a small built-in registry. A definition contains the stable connector ID, trusted setup schema, capabilities, and adapter factory. Derive the provider picker and capability descriptions from this registry. Do not add a dynamic plugin loader, downloaded executable, arbitrary shell command, or arbitrary provider HTTP passthrough.

Proposed private module ownership follows the domain.

```text
apps/api/src/connectors/
  mod.rs              Connections, setup, grants, event admission, action receipts
  account.rs          Per-connection actor, generation, token/session lifecycle
  credentials.rs      Host-only purpose-separated encrypted vault
  registry.rs         Trusted built-in definitions and capability metadata
  mail/
    mod.rs            Typed mail model, commands, normalization contract
    imap.rs           Shared provider-independent IMAP and SMTP adapter
    gmail.rs          Gmail-native requests and history reconciliation
    microsoft.rs      Graph mail requests and folder delta reconciliation
    jmap.rs           Later native JMAP adapter
```

HTTP, database, and desktop modules follow existing repository conventions. Keep protocol-library types private. There is no universal untyped `execute(JSON)` capability at the bot boundary.

## Data shapes

These signatures describe ownership and outcomes, not compilable application stubs.

```rust
struct ConnectionId(Uuid);
struct BindingId(Uuid);
struct SetupId(Uuid);
struct EventId(Uuid);
struct ActionId(Uuid);
struct ConnectionGeneration(u64);

enum ConnectionState {
    AwaitingOwner(SetupId),
    Verifying,
    Ready,
    Reconnecting,
    NeedsAuthentication,
    Paused,
    Revoked,
    RecoveryRequired,
}

enum SetupMethod {
    OAuth(OAuthSetup),
    AppPassword(PrivateForm),
    ImapSmtp(PrivateServerForm),
}

struct BotBinding {
    id: BindingId,
    bot_id: Uuid,
    connection_id: ConnectionId,
    revision: u64,
    access: ServiceAccess,
    wake_rule: WakeRule,
}

enum ServiceAccess {
    Mail(MailAccess),
}

struct MailAccess {
    read: bool,
    organize: bool,
    draft: bool,
    send: bool,
    trash: bool,
    delete_permanently: bool,
}

enum WatchMode {
    ImapIdle,
    GmailPubSub,
    GraphNotifications,
    JmapEventSource,
    Manual,
    DisclosedServerPolling,
}

enum MailCommand {
    Search(MailQuery),
    Read(MessageId),
    ReadAttachment(AttachmentId),
    ListFolders,
    CreateFolder(FolderName),
    RenameFolder { folder: FolderId, name: FolderName },
    DeleteFolder(FolderId),
    SetFlag { message: MessageId, flag: MailFlag, enabled: bool },
    SetRead { message: MessageId, read: bool },
    Archive(MessageId),
    Move { message: MessageId, destination: FolderId },
    SetLabels { message: MessageId, change: LabelChange },
    Trash(MessageId),
    Restore(MessageId),
    DeletePermanently(MessageId),
    SaveDraft(DraftContent),
    Reply { message: MessageId, mode: ReplyMode, content: ReplyContent },
    Forward { message: MessageId, content: ForwardContent },
    Send(DraftId),
}

enum ActionOutcome {
    Prepared,
    Dispatching,
    Submitted(ProviderReceipt),
    Failed(ActionError),
    Uncertain,
}
```

`Submitted` means the provider accepted submission. It does not prove delivery to a recipient. Bounce and delivery-status notifications are separate incoming events. Reply and forward create drafts by default. A send action is explicit and can follow automatically when already authorized.

Message references must retain service identity. IMAP references include mailbox, UIDVALIDITY, and UID. Graph uses immutable IDs consistently. An RFC Message-ID header is useful for threading and reconciliation but is not a trusted global deduplication key. It can be absent, duplicated, or forged.

Mail tool inputs use a connection ID, typed message or draft IDs, and validated content. Reads are paginated and bounded. Read operations must not mark mail read implicitly. Fetch bodies with IMAP PEEK where appropriate. Headers, MIME body parts, attachment size, and recipient counts have explicit limits.

## Persistence

Add versioned SQLite migrations for these records, with foreign keys and uniqueness constraints.

| Record | Owns |
| --- | --- |
| `connections` | Provider identity, verified account subject, display address, lifecycle, generation, installation activation state |
| `connection_setups` | Owner request, expiring setup state, PKCE/session references, outcome. No secret in browser URL or chat projection |
| `connector_credentials` | Versioned encrypted payload, fresh per-write salt, key purpose, encrypted connection and credential-kind binding |
| `bot_connections` | Per-bot account grant, scope, revision, wake rule, enabled state |
| `connector_sync` | Per-account or per-folder cursor, mailbox validity, initial baseline, subscription reference and renewal time |
| `connector_events` | Normalized stable event identity, bounded metadata, arrival classification, origin |
| `connector_deliveries` | Unique event and binding delivery, admitted turn, acknowledgement, or deferred status |
| `connector_actions` | Stable action ID, canonical request digest, authorized scope/revision, dispatch state, receipt or uncertainty |

Use unique constraints for provider account identity within a connector, event identity within an account, delivery within a binding, and action ID within its authorized namespace. For OAuth, authenticate account identity from the provider issuer and subject/profile after login, rather than trusting the submitted email address. IMAP AUTH establishes the accepted login principal at a TLS-verified server, not ownership of an arbitrary display email address. Key app-password account identity by normalized service host and authenticated username. Treat the display address as owner-asserted and verify permitted SMTP sender identities separately.

A normalized new-arrival identity differs from a provider delivery ID and from a general mailbox change. Label edits, sent mail, a move into another watched folder, and reconnect baseline messages must not all become new inbox arrivals. Specify first-match and folder-entry behavior in the wake rule. Changes caused by the bot do not re-trigger its own arrival handling.

## Account ownership and backpressure

One account actor owns refreshed tokens, sessions, listener intent, cursors, connection generation, and in-flight operation bookkeeping. It serves multiple bot bindings. Mailboxes and action queues are bounded. Independent accounts do not share a coordination lock.

The actor must remain responsive during long IDLE, network requests, and retries. Start cancellable I/O workers with owned immutable request snapshots. Return bounded completion messages tagged with generation. The account actor alone applies their state changes. A stale completion can classify an already-dispatched action but cannot reactivate a revoked connection or commit a stale cursor.

Use bounded account concurrency and explicit connection limits. Reserve control capacity or a separate bounded control channel so revoke and pause can preempt a mail backlog. Do not serialize unrelated accounts through a single global actor doing network I/O. Independent async calls use `tokio::join!`; cursor pages, token replacement, action transitions, and order-sensitive mail operations stay sequential.

## Wake pipeline

Provider notification → authenticated wake hint → account reconciliation → durable event and cursor transaction → per-binding filtering and delivery → eligible connector-origin turn → existing scheduler.

1. Authenticate and validate the external notification before mapping it to a connection. Bound sizes and rates. Gmail push requires verified Pub/Sub origin and expected audience/topic routing. Graph requires subscription identity and secret client-state validation. Lifecycle validation challenges are narrow special cases, not authenticated mail events.
2. Persist accepted hints before acknowledging a delivery, or delay acknowledgement until normalized events are committed. Never acknowledge and then rely only on an in-memory queue. A bounded hint table or a durable dirty-account marker can coalesce hints without losing the need to reconcile.
3. Reconcile from the saved cursor. Compare-and-swap the starting cursor and connection generation, then save every page's normalized events and matching cursor progress in one transaction. A retry sees the same events and uniqueness constraints remove duplicates. A provider timestamp is not a commit cursor.
4. Create a unique delivery per eligible event and enabled binding. Check the account grant, new-email wake checkbox, incoming-message classification, and deduplication in code. The selected release has no watched-folder or sender-filter UI. Content classification can use the model after a real arrival, when requested by the owner.
5. Admit a bounded batch as one turn or a limited number of turns. Persist the mapping before waking the scheduler. Queue saturation defers pending delivery. It does not drop mail or mark it handled.
6. Mark a delivery admitted only in the transaction that creates its turn. Processing completion and action completion are different outcomes. A crash or cancelled turn must not replay an already-submitted send.
7. Restore lost in-memory wake signals by scanning durable pending rows at startup and during normal scheduler maintenance. Listening, refresh, and retry never reserve a model process slot.

Bound synchronization page sizes, attachment downloads, per-account connections, and queued action counts. Retain event IDs and action receipts long enough for crash recovery and the documented replay window. A durable backlog also needs a disk budget. At its threshold, stop advancing the cursor, retain a dirty marker, and report delayed recovery instead of silently discarding arrivals. If provider retention expires before catch-up, report a recovery gap.

Initial setup establishes a baseline with watching disabled, then enables delivery from the chosen activation boundary. Existing mail does not wake the bot by default. Catch up mail received during disconnection. An expired cursor can require a bounded full scan against a saved inventory. If exact recovery cannot distinguish new arrivals, expose delayed/recovery status and avoid replaying the entire historical mailbox into paid tasks.

## Credential boundary

Create a host-only vault namespace. Existing task secrets are unsuitable because `task_secrets::resolve` injects plaintext into guest commands. Neither `computer_secrets` nor a bot-facing credential read tool can see this vault.

Derive encryption through the existing HKDF and age helper with a new purpose such as `else.bot/connectors/credentials/v1`. Bind plaintext to connection ID, provider ID, credential kind, and format version inside the encrypted payload and verify it after decryption. This prevents treating swapped ciphertext rows as another connection's credentials. Rotate the salt on replacement. Redact Debug implementations and use zeroizing buffers for decrypted data where the libraries permit.

Only owner endpoints accept credential submissions. Ordinary list, WebSocket, tool, prompt, memory, export, and audit projections return metadata. The account actor decrypts only while authenticating or refreshing. No plaintext credential enters a bot environment, guest, command line, general log, or model tool result. An OS keyring can help desktop session storage, but it cannot be the only mailbox credential store because headless operation and portable backup must work.

The default desktop distribution uses project-owned, provider-registered public OAuth clients and PKCE, with a desktop loopback callback and an authenticated handoff to the API setup session. The API owns the verifier and token exchange. No confidential client secret is embedded in the app. API-only self-hosted installations can configure their own registered OAuth application through owner-only settings, with any confidential client secret kept in the host vault. Application identity is installation configuration, while account tokens belong to connections. The trusted registry declares supported OAuth modes. Gate 1 must prove approved redirects and scopes for each mode; a provider that cannot support the public-client flow needs its documented confidential alternative. The UI must expose an actionable setup requirement, never pretend OAuth works without registration.

OAuth setup uses unpredictable single-use state, PKCE S256, exact redirect matching, short expiry, and server-side account identity verification. Bind completion to the authenticated owner session and setup ID. Keep client secrets on the API host. Refresh is serialized per account. A refresh token replacement is durable before later requests use the replacement. A crash during provider refresh-token rotation can still require owner reauthentication and must report that honestly.

Trusted local forms are defined by the connector registry. A bot supplies purpose and provider choice, not HTML, credential field definitions, or an authorization URL. Credential submissions never become chat messages. Clear native buffers and setup secrets on completion, cancellation, expiry, navigation, and disconnect.

## Authorization and external data

Provider OAuth consent and bot grants solve different problems. A provider may permit reading the whole mailbox while the owner gives one bot only a support folder. Enforce that restriction on searches, reads, attachments, draft recipients where configured, and every mutation. Recheck current grant revision immediately before dispatch. Delegated tasks inherit only an explicit intersection of allowed account access, not all connections attached to the child bot.

Connector-origin turns are external input. Extend turn provenance explicitly. They default to the explicitly bound mail actions and scoped memory reads. They cannot call `ask_owner` or permission-request tools, request new secrets, delegate, create bots, broaden grants, create routines, or inherit computer access merely because they run in the owner's conversation. Extra computer access needs a separate owner-granted capability. Email bodies and attachments remain untrusted data. Escape HTML, avoid remote-image fetching, and preserve source attribution. Quoted instructions inside email never change tool authority. Reauthentication and health notices are trusted application-generated cards with connection provenance, not model-authored requests derived from email.

## Human-equivalent actions, with explicit semantics

The first email release must cover search, message and thread reading, attachments, drafts, reply, reply-all, forward, new mail, send, archive, trash, restore, read/unread, flags, labels where available, folders, and moves. Unsupported account capabilities return a typed unsupported result. They do not quietly fall back to destructive approximations.

Gmail archive removes INBOX without deleting other labels. IMAP archive moves to the provider's special-use archive mailbox, or a verified owner-selected destination. Missing archive configuration is an actionable setup problem. Trash differs from immediate deletion. Never issue a mailbox-wide EXPUNGE to delete one message unless selected deletion semantics are proven safe, such as supported UID EXPUNGE.

Reply maintains In-Reply-To and References. Reply-all excludes the sending account and handles Reply-To. Forward constructs a new message or attached original according to the command. Attachments, Unicode filenames, multipart text and HTML, sender aliases, Bcc, and Sent-folder behavior need provider tests. SMTP submission and appending a Sent copy are separate effects. Do not create a second Sent copy when the provider already does so.

## Idempotency, revocation, and uncertainty

Record an action receipt before dispatch. Repeated action ID and request digest returns the existing result. The same ID with different content fails. Apply a durable compare-and-swap to claim dispatch. Mark Dispatching before issuing a request. Idempotent setters reconcile to desired state. They must not use toggle semantics.

A cancellation after request dispatch cannot prove the provider did nothing. Loss of response, crash, restore rollback, or token revocation during send produces `Uncertain` unless provider evidence resolves the outcome. Use stable draft identities and provider receipts where available. Do not claim universal exactly-once sending and do not automatically resend an uncertain action. Provide an owner-visible receipt and a resolution path.

Revocation prevents new dispatch claims, increments generation, cancels listeners, and fences queued operations. The durable Dispatching claim is the authorization ordering point. An action claimed before revocation is already in flight, even if the provider has not accepted it yet. Cancellation is best effort and the action may still complete. Revocation cannot unsend a message the provider already accepted. In-flight completions retain their operation result for audit without restoring account access. Pause preserves credentials. Disconnect removes active credentials and best-effort revokes the provider token, with provider errors surfaced safely. Retained historical backups can still contain older encrypted credentials; their removal follows backup retention, and restored accounts begin inactive.

## Backup and restore

The existing database image contains all connector records and encrypted credential values. The backup layer encrypts that image again. The installation master key remains recoverable separately. A backup is not usable without its correct key.

Short connector mutations, token replacement, event cursor commits, and external dispatch enter the maintenance gate. A backup closes admission and drains or durably classifies in-flight actions. Listeners can stay connected but cannot advance durable state while capture is closed. If a webhook cannot be durably accepted during maintenance, it must receive a retryable response. Never return success for a discarded wake hint.

Restore is passive. It restores accounts, grants, cursors, deliveries, and receipts, but does not start sending, renew subscriptions, or wake bots until owner activation after recovery. Saved Dispatching actions become Uncertain. Provider subscriptions are recreated or verified for this installation, and cursors are reconciled before watched delivery resumes.

A restored backup may predate mail already sent by the old installation. A clone can also hold the same tokens while the original runs. Local SQLite leases cannot fence another installation. Require explicit failover activation and a recovery cutoff that suppresses automatic replay of pre-backup pending deliveries/actions. Reconcile them and expose unresolved items for review. If automatic seamless active failover is later required, add a shared external fencing authority. Do not claim that the installation's runtime lock solves split brain.

## Implementation sequence and acceptance gates

1. **Protocol spike.** Compile the pinned crates and exercise real disposable Gmail, Microsoft, Yahoo, and iCloud accounts. Prove cancellable async IDLE or choose `async-imap`. Confirm project-owned public-client and API-only bring-your-own-client registration modes, redirect handoff, token injection, scopes, account discovery, provider capability probes, notification transport, reconnect, and MIME behavior. No product promise before this gate.
2. **Connector core and vault.** Add migrations, trusted registry, host-only credential storage, owner setup routes, current account grants, parked setup continuation, and native connection-card contract. Prove deny-by-default tool mapping, wrong bot and delegated scope denial, connector-origin privilege restriction, ciphertext swapping rejection, redacted outputs, and encrypted backup/restore of credentials.
3. **Event delivery.** Implement one shared IMAP/SMTP adapter and owner-selectable event transports. Prove duplicate and reordered hints, crash between persistence and wake, full queue backpressure, first-sync quiet behavior, catch-up, UIDVALIDITY reset, expired cursors, and no model runs on empty sync or renewals.
4. **Mail actions.** Add read and organize operations, drafts, sending, reply and forward, attachments, and folder semantics. Prove idempotent mutations, no broad expunge, no implicit read marking, stable reply headers, and uncertain-send behavior across connection loss and process crashes.
5. **Native UI.** Implement the selected HTML direction in GPUI with keyboard navigation, focus management, masked inputs, browser handoff, account access controls, health, pause/reconnect/disconnect, and action receipts. Check the real native app on supported platforms.
6. **Recovery and provider completion.** Run a real encrypted backup/restore drill with connector state and a still-running original. Confirm passive startup, explicit activation, no replayed sends, revoked credential behavior, subscription renewal, provider quotas, and offline catch-up. Complete OAuth registrations and distribution checks before public release.

Full live email support remains open until these gates pass. The current artifacts prove design alternatives, prototype interactions, and isolated dependency compatibility only.

## Native settings bootstrap

The selected UI is conversation setup A with a single new-email wake checkbox. Connected accounts belong under Email in app Settings. The first native slice replaces Change server, moves the existing server connection flow into a near-window-sized modal, and adds the Email empty state. Connector API and conversation account-setup cards remain subsequent implementation work.
