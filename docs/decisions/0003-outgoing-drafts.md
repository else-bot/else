# 0003: Outgoing drafts are approved as typed revisions

Status: accepted (2026-11-05).

## Context

Bots send email, X posts, and Telegram messages for the owner through tools they already have: a script run by a computer tool, the guest browser, or host tools. Until now the owner approved a send the way they approve any external action. The bot put the drafted text into an approval card's `context`, and the owner approved the exact tool call. The owner could not edit the text, a follow-up in chat skipped the card, and nothing recorded whether the send actually happened.

The desktop now shows a draft card. The owner edits the text, then approves or discards. A follow-up revision replaces the card. The owner's own clear chat approval also counts. The card collapses to a sent receipt only after a confirmed send. An email card shows its sender, recipients, and subject. A message card shows its platform and, where one exists, its account and destination, and has no addresses or subject.

## Decision

A standalone `outgoing_drafts` table holds one row per revision. `lineage_id` identifies the email, message, or post across revisions. A partial unique index allows at most one revision of a lineage that is not `superseded`. The row stores the typed `content` as JSON tagged by `channel`. The row also has a lifecycle of its own, shared by every channel:

`pending` → `approved` → `sending` → `sent` | `failed` | `uncertain`, plus `discarded` and `superseded`.

- **Content is a tagged enum.** `email` carries `from`, `to`, `cc`, `bcc`, `subject`, and `body`. `message` carries `platform` (`x` or `telegram`), optional `account` and `destination`, and `body`. A Telegram message requires a destination chat. An X post does not need one. The bot drafts with `draft_email` or `draft_message`, and each parses its own channel. Adding a platform means adding a `Platform` variant, without copying the lifecycle.
- **Edits.** Only `pending` is editable. The owner may change the body, and the subject of an email. A `subject` edit on a message is refused (`invalid_draft`). Each owner edit bumps `version`. Approve requests and message attachments name the version the owner saw and carry the final edits in the same transaction. A stale version is refused, so obsolete content can never be approved. A trigger refuses any in-place update that changes more than `subject` and `body`, so a revision's channel, platform, sender, recipients, and destination are fixed.
- **Revisions keep their channel.** `replaces` must name a draft of the same channel, either email or a message on the same platform. Otherwise `CHANNEL_CHANGED` is returned. A revision may change recipients or destination. It starts `pending` and needs a fresh approval, so no earlier approval covers the change.
- **Button approval.** This reuses the owner-question hold. The pending draft is a `turn_holds` row for the attempt that proposed it. Approve and discard settle the row and continue the waiting turn, and the continuation receives the decision once (`delivered_attempt`).
- **Chat approval is source bound.** `POST /turns` may carry `open_draft`. Acceptance saves the edits and records `context_turn_id` and `context_version` in the same transaction as the owner item. The bot resolves intent, and `confirm_draft_approval` enforces the binding. The draft must be bound to the caller's own request, that request must start from an owner `user` item, and the version must be unchanged since the message. Confirming moves the card under that request (`turn_id`) and records `approved_item_id`. The API does no text matching.
- **One send per approval.** `send_turn_id` names the only request that may send. The send is claimed atomically, either by an external computer tool call naming `approved_draft` (recorded in `delivery` with loggable arguments) or by `start_draft_send` before a host send. That claim replaces the generic approval card for that one call. A second claim is refused.
- **Receipts.** Only `report_draft_send` from the send request can settle a claim. Its result names the medium ("email", "X post", "Telegram message"). `failed` can also release an approval that was never claimed. A trigger handles the request ending: an unreported claim becomes `uncertain` and an unused approval becomes `failed`. Chat approval can retry a `failed` or `uncertain` draft.

## Alternatives rejected

- **Model every channel as email.** Dummy addresses and empty subjects would show on message cards, and an owner edit could add a subject that no platform sends.
- **Shared nullable columns (`subject`, `recipients`, `platform`, `destination`).** Every reader would need to know which combinations are legal. One JSON `content` column, parsed into one Rust enum, makes those combinations unrepresentable.
- **A separate table and tools per channel.** That would copy the approval, claim, and receipt invariants. Those invariants are the hard part, and they do not depend on the medium.
- **A free-form platform string.** The card, receipt, and destination rule need to know the platform. An unknown string would lead to guessing.
- **Reuse `owner_questions` with a draft kind.** Questions are immutable once asked. Their approval binds a hash of a tool call, not editable content. They have no delivery states and are skipped by any newer owner message.
- **Parse drafts out of message prose or approval `context`.** That is lossy, and the bot could change the text between showing and sending.
- **Match affirmative words in the owner's message.** That is brittle in both directions ("don't send it yet, looks good otherwise"). The bot resolves intent. The API binds what may be approved: one request, one version, one owner message.
- **Let chat approval or a draft approval satisfy the generic external gate.** That would turn draft approval into a bypass for any external call. Instead, the bypass covers exactly one claim of one approved revision in one request, and the call is logged on the draft.
- **Fire-and-forget autosave.** It races with Approve and with follow-ups. Edits ride on the request that acts on them. The PUT exists for explicit saves and is guarded by version.
- **Email, X, or Telegram services, credentials, or length limits in the API.** That would be speculative infrastructure. Bots keep sending through their own tools and task secrets.

## Consequences

- The API cannot verify that the claimed call sent exactly the approved text. This limit is the same one host approvals have (0002). The claim, the logged call, and the receipt make a mismatch auditable, and the approval allows one call only.
- A pending draft whose request ended stays editable and discardable. Its Approve button returns `draft_inactive`, and the owner approves it in chat.
- Delegated tasks cannot draft, confirm, or send drafts because they are not the owner conversation. A routine run can draft, and the owner approves it on the card. A routine run cannot confirm, because its item is not an owner message.
- A computer send claims its approval before dispatch. A refused or unavailable computer call still consumes that claim. The bot reports it as failed, and the owner approves a retry in chat. This favors avoiding a duplicate send over automatic retries.
- Platform limits such as post length are not enforced. The platform refuses an over-long post, and the bot reports it as failed.
