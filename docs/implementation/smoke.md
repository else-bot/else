# First usable POC smoke procedure

This procedure exercises the real desktop, API, PostgreSQL, and host Codex on a disposable installation. It uses the owner's Codex subscription for about fifteen short turns, so run it deliberately; routine `cargo test` never does. Messages are synthetic.

## Prerequisites

- The Compose PostgreSQL is running (`docker compose up -d --wait`).
- `codex login status` reports a login.
- A graphical session for the desktop. Keyboard shortcuts are listed in the [development guide](../development.md#run-the-desktop).

`scripts/demo.sh` keeps everything in the `else_demo` database and `/tmp/else-demo`, marks the database as a demo database, and refuses to touch the owner's database or credentials. It enables both Codex (the default) and the labeled Synthetic provider.

## Steps

Run each command from the repository root. Use a second terminal for the desktop.

1. **Fresh installation.** `scripts/demo.sh reset`, then `scripts/demo.sh api` and `scripts/demo.sh desktop`. The connection editor opens first, filled in with the demo API: press **Check connection**, then **Continue**. The window shows Connected and the main bot Assistant with its Main marker. The header offers Codex (selected) and Synthetic (test).
2. **Two bots.** Press Ctrl+Shift+N, create Work ("Help with my work projects"), then Family ("Family logistics: school, appointments, reminders"). Each new bot is selected with the composer focused, and opening it starts no provider.
3. **Separate context through Codex.** In Work send "Synthetic test: my project codename is BLUEFIN. Reply OK." In Family send "Synthetic test: the school is Maple Elementary. Reply OK." Then ask Work "What is my project codename? One word." (BLUEFIN) and Family "Do you know a project codename? Answer NONE if not." (NONE). Replies stream into the transcript.
4. **Profile.** `scripts/demo.sh sql "select reported_profile from turns where provider_id='codex'"` shows `gpt-6.1-sol`, `medium`, `priority` for every Codex turn.
5. **Restarts.** Stop and start the API and the desktop twice (Ctrl-C, then `scripts/demo.sh api`; close the window, then `scripts/demo.sh desktop`). Record `scripts/demo.sh sql "select id, name, status from bots order by created_at"` and the default pointer before and after; they must match. History reappears in order. Ask Work "Which codename did I give you? One word." (BLUEFIN, from the resumed native session).
6. **Profile update.** `scripts/demo.sh curl -X PATCH -d '{"reasoning_effort":"low"}' http://127.0.0.1:17878/v1/settings/providers/codex`. Restart the API. The next Codex turn records `low`; earlier turns keep `medium`.
7. **No native sub-agents.** In Family send "Spawn three parallel sub-agents with another model to count to three and merge their answers." The turn completes without a policy failure, and the reply says it cannot.
8. **Provider choice.** (Removed from the header: each bot answers with its own provider and model.)
9. **Failed connection and unknown send.** Stop the API. The sidebar shows Disconnected and history stays visible. Send a message: it shows Not confirmed with Retry. Start the API. After reconnecting the desktop resolves it; exactly one copy is stored.
10. **Stop work.** In Work send "Write a 2000-word essay about the history of tea." When text appears press Ctrl+Shift+X. The turn ends Stopped with its partial text, and `pgrep -c -f "codex app-server"` returns 0 shortly after.
11. **Deletion during work.** In Family send another long request and press Ctrl+Shift+D, then Enter. Family shows Deleting, then disappears. Restart the API: Family stays deleted, no `codex app-server` process remains, and its history rows remain in the database.
12. **Main bot.** The main bot (Assistant) has no Delete button, and Ctrl+Shift+D on it explains that it cannot be deleted; the API answers `409 main_bot`. Other bots delete as before.
13. **Categories (POC-031).** With several bots: Ctrl+Shift+G, type a name, Enter creates a category; New category creates a second. Both show with No bots. Select a bot, type a draft, open its Category button (or Ctrl+Shift+M) and choose a category with the mouse, and another with Tab, Space, and Enter: each bot moves, stays selected, and keeps its draft and Main marker. Move a bot while a turn runs and rename its category: the turn keeps running. Create or move from a second client (`scripts/demo.sh curl`): the sidebar follows. Delete a category with a bot: the dialog says the bot is kept; it moves to Unassigned. Create a bot with a category chosen in its dialog. A duplicate name (any case) shows an error. Stop the API: groups and selection stay; restart the API and the desktop: the same groups return, without duplicates. Up/Down follow the grouped order.
14. **Clean up.** Stop the processes and run `scripts/demo.sh drop`.

Record the outcomes, the application revision, and any deviation in the [handoff log](progress.md).
