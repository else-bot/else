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
2. Main is told a fact, the owner corrects it through the memory corrections route, and Main is asked for the current value and to have the best-suited bot suggest a Lisbon museum. Checked: the answer has the corrected value, Main's own turn called a memory tool, exactly one delegation went to Travel (chosen by role) and completed, and the parent continuation received it, all with one provider slot.
3. Builder (Codex) writes `notes.txt`, starts a 15-minute background job, and is shown copied text demanding host access, a credential file, and other bots' memory. Checked: one job, no credential text and none of Main's memory in its reply or tool activity. Then Builder continues on Claude: it reads `notes.txt`, checks the job with `computer_job` by its recorded ID in Claude's own turn, the job is not started again, and the handoff `codex → claude` is `accepted`.
4. A running Travel turn is stopped (`interrupted`); a bot with a running turn is deleted; the owner stops Builder's computer; the API is stopped. Checked: no provider sandbox survives, and after a restart the deleted bot is a tombstone outside the list, Builder's computer is still owner-stopped, the history count is unchanged, and no computer runs.
5. With the API stopped, `else-api backup` captures a whole-assistant set and replicates it (`remote: 1 sets replicated`).
6. The runtime home and local sets are deleted; `else-api restore-remote <set> <new database> <new directory>` restores into a clean installation, which is started. Checked: the Codex profile is `low` at revision 2, `ultra` (native sub-agents) is still refused, the Claude profile is restored; after the owner resumes Builder's computer, Builder reads `BUILD-NOTES-7` from its restored disk; Main answers the corrected fact from the conversation rebuilt from PostgreSQL (native sessions are not portable; the host logins are reused on the same host); no turn ended in a policy violation.
7. The first usable POC scenario at API level: capabilities default to Codex; a new Work bot learns a codename; the reported settings are `gpt-6.1-sol`/`low`; after a restart the bot list is identical and Work recalls the codename; no provider process is left.

The small messaging UI was checked separately against a disposable `scripts/demo.sh` installation inside an omabox desktop (never the owner's session): the desktop connected, listed both bots, showed Codex enabled and Claude disabled from the server's capabilities, and a message typed into its composer was stored and answered by Codex.

## Observed run (2026-11-02)

`ELSE_LIVE_ACCEPTANCE=1 … acceptance_live` passed in 319 s: Codex 0.166.0 (`gpt-6.1-sol`), Claude Code 2.1.316 (`claude-haiku-5-5`), msb 0.7.13 with `else-guest:1`, PostgreSQL 18.6, SeaweedFS 4.50. Main replied "Thursday at 15:00, per your correction in memory entry #2" after the restore; the delegation went to Travel and was delivered to attempt 2; Builder's Claude turn checked the job by ID; the restored Builder read `BUILD-NOTES-7`; Work recalled `BLUEFIN` after a restart. In the box, the desktop showed the conversation and the answer `ACCEPTED` to a message typed into it (`target/acceptance/desktop.png`, not committed).

Limitations and follow-up work are in [follow-up.md](follow-up.md).
