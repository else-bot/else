# First usable POC smoke procedure

This procedure exercises the real desktop, API, SQLite, and host Codex on a disposable development session. It uses the owner's Codex subscription for about fifteen short turns, so run it deliberately; routine `cargo test` never does. Messages are synthetic.

## Prerequisites

- `codex login status` reports a login.
- A graphical session for the desktop, started from your own terminal. Keyboard shortcuts are listed in the [development guide](../development.md#run-the-desktop).

The `smoke` session of [`scripts/dev.py`](../development.md#run-isolated-development-sessions) keeps its database, credentials, and bot projects in `.else-dev/smoke`, picks a free port, and never touches the owner's installation or another checkout's session. It enables both Codex (the default) and the labeled Synthetic provider. Every command below takes `--session smoke`; the steps write `dev.py` for `scripts/dev.py ... --session smoke`. `API_PID` is the `pid` that `dev.py status` prints; read it again after each API restart.

## Steps

Run each command from the repository root. Use a second terminal for the desktop.

1. **Fresh installation.** `dev.py reset`, then `dev.py api` and `dev.py desktop`. The connection editor opens first, filled in with this session's API: press **Check connection**, then **Continue**. The window shows Connected and the main bot Assistant with its Main marker. The header offers Codex (selected) and Synthetic (test).
2. **Two bots.** Press Ctrl+Shift+N, create Work ("Help with my work projects"), then Family ("Family logistics: school, appointments, reminders"). Each new bot is selected with the composer focused, and opening it starts no provider.
3. **Separate context through Codex.** In Work send "Synthetic test: my project codename is BLUEFIN. Reply OK." In Family send "Synthetic test: the school is Maple Elementary. Reply OK." Then ask Work "What is my project codename? One word." (BLUEFIN) and Family "Do you know a project codename? Answer NONE if not." (NONE). Replies stream into the transcript.
4. **Profile.** `dev.py sql "select reported_profile from turns where provider_id='codex'"` shows `gpt-6.1-sol`, `medium`, `priority` for every Codex turn.
5. **Restarts.** Stop and start the API and the desktop twice (Ctrl-C, then `dev.py api`; close the window, then `dev.py desktop`). Record `dev.py sql "select id, name, status from bots order by created_at"` and the default pointer before and after; they must match. History reappears in order. Ask Work "Which codename did I give you? One word." (BLUEFIN, from the resumed native session).
6. **Profile update.** `dev.py curl -X PATCH -d '{"reasoning_effort":"low"}' /v1/settings/providers/codex`. Restart the API. The next Codex turn records `low`; earlier turns keep `medium`.
7. **No native sub-agents.** In Family send "Spawn three parallel sub-agents with another model to count to three and merge their answers." The turn completes without a policy failure, and the reply says it cannot.
8. **Provider choice.** (Removed from the header: each bot answers with its own provider and model.)
9. **Failed connection and unknown send.** Stop the API. The sidebar shows Disconnected and history stays visible. Send a message: it shows Not confirmed with Retry. Start the API. After reconnecting the desktop resolves it; exactly one copy is stored.
10. **Stop work.** In Work send "Write a 2000-word essay about the history of tea." When text appears press Ctrl+Shift+X. The turn ends Stopped with its partial text, and `pgrep -c -P "$API_PID" -f "codex app-server"` returns 0 shortly after. Count only this API's children: another checkout's Codex processes are not part of this check.
11. **Deletion during work.** In Family send another long request and press Ctrl+Shift+D, then Enter. Family shows Deleting, then disappears. Restart the API: Family stays deleted, `pgrep -c -P "$API_PID" -f "codex app-server"` returns 0 for the new `API_PID`, and its history rows remain in the database.
12. **Main bot.** The main bot (Assistant) has no Delete button, and Ctrl+Shift+D on it explains that it cannot be deleted; the API answers `409 main_bot`. Other bots delete as before.
13. **Categories (POC-031).** With several bots: Ctrl+Shift+G, type a name, Enter creates a category; New category creates a second. Both show with No bots. Select a bot, type a draft, open its Category button (or Ctrl+Shift+M) and choose a category with the mouse, and another with Tab, Space, and Enter: each bot moves, stays selected, and keeps its draft and Main marker. Move a bot while a turn runs and rename its category: the turn keeps running. Create or move from a second client (`dev.py curl`): the sidebar follows. Delete a category with a bot: the dialog says the bot is kept; it moves to Unassigned. Create a bot with a category chosen in its dialog. A duplicate name (any case) shows an error. Stop the API: groups and selection stay; restart the API and the desktop: the same groups return, without duplicates. Up/Down follow the grouped order.
14. **Clean up.** Stop the desktop and the API, then run `dev.py drop`.

Record the outcomes, the application revision, and any deviation in the [handoff log](progress.md).
