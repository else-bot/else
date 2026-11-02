# Set up local development

The repository contains the Rust API, the GPUI desktop client, and a PostgreSQL Compose service. The API stores installation state in PostgreSQL, serves authenticated loopback HTTP, and runs Codex turns in a bubblewrap sandbox. The desktop connects to it as the owner.

## Prerequisites

Install Rust with Cargo and rustfmt, Docker Engine with Docker Compose, and `bubblewrap`. Checked with Rust 1.98.0, Compose 5.6.1, and bubblewrap 0.13.1. The desktop also needs the Linux libraries listed under [Run the desktop](#run-the-desktop). Run the commands below from the repository root.

To chat through Codex, install the Codex CLI (verified with 0.166) and log in once with `codex login` as the user that runs the API. A version-manager shim such as mise's is resolved to the real binary; otherwise set `ELSE_CODEX_BIN` to the native `codex` executable. Without Codex the API still runs, and the capabilities report why Codex is unavailable.

The saved Codex profile is GPT 6.1 Sol Medium Fast (`gpt-6.1-sol`, `medium`, `priority`), seeded by `else-api setup` and editable through the [saved profile contract](providers.md#saved-execution-profile). Provider-native sub-agents are always disabled.

## Configure the development services

Create the local environment file if it does not exist:

```sh
test -e .env || cp .env.example .env
```

Edit `.env` for your local setup. Git ignores this file. Its example credentials are for the local development database.

| Variable | Default | Purpose |
| --- | --- | --- |
| `COMPOSE_PROJECT_NAME` | `else-dev` | Namespaces containers, the network, and the data volume. Use a different value for another checkout. |
| `COMPOSE_FILE` | `infra/compose.yaml` | Lets root-level Compose commands find the infrastructure definition. |
| `POSTGRES_DB` | `else` | Database created on first initialization. |
| `POSTGRES_USER` | `else` | Development database user created on first initialization. |
| `POSTGRES_PASSWORD` | `else_dev_only` | Local development password. |
| `POSTGRES_PORT` | `5432` | Host port bound to `127.0.0.1`. Change it if another database uses that port. |

Compose reads these settings from the root `.env`. The file selection and project name use [Compose's predefined variables](https://docs.docker.com/compose/how-tos/environment-variables/envvars/). Only the three PostgreSQL initialization variables are passed into the container. Adding future API secrets to `.env` does not automatically expose them to PostgreSQL.

## Start PostgreSQL

Validate the configuration, then start the database and wait for its health check:

```sh
docker compose config --quiet
docker compose up -d --wait
docker compose ps
```

The service uses `postgres:18.6`, password authentication for TCP connections, and a persistent named volume. Its health check uses TCP so the temporary initialization server does not report ready. PostgreSQL 18 stores its versioned data directory under `/var/lib/postgresql`, which is the volume mount used here. See the [official image documentation](https://hub.docker.com/_/postgres).

Open a database shell inside the container:

```sh
docker compose exec postgres sh -c 'psql -U "$POSTGRES_USER" -d "$POSTGRES_DB"'
```

For a client running on the host, use `127.0.0.1`, the configured port, database, user, and password. The default connection URI is `postgresql://else:else_dev_only@127.0.0.1:5432/else`. Percent-encode credentials if you construct a URI with different values. The API reads `DATABASE_URL`, or builds this URI from the `POSTGRES_*` variables when `DATABASE_URL` is unset.

## Check the Rust workspace

Check both workspace members, their formatting, and tests:

```sh
cargo check --workspace --locked
cargo fmt --all -- --check
cargo test --workspace --locked
```

## Run the API

Start PostgreSQL, provision the installation once, then start the API from the repository root:

```sh
docker compose up -d --wait
cargo run -p else-api -- setup
cargo run -p else-api -- serve
curl -s http://127.0.0.1:7878/healthz
printf 'Authorization: Bearer %s\n' "$(cat ~/.local/share/else-bot/owner-token)" |
	curl -s -H @- http://127.0.0.1:7878/v1/capabilities
```

`serve` is the default command. The process reads the root `.env` when present, and variables already set in the environment take precedence. A `.env` that exists but cannot be parsed fails startup without printing its contents. Configuration is validated before the API opens any file or listener. An invalid value exits with status 1 and names the variable without echoing secret values. Ctrl-C or `SIGTERM` stops accepting connections, drains requests, and closes any remaining connections after the grace period.

`setup` is the explicit installation step. It creates the data directory with mode `0700` and each missing credential file with mode `0600`, applies migrations, seeds any missing provider profile, and creates the main bot `Assistant` with its conversation the first time. Running it again reports `kept existing` and changes nothing: saved profiles keep their values, and a deleted main bot or cleared default stays as it is. It never replaces an existing or malformed file and prints paths, never values. `serve` never creates credentials. It exits with an error naming `else-api setup` when a file is missing, and rejects malformed or empty values, symbolic links, files owned by another user, files readable by group or others, and credential directories writable by others. The file is opened without following symlinks and validated through the opened descriptor.

| Variable | Default | Purpose |
| --- | --- | --- |
| `ELSE_API_BIND` | `127.0.0.1:7878` | Listener address. Only loopback addresses are accepted because the API serves plaintext HTTP. Use an SSH tunnel such as `ssh -L 7878:127.0.0.1:7878 host` for remote access. |
| `DATABASE_URL` | built from `POSTGRES_*` | PostgreSQL connection URI. Never logged; connection errors omit it. |
| `ELSE_DATA_DIR` | `$XDG_DATA_HOME/else-bot`, else `~/.local/share/else-bot` | Private runtime storage for credentials and later bot session data. Use a separate directory per checkout or test installation. |
| `OWNER_TOKEN_FILE` | `$ELSE_DATA_DIR/owner-token` | Owner bearer token: 32 random bytes as unpadded base64url. |
| `SECRET_KEY` | unset | Installation master key: 32 bytes as standard base64. When set, it is validated and used instead of the file. |
| `SECRET_KEY_FILE` | `$ELSE_DATA_DIR/secret-key` | Master key file used when `SECRET_KEY` is unset. |
| `ELSE_CODEX_BIN` | `codex` on `PATH` | The Codex CLI used for the `codex` provider. Its login is the host's (`$CODEX_HOME` or `~/.codex`); run `codex login` once as the API user. |
| `ELSE_SYNTHETIC_PROVIDER` | `0` | `1` exposes the deterministic `synthetic` provider, labeled "Synthetic (test)", for demos and manual checks. It runs no model. |
| `ELSE_MAX_ACTIVE_TURNS` | `2` | Provider turns running at once across all bots, from 1 to 32. Each bot runs at most one turn at a time regardless. |
| (bubblewrap) | required for providers | Provider CLIs run inside `bwrap` (`pacman -S bubblewrap`, `apt install bubblewrap`). Without it the API starts, but host providers report unavailable. See [host provider processes](security.md#implemented-codex-launch-profile). |
| `ELSE_MAX_BODY_BYTES` | `65536` | Maximum request body, from 1024 to 1048576 bytes. A larger declared `Content-Length` receives `413 too_large` on every route; body extractors also stop chunked bodies at the limit. |
| `ELSE_SHUTDOWN_GRACE_SECS` | `10` | Seconds to drain connections after a shutdown signal, from 1 to 120. |
| `RUST_LOG` | `info` | Log filter written to stderr. Request logs record method and path only, never query strings, headers, or bodies. |

### Owner routes

The implemented routes follow the [desktop contract](research/desktop-and-feature-contracts.md#proposed-rest-endpoints):

| Route | Behavior |
| --- | --- |
| `GET /v1/capabilities` | API version, per-process `server_instance_id`, enabled providers with availability, the default provider, and limits. Model settings and credentials are not exposed. |
| `GET /v1/bots[?include_deleting=true]` | `default_bot_id` and active bots, oldest first. `include_deleting` adds tombstones whose shutdown is pending. |
| `POST /v1/bots` | `{request_id, name, role}`. `201` with `Location` on first acceptance; the same request returns `200` with the same bot; changed input under that ID returns `409 request_conflict`. |
| `GET /v1/bots/{id}` | An active bot, or a deletion receipt with `cleanup.state` and no role or conversation reference. |
| `DELETE /v1/bots/{id}` | Commits the tombstone, clears the default if it pointed at this bot, ends queued messages, and interrupts running work before returning. `200` with `cleanup.state = done` when nothing was running; `202` with `pending` while the provider stops, after which the bot becomes `deleted`. Repeating it returns the same receipt. History and conversations are retained, and late provider output is never appended. |
| `POST /v1/bots/{id}/cancel` | Stop work: `{request_id}`. Ends every queued message and interrupts the running one accepted up to now. `202 {state: "pending"}` until the provider has stopped, then `200 {state: "done"}`. New messages get `409 cancellation_pending` meanwhile. Retrying a completed request ID returns its receipt and never affects later messages. |

| `POST /v1/bots/{id}/turns` | `{request_id, text, provider_id}`. Saves the owner message and a queued turn in one transaction, then wakes the scheduler. `202` on acceptance; a matching retry returns `200` with the same item and turn; different text or provider under that ID returns `409`. A deleted bot returns `410 bot_deleted`. More than four waiting messages return `429 queue_full` with `Retry-After`. A `null` provider uses the default. |
| `GET /v1/bots/{id}/turns/by-request/{request_id}` | Resolves a lost send response, or `404`. |
| `GET /v1/bots/{id}/conversation[?limit=50&before_seq=N]` | Consistent snapshot read in one repeatable-read transaction: the newest `limit` items (1 to 200) in ascending `seq`, their turns plus every unfinished turn, activity, `older_before_seq` for the previous page (or `null`), and `event_cursor`. |
| `GET /v1/ws` | Owner WebSocket for live updates. Subscribe to `bots` (pushed list) and `bot:<id>` (committed events after a snapshot cursor). Event kinds are `item.upsert`, `item.delta`, `turn.updated`, and `bot.updated`. See [events and reconnecting](#events-and-reconnecting). |
| `GET`, `PATCH /v1/settings/providers/{provider_id}` | Read or update the saved execution profile. `PATCH` takes any of `model`, `reasoning_effort`, `service_tier`, and an optional `expected_revision`. The adapter validates the exact values; unsupported values return `400 invalid_profile` and nothing changes. A change bumps the revision. Accepted turns keep the revision they were accepted with. |

Unknown or malformed IDs return `404 not_found`, always after authentication. A role over 8 KiB or message text over 32 KiB returns `413 too_large`; other invalid input, including NUL characters, returns `400 invalid_input`. Try it with a synthetic bot:

```sh
printf 'Authorization: Bearer %s\n' "$(cat ~/.local/share/else-bot/owner-token)" |
	curl -s -H @- -H 'Content-Type: application/json' \
	-d "{\"request_id\":\"$(uuidgen)\",\"name\":\"Work\",\"role\":\"Help with my work\"}" \
	http://127.0.0.1:7878/v1/bots
```

### Turns and the scheduler

One scheduler runs inside the API. PostgreSQL rows are the queue: it claims the oldest queued message of each idle bot, oldest first across bots, within `ELSE_MAX_ACTIVE_TURNS`. A unique index allows one active turn per bot. Turn status moves through `queued`, `preparing`, `sending`, `running`, and a terminal `completed`, `interrupted`, `failed`, or `uncertain`. Assistant text is persisted at most every 100 ms or 2 KiB under one stable item ID. The completion of a turn comes only from the adapter's terminal outcome, never from text.

History `seq` is arrival order. A message queued behind a running turn can therefore precede that turn's reply; each item carries its `turn_id`.

On startup the API takes two singleton locks before reading or changing runtime state: an exclusive lock on `$ELSE_DATA_DIR/api.lock` and a PostgreSQL session advisory lock. A second API on the same data directory exits with `another else.bot API is already running`, and one on the same database with `another else.bot API is already using this database`, before reconciling or dispatching anything. It then binds the listener and reconciles: a turn claimed but never sent returns to the queue; a turn that may have reached its provider, including one being stopped, becomes `uncertain` with `effect_uncertain = true` and is never resent; pending stops and deletions waiting on those turns complete.

An interrupted provider has 10 seconds to stop. If it does not, its turn is recorded as `uncertain` rather than claiming the action was undone.

### Codex provider

The `codex` provider is listed first and is the default when available. It reports unavailable, with the reason, when bubblewrap, the CLI, or the host login is missing. Each turn starts `codex app-server` inside the bot's sandbox, opens or resumes the bot's native thread, sends the message with the saved profile (`gpt-6.1-sol`, `medium`, `priority` by default) on thread start or resume and on turn start, streams the reply, and stops the process after the terminal event. The settings Codex reports back must equal the requested ones or the turn fails with `profile_mismatch`. A native thread is reused while its fingerprint (prompt version, sandbox layout, tool and agent policy, profile, bot name and role) matches. A changed profile or role, or a thread Codex can no longer resume, starts a new thread and sends earlier history from PostgreSQL (whole messages, newest first, within 32 KiB) ahead of the owner's message. History handled by another provider since the thread last ran is sent the same way. Stop work sends `turn/interrupt` and keeps the partial reply. Codex's only tools are else.bot's memory tools: the per-launch configuration names one MCP server, `elsebot`, at the API's `/tools/v1/mcp`, with only the six memory tools enabled. The turn's task grant reaches Codex through the `ELSE_TASK_GRANT` environment variable and is never written to disk. For this model Codex calls MCP tools from its code-mode host (`codex-code-mode-host`, mounted read-only next to the CLI inside the sandbox). Preflight requires exactly that server and those tools. Any other tool, command, file, web, image, or native-agent item, an MCP call outside the allowlist, or any approval request ends the turn as `policy_violation`. Reasoning text is never stored.

Profile updates accept models `gpt-6.1-sol`, efforts `low` to `max`, and tiers `default` or `priority`. `ultra` is refused because it delegates to native sub-agents. Only Codex versions whose policy controls were verified (0.166) are offered; another version reports unavailable unless `ELSE_CODEX_ALLOW_UNVERIFIED=1` is set after checking it. The fingerprint also covers the CLI version and the logged-in account, so changing either starts a new native thread. A session that may hold an uncertain delivery is never resumed. Events are matched to the current thread and turn, and a model reroute or settings change away from the saved profile ends the turn as `profile_mismatch`.

Live checks use the owner's subscription and run only on request:

```sh
ELSE_LIVE_CODEX=1 cargo test -p else-api --test codex_live -- --nocapture
```

### Memory

Each bot has an immutable, ordered memory log (`memory_entry`, zero-based `seq` per bot). Owner messages enter it when accepted; assistant replies enter it with their final text when the turn ends, including partial replies of stopped, failed, or uncertain turns. Both are written in the same transaction as the conversation change, keyed `item:<id>`, so a retried send adds nothing. Texts over 64,000 bytes are split at UTF-8 boundaries into ordered parts. Migration `0006` backfilled existing history the same way. Entries cannot be updated or deleted. Search uses PostgreSQL's `simple` text configuration (lexical, not semantic).

Authority follows the kind: `owner` only for owner messages and explicit corrections, `assistant` for replies, `tool` for tool activity. A correction exists only through the owner route below and must supersede an earlier owner entry of the same bot; a database check and trigger enforce this as well. Search hides superseded entries unless asked, and reads show `supersedes_seq` and `superseded_by`.

| Route | Behavior |
| --- | --- |
| `GET /v1/bots/{id}/memory[?start_seq&count]` | The original entries with source, authority, parts, and correction links, plus `length`. `410` for a deleted bot. |
| `POST /v1/bots/{id}/memory/corrections` | `{request_id, supersedes_seq, text}`. `201 {seq}`; a retry returns the same entry; `400 invalid_correction` when the target is not an owner entry of this bot. |
| `GET /v1/bots/{id}/memory-access` | Bots whose memory this bot may read. |
| `PUT`, `DELETE /v1/bots/{id}/memory-access/{target_id}` | Grant or revoke reading another bot's memory. A bot always reads its own. |

The summary tree (POC-014) is built inside the API by one compactor loop. Each entry gets a leaf job; a leaf whose labeled text (`Owner: …`, `Assistant: …`, `Owner correction of #N: …`, `Tool: …`) fits the node limit is copied verbatim, and two built siblings whose texts joined by a newline fit are joined into their parent. Anything longer waits with `summary_unavailable` until a no-tools summary provider exists (POC-015); nothing is cut to fit. Jobs are claimed with a lease token that completion must present, so a worker whose lease was replaced cannot publish. Leaves publish into the saved view strictly in order after `published_seq`; above `view_high` bytes the view merges the adjacent sibling pair with the greatest `(count − last entry) / span` (oldest first on ties) until it is at most `view_low`, and only parents that exist merge, so a blocked parent never drops coverage. The view, watermark, and revision change together and are loaded unchanged after a restart. Budgets (`node_bytes` 512, `view_high` 128,000, `view_low` 64,000 by default) are the installation recipe, recorded with each generation.

| Route | Behavior |
| --- | --- |
| `GET /v1/bots/{id}/memory/view` | The saved view: generation, revision, `valid`, published watermark, compaction state, jobs waiting by reason, and the nodes with their text. |
| `POST /v1/bots/{id}/memory/rebuild` | `202 {building_generation}`. Builds a new generation with the current recipe; appends meanwhile reach both. Once its leaves cover at least the current view, the API switches generation and view atomically and keeps the old nodes. |
| `GET`, `PATCH /v1/settings/memory` | The recipe for new generations: `{node_bytes, view_high, view_low}` (64 ≤ node ≤ 65,536; `view_low` ≥ 4 × node; `view_low` < `view_high` ≤ 4,000,000). |

Provider sessions reach memory over MCP at `POST /tools/v1/mcp` with their turn's task grant. Requests must be JSON-RPC 2.0 (`-32600` otherwise, `-32602` for bad tool parameters). `memory_view` pages through the saved view (`next_index`) with its covered and pending ranges and waiting jobs; `memory_zoom` opens a node into its halves (unbuilt halves are marked) or, for span 1, the original; `memory_date` gives when entries were recorded. `memory_bots` lists readable bots (`more` when the budget cut the list); `memory_search` returns up to 20 hits (rank, then newest), each body at most 600 bytes, with a cursor bound to the log length at the first page, so corrections made later neither hide nor shift entries on later pages; `memory_read` reads up to 20 entries from a sequence number, gives `next_start_seq` when the budget stopped earlier, and pages a long entry with `offset` and `next_offset` (an offset inside a character starts at that character). Cut text is marked `truncated`, and at most ten correction links are listed per entry (`more_superseded_by`). Each complete tool result, sent once as JSON text, is at most 16,000 bytes after escaping. Appends lock the bot row before the memory store, so none lands after a deletion commits. Hits carry the originating bot, source IDs, authority, and correction links outside the text. Unknown, deleted, and unshared bots look the same. Each call is logged to the calling bot's memory as `tool_activity` (arguments and returned sequence numbers, not copied text). Deleting a bot keeps its memory and ends every read of it.

### Events and reconnecting

Every change to history or turn state appends rows to `bot_events` in the same transaction, under the bot's row lock, so a bot's event sequence follows commit order. Cursors are `<server_instance_id>:<bot_id>:<seq>`; the instance ID changes on every API start. The socket sends only committed rows; inside the API, channels wake on a `pg_notify` hint or a one-second database poll. `item.delta` carries `offset_utf8`, the byte length of the item's text before the delta; a client appends only when its current length matches.

Live updates use one WebSocket at `GET /v1/ws`, authenticated like every other route. Commands stay on HTTP because they carry request IDs and status codes. Messages are JSON:

```text
→ {"op":"subscribe","channel":"bots"}
→ {"op":"subscribe","channel":"bot:<id>","cursor":"<cursor from a snapshot>"}
→ {"op":"unsubscribe","channel":"bot:<id>"}
← {"channel":"bots","event":"bots.snapshot","data":{"default_bot_id":…,"bots":[…]}}
← {"channel":"bot:<id>","id":"<cursor>","event":"item.delta","data":{…}}
← {"channel":"bot:<id>","error":{"code":"cursor_reset_required","message":"…"}}
```

`bots` pushes the whole list on subscribe and whenever a bot is created, deleted, or changes activity. Clients take a snapshot, subscribe to `bot:<id>` with its cursor, apply only the next contiguous `seq`, ignore IDs at or below the applied one, and after a reconnect resubscribe with the last received cursor. Channel errors end only that channel: `cursor_required` or `invalid_cursor` (malformed or another bot's), `cursor_reset_required` (earlier API process or beyond the latest event; load a fresh snapshot), `bot_deleted`, `not_found`, and `unavailable` (a database read failed; resubscribe). Every subscription ends with an error or an unsubscribe, never silently, and nothing queued by a replaced or unsubscribed subscription is written. Deleting a bot ends its channel within about a second with `bot_deleted`, and the snapshot then returns `410 bot_deleted`. The server pings every 15 seconds; a connection allows 32 running channels and shares one bounded outgoing queue among them. Event payloads contain no request hashes, profiles, native session IDs, or credentials.

The first version used a per-bot SSE route and three-second sidebar polling; the [desktop contract](research/desktop-and-feature-contracts.md#history-sse-and-polling-were-tried-first) explains the switch.

### Database and migrations

Versioned SQL migrations live in `apps/api/migrations` and are embedded in the binary. Applied versions are recorded in `_sqlx_migrations`, so applying them again is a no-op. Apply them explicitly with:

```sh
cargo run -p else-api -- migrate
```

`serve` and `setup` also apply pending migrations before doing anything else. `serve` then requires completed setup: on an empty installation it exits with `installation setup has not completed; run else-api setup` and creates nothing. An unreachable database fails startup within five seconds with `cannot reach PostgreSQL`. `GET /healthz` reports process liveness; `GET /readyz` returns `200 {"status":"ready"}` while PostgreSQL answers and `503 database_unavailable` otherwise. Neither requires authentication or returns data.

Migrations never delete or rewrite saved rows to make startup succeed. The saved Codex profile is seeded as `gpt-6.1-sol`, `medium`, `priority` only when its row is missing.

### Credentials

The API has three independent kinds of credential. Do not substitute one for another.

| Credential | Holder | Authority |
| --- | --- | --- |
| Owner token | The desktop and trusted owner tooling | Every `/v1` route, sent as exactly one `Authorization: Bearer <token>` header. The API keeps only its SHA-256 digest and compares in constant time. |
| `SECRET_KEY` | The API process only | Encrypts recoverable secrets and backups in later tasks. It authenticates no request. |
| Task grant | One restricted provider session, in a later task | Its own bot, conversation, and tool allowlist. It cannot use owner routes. Not implemented yet. |

The examples pipe the header through `printf`, a shell builtin, so the token never appears in a process argument list. Avoid `curl -H "Authorization: Bearer $TOKEN"`.

Missing, malformed, duplicated, or wrong bearer headers receive `401` with `WWW-Authenticate: Bearer`, including on unknown `/v1` paths. Tokens in query strings are ignored. After 30 failed attempts in a minute, further failures receive `429` with `Retry-After`; the valid token still works. Requests carrying an `Origin` header receive `403`, and a `Host` that is not a loopback name receives `400`. The Host port is not checked, so a tunnel's local port works. Provider processes will never inherit either credential.

To rotate the owner token, stop the API, move the old file away, run `setup`, update the desktop's copy, and restart. Rotation does not touch `SECRET_KEY`.

`GET /healthz` returns `{"status":"ok"}` while the process serves HTTP. It requires no authentication and reports liveness only. All errors use the JSON shape from the [desktop contract](research/desktop-and-feature-contracts.md#proposed-rest-endpoints).

### PostgreSQL tests

`cargo test --workspace` includes tests that need the Compose PostgreSQL service. Each test creates its own `else_test_<uuid>` database, applies migrations, and drops it with `WITH (FORCE)` when the test ends. The application database is never used. Set `TEST_DATABASE_URL` to use a different server; its user needs `CREATEDB`. Start the service with `docker compose up -d --wait` before running tests.

## Run the desktop

The desktop is a native GPUI window built with [GPUI Kit](https://gpui-kit.com) `gpui-kit = "=0.8.1"`, which pins GPUI as `gpui-pre 0.3.10`. The pin is exact because GPUI is pre-1.0. It was built and run on Arch Linux (Omarchy) with Hyprland 0.56.2 on Wayland and Rust 1.98.0. Linux needs `libxkbcommon`, `libxcb`, `fontconfig`, and `freetype2`, plus a Vulkan-capable graphics driver; Wayland and X11 sessions are both supported by GPUI. The first build compiles GPUI and takes a few minutes.

Start the API, then the desktop. The desktop reads the same root `.env` as the API, so `ELSE_DATA_DIR` or `OWNER_TOKEN_FILE` set there apply to both:

```sh
cargo run -p else-desktop
```

| Variable | Default | Purpose |
| --- | --- | --- |
| `ELSE_API_URL` | `http://127.0.0.1:7878` | API origin. Only `http://` loopback origins without a path, query, or credentials are accepted; this build has no TLS. For a remote server use `ssh -L 7878:127.0.0.1:7878 <server>`. |
| `ELSE_OWNER_TOKEN_FILE` (or the API's `OWNER_TOKEN_FILE`) | `$ELSE_DATA_DIR/owner-token`, else `$XDG_DATA_HOME/else-bot/owner-token`, else `~/.local/share/else-bot/owner-token` | The owner token written by `else-api setup`. It must be a regular file you own with mode `0600`; symbolic links are refused. |
| `ELSE_OWNER_TOKEN` | unset | Token value for a development launcher. Takes precedence over the file. Prefer the file. |

The window lists bots on the left and the selected conversation on the right. New bot opens a dialog for the name and role; deleting asks for confirmation, keeps history, and shows the bot as Deleting until its work has stopped. The header shows a provider choice for the next message, Stop work while the bot is busy, and Delete. Replies stream into the transcript as the API commits them; per-turn notes show Queued, Working, Stopped, Failed, or Outcome unknown. Drafts are kept per bot while the desktop runs. A message whose send outcome is unknown stays visible as Not confirmed with Retry; after reconnecting the desktop asks the API whether it arrived and resends it under the same request ID only if it did not. The view follows new output only while it is at the bottom.

| Keys | Action |
| --- | --- |
| Up/Down, Ctrl+N/Ctrl+P | Select the next or previous bot while the sidebar has focus |
| Enter (sidebar) / Escape (composer) | Move focus to the composer / back to the sidebar |
| Enter / Shift+Enter | Send / insert a new line |
| Ctrl+Shift+N | New bot |
| Ctrl+Shift+X | Stop work |
| Ctrl+Shift+D | Delete the selected bot |
| Ctrl+Shift+P | Cycle the provider for the next message |
| Ctrl+Shift+Up | Load older messages |

The token is sent only as an `Authorization` header to the configured origin. Redirects are never followed, and the token never appears in URLs, logs, or messages. A missing or invalid configuration, a rejected token, or an unreachable API is shown in the sidebar; loaded bots stay visible while the desktop retries. The sidebar and the open conversation update live over the WebSocket; nothing polls. Up/Down (or Ctrl+N/Ctrl+P) move the selection while the sidebar has focus. Closing the window quits the desktop; work already accepted by the API keeps running.

`scripts/demo.sh` runs a disposable installation for manual checks, with its own `else_demo` database and `/tmp/else-demo` data directory and the synthetic provider enabled: `scripts/demo.sh reset`, then `scripts/demo.sh api` and `scripts/demo.sh desktop` in separate terminals. `scripts/demo.sh drop` removes it.

The workspace shares its root `Cargo.lock` and `target` directory. The backup research crate remains excluded so its documented standalone commands still work. See [Cargo workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html).

## Stop and resume services

Stop containers and remove their network while preserving the database volume:

```sh
docker compose down
```

Run `docker compose up -d --wait` to recreate the container with the saved database. Initialization settings apply only to an empty volume. Changing the database name, user, or password in `.env` does not rewrite an existing database. Apply deliberate SQL changes when updating an initialized installation. The [official image documentation](https://hub.docker.com/_/postgres) describes this initialization behavior.

For a disposable development database that you explicitly want to erase, remove its volume:

```sh
docker compose down --volumes
```

This deletes the current Compose project's database. Use ordinary `down` when you want to retain it.

## Start implementation work

Read the [POC backlog](../TODO.md) and [agent runbook](implementation/README.md). Resume from the [latest handoff](implementation/progress.md). The first usable POC (POC-001 to POC-012) is complete; the [smoke procedure](implementation/smoke.md) repeats its acceptance against a disposable installation. The next milestone is the assistant POC.
