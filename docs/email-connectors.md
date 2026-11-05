# Email connectors

Bots use built-in mail tools to read and manage accounts the owner connects. The API watches incoming mail with IMAP IDLE and queues work only when new messages arrive. Mail transport uses Pimalaya's `io-imap` and `io-smtp` crates through cancellable Tokio sockets. No bot routine or empty model turn checks for mail.

## Connect an account

All account setup routes require the owner bearer token under `/v1`. Task tokens cannot call them. Send credentials directly from the private connection UI to these routes. Never include passwords, OAuth codes, refresh tokens, or client secrets in chat, tool arguments, or guest environment variables.

`GET /v1/connectors` returns the email connector definition, provider presets, authentication methods, and permissions. Presets cover Gmail, Microsoft, Yahoo, and iCloud. Custom IMAP/SMTP servers cover other providers. TLS certificate verification is mandatory, including after STARTTLS. There is no plaintext or certificate-bypass setting.

For Gmail and Microsoft, register an OAuth client with the provider and an approved callback URI. Then:

1. `POST /v1/connectors/email/oauth/start` with `provider`, `address`, `client_id`, optional `client_secret`, and `redirect_uri`.
2. Open the returned `authorization_url` in the user's browser.
3. Deliver the callback's `code` and `state`, plus the returned `request_id`, to `POST /v1/connectors/email/oauth/complete`.

The API uses PKCE S256 and single-use, expiring state. It exchanges the code itself, creates the account, encrypts the refresh token, and refreshes access tokens on the host. A restarted API discards unfinished OAuth setup. Callback delivery is explicit; the API does not run an unauthenticated callback server. Redirects require HTTPS or a loopback HTTP address. Production desktop OAuth clients need their own registration and provider approval.

For iCloud and Yahoo, create an app password with the provider. Call `POST /v1/connectors/email/accounts` with `config` and `credential`. Obtain the preset config from `/v1/connectors`, replace `address` and `username`, and choose the correct localized folder names. The credential is `{ "kind": "password", "password": "..." }`. iCloud accepts the full email address as username. Custom accounts require their IMAP and SMTP endpoints, ports, `start_tls` choices, and archive, trash, drafts, and sent folder names.

API-only clients that already completed OAuth can supply `{ "kind": "oauth2", "client_id": "...", "client_secret": null, "refresh_token": "...", "access_token": null, "expires_at": null }` instead. Gmail and Microsoft require OAuth credentials. Yahoo also accepts registered OAuth refresh credentials. The API refreshes only at fixed provider token endpoints.

Account creation starts the connection attempt. Inspect `health` to see connection or authentication failures. A saved account does not mean the provider accepted its credentials. Gmail needs the full mail scope. Gmail and Microsoft preserve Sent copies on the server, so the connector does not append another copy. Microsoft administrators can change that [SMTP copy setting](https://learn.microsoft.com/en-us/powershell/module/exchangepowershell/set-mailbox#-messagecopyforsmtpclientsubmissionenabled). Microsoft tenants must permit IMAP and authenticated SMTP for the mailbox. Custom folder names may need adjustment before creation.

## Manage accounts and permissions

- `GET /v1/connectors/email/accounts` lists account metadata without credentials.
- `GET /v1/connectors/email/accounts/{account_id}` reads one account.
- `PATCH /v1/connectors/email/accounts/{account_id}` with `{ "active": false }` pauses it; `true` resumes it.
- `PUT /v1/connectors/email/accounts/{account_id}/config` updates folder names in the existing `MailConfig`. Account identity and endpoints require a new connection. This supports localized archive, trash, drafts, and sent folders.
- `PUT /v1/connectors/email/accounts/{account_id}/credentials` replaces its write-only credential.
- `DELETE /v1/connectors/email/accounts/{account_id}` disconnects it and erases its credential.
- `PUT /v1/connectors/email/accounts/{account_id}/bots/{bot_id}` sets `{ "read": true, "write": false, "send": false, "wake": true }`.
- `DELETE` on that bot binding revokes its permissions and pending deliveries.

Read is required for write and wake. Sending also requires write. Permissions apply to one bot and one account. `wake` watches new messages in INBOX; there is no watched-folder selector. New bindings apply to future arrivals. Disconnecting an account preserves event and action records needed to explain earlier effects.

## Bot tools

`mail_accounts` lists the accounts this task can access. `mail_action` takes `account_id`, a typed `command`, and an `action_id` UUID for mutations. `mail_action_status` reads a recorded receipt.

Supported command operations are `folders`, `search`, `read`, `attachment`, `set_read`, `set_flagged`, `move`, `archive`, `trash`, `delete`, `draft`, `send`, `reply`, `forward`, `create_folder`, `rename_folder`, and `delete_folder`. `delete` permanently removes a message. Restore a trashed message with `move` back to its original folder.

Message operations use `message: { "folder": "INBOX", "uid_validity": 123, "uid": 456 }` from search results. A changed UIDVALIDITY invalidates old references. Reads use PEEK and do not mark messages as read. Search takes `folder`, an optional text query, and a limit up to 100. Queries are text searches, not raw IMAP commands. Reply and forward save drafts unless `send` is true. Sending a new message takes `draft: { "to": ["recipient@example.com"], "cc": [], "bcc": [], "subject": "Subject", "body": "Message", "attachments": [] }`.

Archive, trash, and move use atomic MOVE when available, or a UIDPLUS-only copy and targeted expunge fallback. Permanent deletion requires UIDPLUS and expunges only the selected UID. Unsupported capabilities return an error rather than expunging unrelated mail. Message and attachment transfers, result bodies, recipient lists, and command queues are bounded.

Outgoing attachments take `name`, `media_type`, and `data_base64`. Up to ten files and 1 MiB of decoded attachment data are accepted within the configured HTTP request limit. Forwarding preserves the original message and its attachments as a message/rfc822 attachment.

Keep the same action ID when retrying the same mutation. The API records `dispatching` before contacting the mail server and returns the saved result for a repeated request. A different command under that ID is rejected. A lost response or interrupted dispatch becomes `uncertain`; it is never automatically sent again. A `pending` response means the queued call is still unresolved; inspect its receipt or retry the same ID. Inspect the mailbox before issuing a new action. This prevents an API retry from silently duplicating a message; it cannot make SMTP and SQLite one transaction.

## Runtime and security

Each account has one actor with a bounded command queue. Its watcher uses separate cancellable sockets, so incoming-mail waits do not block commands. Network operations have deadlines. A quiet IDLE connection renews after 29 minutes and stays connected during bot commands. Connection failures retry with growing delays capped at five minutes, without invoking a model. An IDLE notification triggers reconciliation from the stored UID cursor. Initial connection and a changed UIDVALIDITY establish a quiet baseline.

Cursor advancement, event identities, and per-bot pending deliveries commit together. Delivery admission checks the active account, bot, current binding, provider, and turn queue in one transaction. Full queues keep pending deliveries. Revoked grants cannot regain old deliveries when re-enabled. New arrivals may be grouped into a turn.

Mail-triggered turns are external events, never owner messages. Their grants allow read and mail tools only, restrict memory to their own bot, and restrict mail access to the triggering account. Computer, delegation, routine management, credential requests, and owner questions are unavailable. Native provider host tools are disabled too. Codex loads and verifies a restricted catalog exported by the installed CLI, while its isolated code-mode runner retains the mail tools. Mail content, headers, and attachments remain untrusted data even when an account has send permission.

Credentials use a separate host-only age-encrypted store derived from the installation key, account identity, provider, and credential version. They never enter task secrets or a guest computer. HTTP responses expose metadata only. Tool activity records omit message contents and recipients.

The existing encrypted SQLite backup includes accounts, encrypted credentials, grants, cursors, events, deliveries, and action receipts. Restore pauses accounts, cancels pending deliveries, and marks unfinished actions uncertain. The owner must activate restored accounts; their first scan is quiet. A restored installation cannot fence another copy still running against the same mailbox.

## Contributor boundary

`connectors/types.rs` defines email commands and transport results. `pimalaya` owns protocol and MIME handling. `store` owns encrypted credentials, permissions, cursor transactions, and action receipts. `delivery` admits external events to the scheduler. The account manager owns runtime state and bounded messages. Owner routes and MCP handlers translate requests into those operations.

Future native connectors can register another definition under `/v1/connectors`, add typed commands and transport adapters, and use the same owner-only setup, scoped task permissions, durable event admission, and action-receipt rules. Protocol details and credential handling belong on the host.

Provider references: [Pimalaya IMAP](https://github.com/pimalaya/io-imap), [Pimalaya SMTP](https://github.com/pimalaya/io-smtp), [Gmail OAuth](https://developers.google.com/workspace/gmail/imap/xoauth2-protocol), [Microsoft OAuth](https://learn.microsoft.com/en-us/exchange/client-developer/legacy-protocols/how-to-authenticate-an-imap-pop-smtp-application-by-using-oauth), [iCloud settings](https://support.apple.com/en-us/102525).
