# Create, select and delete bots

## Sub-features

- Create a bot by name alone; retry once without duplicating it.
- Select bots in the sidebar with independent transcripts and drafts.
- Delete an ordinary bot while retaining its history.
- Refuse deletion of the main bot.

## How to get to it (user POV)

Use New bot or Ctrl+Shift+N, select a bot in the sidebar, then use its Delete
control or Ctrl+Shift+D and the confirmation dialog. The main bot is protected.

## Driving it with verify-else

Run the default helper. Its `bots` lane checks `POST /v1/bots`, retries the
same request UUID, verifies `DELETE /v1/bots/{main}` returns `409 main_bot`,
deletes the new bot, and confirms it disappears from `GET /v1/bots` while its
database row remains deleted. `retained-row.json` records that side effect.
Run `verify.py --tests bots cancel` for concurrency and deletion during work.

For native proof, launch the documented private session. Use Ctrl+Shift+N,
check that the dialog asks only for a name (plus model and category), fill it and submit. Confirm one selected bot
and a matching ID/name from `--probe native`. Select another bot and return;
check its draft/transcript. Delete through Ctrl+Shift+D, confirm, then probe
again. Capture the creation action and deletion confirmation as well as the
resulting sidebar. Test the mouse controls too when those controls change.

## Gotchas

- Selecting a bot should not start a provider turn.
- Deletion can return 202 while cleanup drains; wait for disappearance.
- API proof alone does not prove focus, selection, drafts, or dialog behavior.

## Settings sidebar

Open Settings for the selected bot. Change its name and description, save,
and confirm both fields through `GET /v1/bots/{id}`. Collapse and reopen the
sidebar without changing the selection. The default `bots` lane checks the
public metadata update and rejects an empty name. Native input proves the
controls and focus.
