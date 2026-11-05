# 0003: Email drafts are approved as typed revisions

Status: accepted (2026-11-05).

## Context

Bots send email for the owner through tools they already have: a script run by a computer tool, the guest browser, or host tools. Until now the owner approved an email the way they approve any external action. The bot put the drafted text into an approval card's `context`, and the owner approved the exact tool call. The owner could not edit the text, a follow-up in chat skipped the card, and nothing recorded whether the email actually went out.

The desktop now shows an email card. The owner edits the subject and body, then approves or discards. A follow-up revision replaces the card. The owner's own clear chat approval also counts. The card collapses to a sent receipt only after a confirmed send.

## Decision

A standalone `email_drafts` table holds one row per revision. `email_id` identifies the email across revisions. A partial unique index allows at most one revision of an email that is not `superseded`. The row stores the typed draft (`sender`, `recipients` JSON, `subject`, `body`) and has a lifecycle of its own:

`pending` → `approved` → `sending` → `sent` | `failed` | `uncertain`, plus `discarded` and `superseded`.

- **Edits.** Only `pending` is editable. Each owner edit bumps `version`. Approve requests and message attachments name the version the owner saw and carry the final edits in the same transaction. A stale version is refused, so obsolete content can never be approved.
- **Button approval.** This reuses the owner-question hold. The pending draft is a `turn_holds` row for the attempt that proposed it. Approve and discard settle the row and continue the waiting turn, and the continuation receives the decision once (`delivered_attempt`).
- **Chat approval is source bound.** `POST /turns` may carry `email_draft`. Acceptance saves the edits and records `context_turn_id` and `context_version` in the same transaction as the owner item. The bot resolves intent, and `confirm_email_approval` enforces the binding. The draft must be bound to the caller's own request, that request must start from an owner `user` item, and the version must be unchanged since the message. Confirming moves the card under that request (`turn_id`) and records `approved_item_id`. The API does no text matching.
- **One send per approval.** `send_turn_id` names the only request that may send. The send is claimed atomically, either by an external computer tool call naming `email_draft` (recorded in `delivery` with loggable arguments) or by `start_email_send` before a host send. That claim replaces the generic approval card for that one call. A second claim is refused.
- **Receipts.** Only `report_email_send` from the send request can settle a claim. `failed` can also release an approval that was never claimed. A trigger handles the request ending: an unreported claim becomes `uncertain` and an unused approval becomes `failed`. Chat approval can retry a `failed` or `uncertain` draft.

## Alternatives rejected

- **Reuse `owner_questions` with an email kind.** Questions are immutable once asked. Their approval binds a hash of a tool call, not editable content. They have no delivery states and are skipped by any newer owner message. Adding edit versions, revisions, and receipts there would make every question invariant depend on `kind`.
- **Parse drafts out of message prose or approval `context`.** That is lossy, and the bot could change the text between showing and sending.
- **Match affirmative words in the owner's message.** That is brittle in both directions ("don't send it yet, looks good otherwise"). The bot resolves intent. The API binds what may be approved: one request, one version, one owner message.
- **Let chat approval or a draft approval satisfy the generic external gate.** That would turn email approval into a bypass for any external call. Instead, the bypass covers exactly one claim of one approved revision in one request, and the call is logged on the draft.
- **Fire-and-forget autosave.** It races with Approve and with follow-ups. Edits ride on the request that acts on them. The PUT exists for explicit saves and is guarded by version.
- **An email service or SMTP credentials in the API.** That would be speculative infrastructure. Bots keep sending through their own tools and task secrets.

## Consequences

- The API cannot verify that the claimed call sent exactly the approved text. This limit is the same one host approvals have (0002). The claim, the logged call, and the receipt make a mismatch auditable, and the approval allows one call only.
- A pending draft whose request ended stays editable and discardable. Its Approve button returns `draft_inactive`, and the owner approves it in chat.
- Delegated tasks cannot draft, confirm, or send drafts because they are not the owner conversation. A routine run can draft, and the owner approves it on the card. A routine run cannot confirm, because its item is not an owner message.

- A computer send claims its approval before dispatch. A refused or unavailable computer call still consumes that claim. The bot reports it as failed, and the owner approves a retry in chat. This favors avoiding a duplicate send over automatic retries.
