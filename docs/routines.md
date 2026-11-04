# Routines and webhooks

A routine is a saved task that a bot runs on a cron schedule in its owner conversation. Webhook triggers are planned; their authorization rules follow. The [security notes](security.md) cover prompt injection and the installation master key.

## Creating and managing routines

The owner asks a bot in chat ("every weekday at 9, summarize my unread mail"). The bot saves it with the `routine_create` MCP tool: a name, the complete prompt, a cron expression, and an optional time zone. `routine_list`, `routine_update`, and `routine_delete` manage the bot's own routines. The bot and its routines come from the turn's task grant, never from tool arguments.

Only work rooted in an owner message may use these tools: a turn answering the owner in the bot's own conversation, or a task another bot delegated while serving an owner message. A routine run, and any task a run delegated, gets no routine permission. The server derives this from the turn's stored source item, or its delegating root's, never from text. Every change rechecks the turn inside its own transaction. A routine always belongs to the bot that saved it and lives in that bot's owner conversation, even when a delegated task created it, so its runs and reports appear in that bot's chat with the owner. A bot has at most 20 routines. Names are unique among a bot's routines. Repeating a create with the same name in the same turn returns the routine it created; different settings conflict.

To move a routine to another bot, for example one the main bot just created, the initiating bot delegates a task stating the complete prompt, schedule, and time zone. The receiving bot saves it as its own and replies with the saved routine. The initiating bot disables or deletes its own copy only after that reply confirms the save. An owner-rooted delegated task may also list, update, or delete the receiving bot's existing routines. Each saved run uses that bot's normal access in its owner conversation. No credentials move with the handoff.

Bot settings lists the conversation's routines under Model, with their schedule, time zone, next run, last run, and an on/off switch. The owner API is `GET /v1/bots/{bot_id}/routines` and `PATCH /v1/bots/{bot_id}/routines/{routine_id}` with `{"enabled": bool}`.

## Schedules

A schedule is a standard five-field cron expression: minute, hour, day of month, month, day of week. Fields use digits with `*`, `,`, `-`, and `/`. Names, `@daily`-style aliases, seconds, years, and extensions such as `L` or `#` are refused. Day of week 0 and 7 are Sunday. When both day fields are restricted, either one matching is enough, as in Vixie cron. Schedules can run as often as once a minute.

Each routine is read in an IANA time zone, such as `Europe/Paris`. Without one it uses UTC, never the host's zone, and the result says so. Times follow the zone's wall clock across daylight saving changes. A time skipped when clocks go forward runs right after the gap. A time repeated when clocks go back runs once. A schedule with no future occurrence, such as 30 February, is refused.

## Runs

The scheduler admits due routines during its scan, before it claims turns, and never while maintenance holds its gate. One transaction records the occurrence and advances the next run time past now, so an occurrence runs at most once across retries and restarts. Occurrences missed while else.bot was down coalesce into one catch-up run, and the schedule then resumes after now.

A run is skipped, and recorded with its reason, when the routine's previous run has not finished, the bot's queue is full, or no provider is available. Otherwise it queues an ordinary turn in the owner conversation. The turn uses the bot's current provider and profile, as an owner message without a named provider would. It never changes the selected provider or stops the owner's work. Stop, deletion, restart reconciliation, and uncertain outcomes apply to it like any turn.

The run's prompt is a hidden routine item. Snapshots, events, and the bot's history tools leave it out, and memory never records it as owner text. The provider receives it labeled as a scheduled routine run, with instructions to do the task and report what actually happened. The report is an ordinary assistant message in the conversation. A run that fails before any reply shows its error as the routine's last run.

Turning a routine off stops future runs; a run already admitted finishes. Turning it on schedules the next occurrence after now. Setting the value a routine already has changes nothing. Renaming or editing a routine leaves earlier runs as they were admitted.

## Webhook authorization (planned)

A future event trigger starts with webhooks authenticated through an `Authorization` header.

Generate a separate bearer token for each webhook from 32 cryptographically random bytes, encoded as base64url. Generate each token independently of `SECRET_KEY` and every other webhook token. Enforce uniqueness on the stored token hash. The caller sends it over HTTPS:

```http
POST /webhooks/{webhook_id}
Authorization: Bearer <webhook_token>
```

The proposed storage is a SHA-256 hash of the token in PostgreSQL, alongside the webhook ID, associated bot and routine, and active or revoked state. Show the original token once when creating it. else.bot only needs to verify incoming tokens, so it can hash the submitted value and compare the digests in constant time. Recoverable encryption is for credentials the assistant must read back or send to another service. [Token digest and encryption patterns](https://docs.gitlab.com/development/authentication/).

Check the token against the webhook ID in the request. A leaked token authorizes only that webhook and its routine's permitted actions. Support revocation and replacement independently for each webhook. Restoring the database preserves token verification without requiring callers to change their tokens. Keep tokens out of URLs and redact the authorization header from logs. [Header handling](https://cheatsheetseries.owasp.org/cheatsheets/REST_Security_Cheat_Sheet.html).

Authentication lets the caller trigger the saved routine. Its payload remains external data. Validate payload fields against the routine's schema and pass them as inputs to its stored prompt. The payload cannot select another bot or routine, replace the system prompt, or expand permissions.
