# Create, select and delete bots

## Sub-features

- Create a bot by name alone; retry once without duplicating it.
- Select bots in the sidebar with independent transcripts and drafts.
- Delete an ordinary bot while retaining its history.
- Refuse deletion of the main bot.
- Let a bot delete another bot only after the owner approves its card.

## How to get to it (user POV)

Use the New bot icon in the sidebar header or Ctrl+Shift+N. Clicking a
sidebar row opens that bot and focuses its composer; Ctrl+K opens a quick
switcher. Delete with Ctrl+Shift+D, **Delete bot** at the foot of Bot
details, or the row's context menu, then confirm. The main bot is protected.

## Driving it with verify-else

Run the default helper. Its `bots` lane checks `POST /v1/bots`, retries the
same request UUID, verifies `DELETE /v1/bots/{main}` returns `409 main_bot`,
renames the bot and changes its avatar, rejects an empty name, deletes the
bot, and confirms it disappears from `GET /v1/bots` while its database row
remains deleted. `retained-row.json` records that side effect.
Run `verify.py --tests bots bot_deletion cancel` for concurrency, the
bot-initiated approval card, and deletion during work.

For native proof, launch the documented private session. Press
Ctrl+Shift+N and check the **Create bot** dialog: avatar studio, a focused
**Bot name** field, the model picker, and a **Category** select only when a
category exists. Type a name and press Enter. Confirm one selected bot with
a focused composer and a matching ID/name from `--probe native`. Select
another bot and return; check its draft/transcript.

Select the main bot and press Ctrl+Shift+D: no dialog opens, and the
composer area shows "{name} is your main bot and cannot be deleted." Select
an ordinary bot, press Ctrl+Shift+D, capture **Delete {name}?**, click
**Delete bot**, then probe until the bot leaves the listing.
`--path /v1/bots/ID` still returns it with `cleanup.state:"done"`.

## Gotchas

- Selecting a bot should not start a provider turn.
- Deletion can return 202 while cleanup drains; wait for disappearance.
- The delete dialog focuses Cancel, and focus is not drawn. Click
  **Delete bot** rather than guessing the focus with Tab.
- API proof alone does not prove focus, selection, drafts, or dialog behavior.

## Bot details

Ctrl+Shift+B or the panel icon at the top right toggles **Bot details**
(Details and Computer tabs). Click the bot's name in its header to rename
it in place; Enter or blur commits. Confirm the name through
`GET /v1/bots/{id}`. Bots have no description: `PATCH` accepts only
`name`, `avatar`, and `muted`. The panel also shows Usage, Model, Rules,
Routines, Reminders and Triggers. Settings (Ctrl+,) is the app's settings,
not this panel.

## Rules

With `--fixture-provider`, send `[ui:rule]`. The fixture calls
`rules_update`, and the chat shows a one-line **Added rule New Linear
issue** row with **View** and **Undo**. Send `[ui:rule-change]` after that
turn finishes; a **Changed rule** row follows. In Bot details the Rules
section shows a "1 rule" summary row. Click it for the **{bot} rules**
dialog: a **Rules** tab, a **History** tab with the owner's words, a line
diff and **Restore v1**, plus **Copy markdown** and **Ask to edit**. Close
it and click **Undo** on the newest row. `GET /v1/bots/{id}/rules` then
reports version 3 whose newest history entry has `change:"restored"`,
`restored_from:1`, and `author:"owner"`.
`verify.py --tests bot_rules` covers the API side.
