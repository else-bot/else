# Email connector research

Checked on 2026-11-05. This report informs the proposed connector architecture. Source inspection and dependency compilation are separate from live account verification.

## Recommendation

Build the first connector inside the Rust API. Use maintained libraries for email protocols, MIME, and provider requests. Keep durable synchronization, bot access, credential ownership, and external-action receipts in else.bot.

Evaluate Pimalaya's current `io-*` crates before choosing its older `email-lib`. Current Himalaya uses separate Gmail, Microsoft Graph, IMAP, SMTP, and JMAP clients. This is a promising collection of reusable libraries, not a complete durable multi-account bot service.

Do not make a hosted email aggregator or a commercial sidecar mandatory. Either could be an optional future adapter if their maintenance savings justify the operational and privacy costs.

## Provider support

| Provider | Authentication | Receive changes | Mail actions and constraints |
| --- | --- | --- | --- |
| Gmail and Google Workspace | OAuth authorization code with PKCE for a registered application | Gmail watch through Cloud Pub/Sub, followed by history reconciliation. Outbound Pub/Sub pull avoids an incoming HTTP endpoint. IMAP IDLE is an alternative with separate mail scopes and enabled IMAP. | Native labels, drafts, send, trash, read status, attachments, and history. `gmail.modify` includes read, send, and most organization. Immediate permanent deletion needs broader `mail.google.com` access. |
| Microsoft Outlook and Microsoft 365 | Delegated OAuth with account and tenant configuration | Graph change notifications plus per-folder delta queries. HTTPS callback, Event Hubs, or Event Grid infrastructure is needed. OAuth IMAP IDLE is a local alternative where enabled. | Graph mail folders, messages, drafts, attachments, and send. Request immutable IDs consistently. Tenant policy and shared mailbox support require separate verification. |
| Yahoo and AOL | App password where available. Yahoo OAuth requires developer access. | IMAP IDLE when advertised by the actual account server | IMAP operations plus SMTP sending. Do not assume every new account can generate app passwords or that OAuth mail access is granted to an unapproved application. |
| iCloud, including custom domains | App-specific password created in Apple Account settings | IMAP IDLE if advertised and confirmed during connection verification | IMAP and SMTP. Apple documents `imap.mail.me.com:993` and `smtp.mail.me.com:587` with TLS. Use an app-specific password, never ask for the main Apple Account password. |
| Fastmail | App password for IMAP and SMTP, or token and supported OAuth for JMAP | IMAP IDLE or JMAP EventSource | JMAP is a useful later native adapter. Verify the account plan and token scope. |
| Other providers, including Zoho and self-hosted servers | Server-dependent app password or OAuth | IMAP IDLE capability detection | Support explicit IMAP and SMTP settings. Advertise verified capabilities for this account, including send availability and configured archive/trash folders. |
| Proton Mail | Official Bridge credentials | IMAP through a running Bridge | Special case. Bridge is a local dependency and requires a paid Proton Mail plan. Do not advertise direct cloud IMAP support. |

Primary sources include [Gmail push](https://developers.google.com/workspace/gmail/api/guides/push), [Gmail scopes](https://developers.google.com/workspace/gmail/api/auth/scopes), [Graph delivery channels](https://learn.microsoft.com/en-us/graph/change-notifications-overview), [Microsoft IMAP OAuth](https://learn.microsoft.com/en-us/exchange/client-developer/legacy-protocols/how-to-authenticate-an-imap-pop-smtp-application-by-using-oauth/), [Apple mail settings](https://support.apple.com/en-us/102525), [Yahoo developer access](https://senders.yahooinc.com/developer/developer-access/), [Yahoo OAuth documentation](https://senders.yahooinc.com/developer/documentation/), [Fastmail developer documentation](https://www.fastmail.com/dev/), and [Proton Bridge](https://proton.me/support/imap-smtp-and-pop3-setup).

Apple and Yahoo article bodies returned HTTP 429 through the web fetcher. Their indexed first-party excerpts were available. Revalidate exact setup instructions and Yahoo discovery endpoints during the live provider spike. They are not release-tested presets.

## No empty prompts, with honest delivery guarantees

A listening connector is ordinary application code. Renewal, reconnect, reconciliation, and a fallback server request consume no provider-model slot and require no prompt.

Gmail watches expire. Google requires renewal at least every seven days and recommends daily renewal. Notifications contain a mailbox history hint, not the full new message. The provider documents possible delayed or dropped notifications. Add background reconciliation without calling a model. Expired history cursors require a full resynchronization. See [Gmail synchronization](https://developers.google.com/workspace/gmail/api/guides/sync).

Graph Outlook subscriptions expire within seven days. The official subscription table lists 1,440 minutes, under one day, for rich notifications. This was checked through the web tool; the independent reviewer did not re-fetch that page. Recheck returned expiration and account support in the live spike. Prefer basic notifications, renew before the returned expiration, and recover through delta queries. Delta cursors belong to individual folders. See [subscription lifetimes](https://learn.microsoft.com/en-us/graph/api/resources/subscription?view=graph-rest-1.0) and [folder delta queries](https://learn.microsoft.com/en-us/graph/delta-query-messages).

[IMAP IDLE](https://www.rfc-editor.org/rfc/rfc2177) keeps an outbound connection open for changes in a selected mailbox. Renewal and reconnect are expected. Multiple watched folders need an explicit listener strategy and account connection budget. A new-mail hint must cause UID-based reconciliation, never a model call by itself. Bind identifiers to mailbox and UIDVALIDITY. Rebaseline if UIDVALIDITY changes. CONDSTORE and QRESYNC are optional improvements, not universal assumptions.

Accounts without a supported push mechanism can still have manual actions. Automatic watching must report unavailable or use an explicitly disclosed server-side polling fallback. Never silently replace event delivery with a scheduled bot prompt.

A stopped or sleeping API cannot process mail immediately. Catch up after restart. UI status needs to distinguish listening, reconnecting, delayed, reauthentication required, and paused. Do not promise guaranteed real-time delivery.

## Library evaluation

The registry snapshots and repository commit pins are recorded in [sources.json](sources.json), [repository-pins.json](repository-pins.json), and the `sources` directory. Registry versions and current source branches can differ. Pin the actual crate archive before implementation.

| Candidate | Checked version | License | Assessment |
| --- | --- | --- | --- |
| Pimalaya `io-imap` | 0.8.1 | MIT or Apache-2.0 | Promising shared IMAP backend. Advertises IDLE, authentication, UID operations, folder operations, and CONDSTORE/QRESYNC. |
| Pimalaya `io-smtp` | 0.7.1 | MIT or Apache-2.0 | Reusable submission protocol, with externally supplied credentials. |
| Pimalaya `io-gmail` | 0.6.0 | MIT or Apache-2.0 | Native Gmail requests include history, watch/stop, messages, labels, drafts, and sending. Its convenience history watcher polls. Use watch/stop plus our own notification ingestion for push. |
| Pimalaya `io-msgraph` | 0.7.0 | MIT or Apache-2.0 | Mail and attachment operations, drafts, sending, and delta queries. The inspected coverage does not establish complete subscription lifecycle support. We may need a small `reqwest` adapter for subscriptions. |
| Pimalaya `io-jmap` | 0.7.0 | MIT or Apache-2.0 | JMAP mail plus EventSource and push-subscription support. Useful for Fastmail and compatible self-hosted services. |
| Pimalaya `email-lib` | 0.27.0 | MIT | Higher-level multi-backend interface, useful mail composition and operations. Published features cover IMAP, SMTP, Maildir, and related backends, not the current CLI's complete native API coverage. |
| `async-imap` | 0.12.0 | MIT or Apache-2.0 | Async IMAP alternative with Tokio support. A fallback if Pimalaya's async integration is too costly. Does not normalize Gmail and Graph or own durable sync. |
| `imap-next` | 0.3.4 | MIT or Apache-2.0 | Protocol-focused state machines. More application integration work than a mail-level interface. |
| `lettre` | 0.11.23 | MIT | Maintained mail composition and SMTP option. Does not provide inbox access. |
| Stalwart `jmap-client` | 0.4.3 | MIT or Apache-2.0 | Dedicated JMAP alternative. Does not supply universal Gmail, Graph, and IMAP coverage. |
| EmailEngine | Current repository inspected | Commercial source-available | Unified headless REST mail engine with relevant provider coverage. Adds a paid runtime license, Node.js, Redis, and an additional credential/backup boundary. Reject as a required built-in dependency. |
| Hosted aggregator | Aurinko inspected, Nylas documentation partially unavailable | Hosted service terms | Reduces provider-specific integration work, but adds a service dependency and another processor of credentials and email. Keep optional, not the default. |

The Pimalaya `io-*` clients expose I/O-free protocol state machines, but their high-level clients are blocking. Do not call those on a Tokio worker or start a permanently blocking IDLE loop for every inbox on Tokio's shared blocking pool. The implementation spike must show a cancellable Tokio driver over the low-level coroutines, or choose the async alternative. OAuth token creation and refresh remain the application's responsibility. Minimum Rust version in these crate metadata records is 1.89. This environment has Rust 1.98.

The old `email-lib` IMAP watch implementation calls `fetch_all_envelopes` after IDLE notifications or timeouts. It does not replace durable incremental synchronization and is a poor default for large inboxes without measurement. Its Cargo feature wiring also differs between the published registry snapshot and repository branch. Avoid opting into `full`, OS keyring, arbitrary credential commands, or desktop notifications by accident.

Sources are the [current Himalaya manifest](https://github.com/pimalaya/himalaya/blob/8196891f891cf9aa0fb83cd859e1c4c86fa5abdb/Cargo.toml), [io-imap](https://github.com/pimalaya/io-imap), [io-smtp](https://github.com/pimalaya/io-smtp), [io-gmail](https://github.com/pimalaya/io-gmail), [io-msgraph](https://github.com/pimalaya/io-msgraph), [io-jmap](https://github.com/pimalaya/io-jmap), [email-lib API](https://docs.rs/email-lib/0.27.0/email/), [async-imap](https://github.com/chatmail/async-imap), [imap-next](https://github.com/duesee/imap-next), [lettre](https://github.com/lettre/lettre), [jmap-client](https://github.com/stalwartlabs/jmap-client), [EmailEngine](https://github.com/postalsys/emailengine), and [Aurinko](https://www.aurinko.io/email-api/).

## Provider setup is a release dependency

Google requires an OAuth application, consent configuration, production scope verification, and potentially a security assessment for restricted server-side data handling. IMAP does not evade this requirement. Use [Google's restricted-scope guidance](https://developers.google.com/identity/protocols/oauth2/production-readiness/restricted-scope-verification) to determine the actual distribution obligations.

Microsoft needs an application registration with appropriate account types, redirect URIs, delegated mail permissions, and tenant-policy handling. Yahoo OAuth mail access requires approval. User passwords cannot substitute for these registrations.

The proposed default is a project-owned registered public OAuth client for the desktop, using PKCE and an authenticated code handoff to the API. API-only deployments can bring their own registered application through owner settings. Provider acceptance of these registration modes is a gate 1 output, not a result of the dependency probe.

Desktop-local OAuth can use an approved loopback redirect. A remote API needs an exact registered redirect reachable by the browser, or an explicit desktop-assisted code handoff bound to the same setup session and PKCE verifier. No open redirect or client secret embedded in the desktop. Device authorization should only be offered where the provider and chosen scopes support it.

Deployment must choose its event transport explicitly. Local installations can use IMAP IDLE where available. Gmail-native push requires provisioned Pub/Sub and a secure pull credential path. Graph-native push needs supported incoming delivery infrastructure. A future hosted relay must forward verified wake hints only and must not need mailbox tokens or message bodies. No central relay exists today.

## Evidence limits

Source inspection establishes candidate capabilities and known provider constraints. The isolated dependency probe checks crate resolution and compilation, without altering production dependencies. Neither proves live OAuth consent, IDLE delivery on a provider, large-folder behavior, callback verification, MIME fidelity, shared mailboxes, or a full restore. Those remain explicit acceptance gates in the architecture document.
