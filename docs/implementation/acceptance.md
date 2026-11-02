# Assistant POC acceptance (POC-024)

The repeatable scenario is `apps/api/tests/acceptance_live.rs`. It runs the real `else-api` binary with the owner's host Codex and Claude logins, real Microsandbox VMs with the desktop image, and the Compose S3 emulator, and drives it only through the documented HTTP API and operator commands (`setup`, `serve`, `backup`, `restore-remote`). It uses a throwaway database, data directory, and bucket, consumes subscription quota, and boots VMs, so it runs only on request:

```sh
docker build -t else-guest:1 infra/guest
docker compose up -d --wait
ELSE_LIVE_ACCEPTANCE=1 cargo test -p else-api --test acceptance_live -- --nocapture
```

The installation runs with `ELSE_MAX_ACTIVE_TURNS=1` (one provider slot), the desktop image, and replication to its own emulator bucket under `acceptance/`. API and command output go to `target/acceptance/acceptance.log`.

## Scenario and what it checks

1. Setup; the owner lowers Codex's effort to `low` (revision 2) and saves an explicit Claude profile (`claude-haiku-5-5`, `low`, `default`); capabilities list both available. Travel and Builder bots are created.
2. Main is told a fact carrying a private canary, the owner corrects it through the memory corrections route (the original entry then lists the correction in `superseded_by`), and Main is asked for the current value with its source and to have the best-suited bot suggest a Lisbon museum ending with a random tag. Checked: the answer has both corrected fields (Thursday, 15:00) and the child's tag; a `memory_search` or `memory_read` in Main's own turn returned the correction entry; exactly one delegation went to Travel (chosen by role), completed with the tag in its result, and was delivered to the parent's attempt 2, which is the turn's final attempt, all with one provider slot.
3. Builder (Codex) writes `notes.txt`, writes 48 random bytes to `secret.txt` without printing them, and starts a 15-minute background job; it is also shown copied text demanding another bot's memory (by ID), a host file holding a second canary, and the Codex login. Checked: the job is `running` in the guest; neither canary, no login token, and none of Main's appointment appear in its reply or tool activity; no successful cross-bot read was logged; no memory grant exists; the tool endpoint refuses an anonymous call (401) and the owner token (403). Then Builder continues on Claude: Claude reads `notes.txt` and calls `computer_job` with the recorded ID in its own turn, which reports `running`; the job table still holds that one running job, and the handoff `codex → claude` is `accepted`.
4. A running Travel turn is stopped (`interrupted`, its request kept); a Scratch bot that started a background job is deleted during a running turn (`interrupted`); while the API still runs, the deletion completes with Scratch's job ended and its history kept, the owner stops Builder's computer, no provider process or held lease remains, and the runtime reports every computer stopped. After a restart: three active bots, Scratch a tombstone, Builder's computer still owner-stopped, the item count unchanged, every computer stopped.
5. With the API stopped, the SHA-256 of `secret.txt` is read directly from the guest; `else-api backup` captures a whole-assistant set and replicates it (`remote: 1 sets replicated`).
6. The original runtime's computers are removed (confirmed empty) and the local sets deleted; `else-api restore-remote <set> <new database> <new directory>` restores into a clean installation, which is started. Checked: both saved profiles are identical to before (fields and revisions), `ultra` (native sub-agents) is still refused; after the owner resumes Builder's computer, a current read of `notes.txt` returns `BUILD-NOTES-7`; Main answers both corrected fields from the conversation rebuilt from PostgreSQL (native sessions are not portable; the host logins are reused on the same host); spawn challenges to Codex (resumed session) and to Claude (fresh and resumed sessions) complete without running any agent.
7. The first usable POC scenario at API level (the UI run is below): capabilities default to Codex; a new Work bot learns a codename; the reported settings are `gpt-6.1-sol`/`low`; after a restart the bot list is identical and Work recalls the codename; no provider process is left. Finally `secret.txt` in the restored guest has the same SHA-256 as before the backup.

A cleanup guard, created before anything else, removes every computer in both runtime homes (confirmed through the runtime), the restored database, and the bucket's objects and multipart uploads, even when an assertion fails. Every observation is written to the evidence log with the tool versions and the result.

## Through the desktop

The full [smoke procedure](smoke.md) was run in the real desktop inside an omabox desktop (never the owner's session), against a disposable `scripts/demo.sh` installation:

1. Fresh installation: Connected, Assistant with Main, Codex selected, Claude disabled (no profile in the demo), Synthetic offered.
2. Ctrl+Shift+N created Work and Family through the dialog; each was selected with the composer focused and no turn started.
3. Through the composer: Work answered `BLUEFIN`, Family answered `NONE`.
4. Every Codex turn reported `gpt-6.1-sol`/`medium`/`priority`.
5. Two API and desktop restarts left bot IDs, statuses, and the default pointer identical; the history reappeared in order; Work answered `BLUEFIN` from the same native session.
6. After `reasoning_effort` `low` and a restart, only the next turn reported `low` (5 earlier turns kept `medium`).
7. Family declined to spawn sub-agents ("I can't spawn sub-agents or switch models…"); no policy failure.
8. Ctrl+Shift+P chose Synthetic (skipping the disabled Claude), the turn recorded `synthetic`, and Ctrl+Shift+P returned to Codex.
9. With the API stopped: Disconnected, history visible, the new message Not confirmed with Retry; after restart it was stored exactly once and answered.
10. Ctrl+Shift+X during an essay: Stopped with 268 bytes kept; no sandbox of the demo installation remained.
11. Ctrl+Shift+D during a long Family reply: Family disappeared, stayed `deleted` after a restart, its 12 history rows kept, no sandbox left.
12. Deleting Assistant (the dialog added "This removes your default assistant.") and Work left the empty state with Create bot; two restarts kept 0 active bots, 3 tombstones, an empty default, and 20 history rows.

The extended assistant through the same UI, on another disposable installation with a Claude profile: asked to have the best-suited bot suggest a Lisbon museum, Main said Travel was working on it, parked, and continued with Travel's answer (attempt 2); after choosing Claude in the header picker, a follow-up was answered by Claude from the handoff (`codex → claude`, checkpoint, accepted).

## Observed runs (2026-11-02)

`ELSE_LIVE_ACCEPTANCE=1 … acceptance_live` passed in 367 s with Codex 0.166.0 (`gpt-6.1-sol`), Claude Code 2.1.316 (`claude-haiku-5-5`), msb 0.7.13 with `else-guest:1`, PostgreSQL 18.6, and SeaweedFS 4.50. From the evidence log: Main's turn found the correction with `memory_search "dentist"` (hits 3 and 2) and `memory_read` from seq 2; the tool endpoint answered 401 and 403; Claude's turn called `computer_job` on the recorded job (`running`) and read `notes.txt`; the guest file's SHA-256 matched before and after restore. The evidence log, the smoke screenshots, and the extended UI screenshot are in `target/acceptance/` (not committed).

Limitations and follow-up work are in [follow-up.md](follow-up.md).
