# 0002: Owner questions are their own records

Status: accepted (2026-11-04).

## Context

A bot sometimes needs the owner to decide something before it can go on: pick one of a few options, or approve an action that deletes, overwrites, or reaches other people. Today the only owner-answered request is `request_secret`: a persisted `secret_requests` row parks the attempt, the turn waits, the owner answers through the owner API, and the same turn continues. Questions and approvals need the same lifecycle, shown as native cards in the desktop app.

Two structures were compared:

1. **Generalize `secret_requests`** into a request table with a `kind` column (`secret`, `choice`, `destructive`, `external`) and nullable columns for each kind.
2. **A standalone `owner_questions` table** with the same lifecycle (pending, park, wait, settle, deliver, end-of-turn withdrawal) but its own columns: title, optional context, ordered choices, the bound action, and the chosen answer.

## Decision

Standalone records. Secret requests exist to keep values out of every row, event, packet, and response; they have a private form, an env allowlist, and a `SecretJson` extractor. A question's whole point is to show text to the owner and record which choice they made. Mixing them puts "never shown" and "always shown" data in one row, with checks that depend on `kind`. Two tables keep each invariant local.

The shared lifecycle is shared in SQL, not in the row: whatever ends a turn withdraws its pending questions (a trigger, like secret requests), and the turn_holds view defines pending work and undelivered outcomes for delegations, secrets, and questions. Scheduling and settlement read that single definition.

### The record

`owner_questions`: `kind` is `choice`, `destructive`, or `external`. `title` and optional `context` are what the card shows. `choices` is an ordered list of `{id, label}`, empty for open questions answered in chat. The application models each approval as a typed action paired with its argument hash. `action` records what an approval is for (a tool name and its exact arguments, or a host action the bot described); `action_hash` binds tool approvals to those arguments. `status` is `pending`, `answered`, `skipped` (a newer owner message replaced it; it was not permission), or `cancelled` (the turn ended for another reason). `answer` is the chosen id. `consumed_attempt` records the one tool call that used an approval, or the continuation that received an approved host action. Repeating a host permission request after delivery needs a new approval.

`bot_permissions` holds the only standing grant: `destructive` for one bot, given by choosing "Always allow" on a destructive card and revocable through the owner API. External actions never get a standing grant.

### Enforcement

The API cannot classify an arbitrary shell command, browser click, or desktop input. Side-effecting computer tools therefore take an explicit `effect` declaration (`local`, `destructive`, `external`). A missing declaration is refused, never treated as local. A `destructive` or `external` declaration creates an approval question bound to the exact call and parks the attempt; after the owner approves, exactly the same call runs once and consumes the approval. Stored action metadata redacts typed text; the hash still covers the original arguments. A different call, a second call, or a call in another turn needs its own approval. Provider-native host tools (shell and files on the owner's machine) run outside the API, so for them the bot asks with `ask_permission` before acting; the API records the approval and the instructions require it, but cannot verify what the host tool then did.

An owner message sent while a question is pending supersedes the turn and marks the question `skipped` before the turn ends. It grants nothing; the owner can approve explicitly in their next message, which the bot turns into a new question.

## Consequences

- Delegated tasks have no owner conversation: they cannot ask, and must not take destructive or external actions unless the bot holds the standing destructive grant.
- The model still classifies its own actions. The API enforces the declared class and the one-shot binding, not the semantics of opaque commands.
