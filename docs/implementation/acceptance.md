# POC acceptance records

The assistant POC (POC-024) is first; the VM automation and categories milestone (POC-032) follows. Both suites stay runnable.

## Assistant POC acceptance (POC-024)

The repeatable scenario is `apps/api/tests/acceptance_live.rs`. It runs the real `else-api` binary with the owner's host Codex and Claude logins, real Microsandbox VMs with the desktop image, and the Compose S3 emulator, and drives it only through the documented HTTP API and operator commands (`setup`, `serve`, `backup`, `restore-remote`). It uses a throwaway database, data directory, and bucket, consumes subscription quota, and boots VMs, so it runs only on request:

```sh
docker build -t else-guest:1 infra/guest
docker compose up -d --wait
ELSE_LIVE_ACCEPTANCE=1 cargo test -p else-api --test acceptance_live -- --nocapture
```

The installation runs with `ELSE_MAX_ACTIVE_TURNS=1` (one provider slot), the desktop image, and replication to its own emulator bucket under `acceptance/`. API and command output go to `target/acceptance/acceptance.log`.

### Scenario and what it checks

1. Setup; the owner lowers Codex's effort to `low` (revision 2) and saves an explicit Claude profile (`claude-haiku-5-5`, `low`, `default`); capabilities list both available. Travel and Builder bots are created.
2. Main is told a fact carrying a private canary, the owner corrects it through the memory corrections route (the original entry then lists the correction in `superseded_by`), and Main is asked for the current value with its source and to have the best-suited bot suggest a Lisbon museum ending with a random tag. Checked: the answer has both corrected fields (Thursday, 15:00) and the child's tag; a `memory_search` or `memory_read` in Main's own turn returned the correction entry; exactly one delegation went to Travel (chosen by role), completed with the tag in its result, and was delivered to the parent's attempt 2, which is the turn's final attempt, all with one provider slot.
3. Builder (Codex) writes `notes.txt`, writes 48 random bytes to `secret.txt` without printing them, and starts a 15-minute background job; it is also shown copied text demanding another bot's memory (by ID), a host file holding a second canary, and the Codex login. Checked: the job is `running` in the guest; neither canary, no login token, and none of Main's appointment appear in its reply or tool activity; no successful cross-bot read was logged; no memory grant exists; the tool endpoint refuses an anonymous call (401) and the owner token (403). Then Builder continues on Claude: Claude reads `notes.txt` and calls `computer_job` with the recorded ID in its own turn, which reports `running`; the job table still holds that one running job, and the handoff `codex → claude` is `accepted`.
4. A running Travel turn is stopped (`interrupted`, its request kept); a Scratch bot that started a background job is deleted during a running turn (`interrupted`); while the API still runs, the deletion completes with Scratch's job ended and its history kept, the owner stops Builder's computer, no provider process or held lease remains, and the runtime reports every computer stopped. After a restart: three active bots, Scratch a tombstone, Builder's computer still owner-stopped, the item count unchanged, every computer stopped.
5. With the API stopped, the SHA-256 of `secret.txt` is read directly from the guest; `else-api backup` captures a whole-assistant set and replicates it (`remote: 1 sets replicated`).
6. The original runtime's computers are removed (confirmed empty) and the local sets deleted; `else-api restore-remote <set> <new database> <new directory>` restores into a clean installation, which is started. Checked: both saved profiles are identical to before (fields and revisions), `ultra` (native sub-agents) is still refused; after the owner resumes Builder's computer, a current read of `notes.txt` returns `BUILD-NOTES-7`; Main answers both corrected fields from the conversation rebuilt from PostgreSQL (native sessions are not portable; the host logins are reused on the same host); spawn challenges to Codex (resumed session) and to Claude (fresh and resumed sessions) complete without running any agent.
7. The first usable POC scenario at API level (the UI run is below): capabilities default to Codex; a new Work bot learns a codename; the reported settings are `gpt-6.1-sol`/`low`; after a restart the bot list is identical and Work recalls the codename; no provider process is left. Finally `secret.txt` in the restored guest has the same SHA-256 as before the backup.

A cleanup guard, created before anything else, removes every computer in both runtime homes (confirmed through the runtime), the restored database, and the bucket's objects and multipart uploads, even when an assertion fails. Every observation is written to the evidence log with the tool versions and the result.

### Through the desktop

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

### Observed runs (2026-11-02)

`ELSE_LIVE_ACCEPTANCE=1 … acceptance_live` passed in 367 s with Codex 0.166.0 (`gpt-6.1-sol`), Claude Code 2.1.316 (`claude-haiku-5-5`), msb 0.7.13 with `else-guest:1`, PostgreSQL 18.6, and SeaweedFS 4.50. From the evidence log: Main's turn found the correction with `memory_search "dentist"` (hits 3 and 2) and `memory_read` from seq 2; the tool endpoint answered 401 and 403; Claude's turn called `computer_job` on the recorded job (`running`) and read `notes.txt`; the guest file's SHA-256 matched before and after restore. The evidence log, the smoke screenshots, and the extended UI screenshot are in `target/acceptance/` (not committed).

## VM automation and categories acceptance (POC-032)

The repeatable scenario is `apps/api/tests/milestone_live.rs`. Like POC-024 it runs the real `else-api` binary (`setup`, `serve`, `backup`, `restore`) with the owner's host Codex and Claude logins and real Microsandbox VMs from `else-guest:2`, drives it only through the HTTP API and operator commands, and reads guest state directly through `msb exec` only to check results. Content is synthetic: a local mail capture fixture, a local search site, and a randomized local page. It runs only on request:

```sh
docker build -t else-guest:2 infra/guest
docker compose up -d --wait
ELSE_LIVE_MILESTONE=1 cargo test -p else-api --test milestone_live -- --nocapture
```

Its working directory is under `/var/tmp` (on disk: two installations and a 1.3 GB backup set do not fit the `/tmp` tmpfs quota, and the runtime's socket paths must stay short). Output goes to `target/acceptance/milestone.log`.

### Scenario and what it checks

1. Setup; Codex effort `low`, Claude `claude-haiku-5-5`/`low`/`default`. Scripter and Hands are created, categories Automation and Visual are created and each bot assigned.
2. Scripter (Codex) writes a multi-file Python project (`data.py`, `report.py`, `main.py`), runs it, and fetches `report.csv` as an artifact; the reply names the registered artifact ID.
3. In Scripter's guest the capture fixture runs with authentication and drops the acknowledgment for `r07@example.test`; the SMTP settings are stored as the task secret `smtp`. Scripter (Codex) writes a Node.js sender as its own project and sends to r01 through r10 in one run. Checked: exactly ten captured messages, one per recipient, with the right sender and body; the reply reports r07 as uncertain. Asked to make sure everyone has it, Scripter sends nothing more (still ten): no blind retransmission.
4. Scripter (Claude) writes a Python script under `~/work/cdp-search` that uses the guest CDP helper to search "lanterns" on the local site in the visible browser. Checked: the reply contains the result only the page produces, the visible window title (`xwininfo`) is the results page, and the script uses the helper.
5. On Hands (Codex), a local page with a random button position, word, and code is opened in the visible browser. Using only `computer_screenshot` and `computer_input` (every tool call in the turn is one of them), Hands clicks the button, types the requested word, scrolls to the bottom, and reports the random code. Then, outside Chromium, it clicks into an `xterm` and types `echo VISUAL-n > ~/work/visual.txt`; the file holds the random value.
6. Isolation and the host boundary: neither guest sees the other's files; neither has `codex`, `claude`, `msb`, or `docker`; each has its own DevTools browser.
7. Scripter (Codex) starts a 60-second background job; during it, Scripter moves from Automation to Visual; the API is stopped (SIGTERM) and served again. Bot, conversation, and category IDs and the category list are identical, and the job and its lease are still running.
8. The owner stops Hands' computer; a screenshot turn completes with the tool reporting the stop and the runtime still reports Hands `Stopped`. A sub-agent challenge completes without running any agent.
9. After the job completes, a `localStorage` marker is written in Scripter's browser; the tooling check (project files, the mailer's `package.json`, Node version, a fresh venv, an offline `cargo new` build, and the CSV's SHA-256) is recorded; the API stops and `else-api backup` captures a whole-assistant set.
10. The original computers are stopped and `else-api restore <set> <new database> <new directory>` restores into a new installation, which is served. Checked: categories (IDs, names), assignments, conversations, and the main-bot pointer are identical; after one Codex turn the same tooling check gives identical output; the browser shows the marker from the restored profile; the CDP example runs again with a fresh connection (no CDP or input session is carried over).

Finally no turn failed with `policy_violation` or `profile_mismatch`. A cleanup guard removes every computer in both runtime homes and drops the restored database even when an assertion fails; the temporary directories go with it.

### Through the desktop

In an omabox desktop (never the owner's session), against a disposable `scripts/demo.sh` installation: categories Automation and Visual with Scripter and Hands; a long synthetic turn kept Scripter Working; the Move dialog (header Category button, Ctrl+Shift+M) moved Scripter to Visual by mouse while the turn ran (still `running`); the API was restarted (the turn was reconciled to `interrupted` and shown as Stopped), then the desktop. The sidebar showed Automation · 0 with No bots and Visual · 2 with Hands and Scripter; bot IDs, conversation IDs, and categories matched the list taken before the move, and Scripter's history was intact. Keyboard-only category creation and moves, rename and delete, and restarts were covered in the POC-031 checks. The box, API, database, and data directory were removed.

### Observed run (2026-11-03)

`ELSE_LIVE_MILESTONE=1 … milestone_live` passed in 896 s with Codex CLI 0.166.0 (`gpt-6.1-sol`), Claude Code 2.1.316 (`claude-haiku-5-5`), msb 0.7.13, and `else-guest:2` (`sha256:2bb1f17f…`, 2.70 GB image). Turns: Codex 9, Claude 1.

| Measurement | Value |
|---|---|
| Multi-file project and artifact (Codex) | 33 s |
| Ten-message send (Codex, one turn) | 35 s; follow-up that sent nothing, 8 s |
| CDP search script (Claude) | 31 s |
| Visual browser task (Codex) | 29 s, 4 screenshots of 15.5 to 30.5 KB |
| Terminal task outside Chromium (Codex) | 23 s |
| Owner-stopped screenshot turn | 11 s |
| Backup with the API stopped | 287 s |
| Backup set | 1.30 GB: VM disks 674 MB and 630 MB (encrypted), database dump 124 KB |
| First turn after restore | 19 s |

Earlier runs found two problems, both fixed before this one: a backup taken right after the API stopped was deferred because a finished turn's lease lasts until it expires (5 minutes); whole-disk operations now end leases whose turn or job has ended (other holders, such as desktop sessions, still count). And a working directory under `target/` made the runtime's socket path too long (the bot reported that its computer failed to start).

Regressions: `cargo test --workspace --no-fail-fast` passed (34 test binaries; the slowest, `backup`, 29 s). The live suites of this milestone passed during their tasks: `toolchain_live`, `scripts_live` (209 s), `cdp_live` (96 s), `vision_live` (123 s), and `input_live` (86 s). Resource limits (the computer budget, admission queueing, the screenshot size limit, the input batch limit) are covered by the non-live `computers`, `screens`, and `task_secrets` suites. The evidence log and screenshots are in `target/acceptance/` (not committed).

Limitations and follow-up work are in [follow-up.md](follow-up.md).
