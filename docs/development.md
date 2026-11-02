# Set up local development

The repository contains the Rust API, a desktop starter, and a PostgreSQL Compose service. The API serves a loopback HTTP liveness route. The desktop has no GPUI dependency or window yet.

## Prerequisites

Install Rust with Cargo and rustfmt, and Docker Engine with Docker Compose. The bootstrap was checked with Rust 1.98.0 and Compose 5.6.1. Run the commands below from the repository root.

Provider CLIs and KVM are needed when implementing provider and VM features. Cargo downloads the API's Rust dependencies on first build.

The planned Codex runtime default is GPT 6.1 Sol Medium Fast, saved as `gpt-6.1-sol`, `medium`, and `priority` in PostgreSQL. Provider-native sub-agents are disabled for the POC. These settings are part of the [provider implementation tasks](implementation/02-desktop-and-codex.md), not active configuration in the generated starters. Future owner changes use the [saved profile contract](providers.md#saved-execution-profile).

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

For a client running on the host, use `127.0.0.1`, the configured port, database, user, and password. The default connection URI is `postgresql://else:else_dev_only@127.0.0.1:5432/else`. Percent-encode credentials if you construct a URI with different values. The API does not read a database URL yet.

## Check the Rust workspace

Check both workspace members, their formatting, and tests:

```sh
cargo check --workspace --locked
cargo fmt --all -- --check
cargo test --workspace --locked
```

## Run the API

Provision credentials once, then start the API from the repository root:

```sh
cargo run -p else-api -- setup
cargo run -p else-api -- serve
curl -s http://127.0.0.1:7878/healthz
curl -s -H "Authorization: Bearer $(cat ~/.local/share/else-bot/owner-token)" \
	http://127.0.0.1:7878/v1/capabilities
```

`serve` is the default command. The process reads the root `.env` when present, and variables already set in the environment take precedence. Configuration is validated before the API opens any file or listener. An invalid value exits with status 1 and names the variable without echoing secret values. Ctrl-C or `SIGTERM` stops accepting connections, drains requests, and closes any remaining connections after the grace period.

`setup` is the explicit installation step. It creates the data directory with mode `0700` and each missing credential file with mode `0600`. Running it again reports `kept existing` and changes nothing. It never replaces an existing or malformed file and prints paths, never values. `serve` never creates credentials. It exits with an error naming `else-api setup` when a file is missing, and rejects malformed values, symbolic links, files owned by another user, and files readable by group or others.

| Variable | Default | Purpose |
| --- | --- | --- |
| `ELSE_API_BIND` | `127.0.0.1:7878` | Listener address. Only loopback addresses are accepted because the API serves plaintext HTTP. Use an SSH tunnel such as `ssh -L 7878:127.0.0.1:7878 host` for remote access. |
| `ELSE_DATA_DIR` | `$XDG_DATA_HOME/else-bot`, else `~/.local/share/else-bot` | Private runtime storage for credentials and later bot session data. Use a separate directory per checkout or test installation. |
| `OWNER_TOKEN_FILE` | `$ELSE_DATA_DIR/owner-token` | Owner bearer token: 32 random bytes as unpadded base64url. |
| `SECRET_KEY` | unset | Installation master key: 32 bytes as standard base64. When set, it is validated and used instead of the file. |
| `SECRET_KEY_FILE` | `$ELSE_DATA_DIR/secret-key` | Master key file used when `SECRET_KEY` is unset. |
| `ELSE_MAX_BODY_BYTES` | `65536` | Maximum request body, from 1024 to 1048576 bytes. Larger bodies receive `413 too_large`. |
| `ELSE_SHUTDOWN_GRACE_SECS` | `10` | Seconds to drain connections after a shutdown signal, from 1 to 120. |
| `RUST_LOG` | `info` | Log filter written to stderr. Request logs record method and path only, never headers or bodies. |

### Credentials

The API has three independent kinds of credential. Do not substitute one for another.

| Credential | Holder | Authority |
| --- | --- | --- |
| Owner token | The desktop and trusted owner tooling | Every `/v1` route, sent as exactly one `Authorization: Bearer <token>` header. The API keeps only its SHA-256 digest and compares in constant time. |
| `SECRET_KEY` | The API process only | Encrypts recoverable secrets and backups in later tasks. It authenticates no request. |
| Task grant | One restricted provider session, in a later task | Its own bot, conversation, and tool allowlist. It cannot use owner routes. Not implemented yet. |

Missing, malformed, duplicated, or wrong bearer headers receive `401` with `WWW-Authenticate: Bearer`, including on unknown `/v1` paths. Tokens in query strings are ignored. After 30 failed attempts in a minute, further failures receive `429` with `Retry-After`; the valid token still works. Requests carrying an `Origin` header receive `403`, and a `Host` that is not a loopback name receives `400`. The Host port is not checked, so a tunnel's local port works. Provider processes will never inherit either credential.

To rotate the owner token, stop the API, move the old file away, run `setup`, update the desktop's copy, and restart. Rotation does not touch `SECRET_KEY`.

`GET /healthz` returns `{"status":"ok"}` while the process serves HTTP. It requires no authentication and reports liveness only. All errors use the JSON shape from the [desktop contract](research/desktop-and-feature-contracts.md#proposed-rest-endpoints).

The desktop starter still prints `Hello, world!`:

```sh
cargo run -p else-desktop
```

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

Read the [POC backlog](../TODO.md) and [agent runbook](implementation/README.md). Resume from the [latest handoff](implementation/progress.md). The backlog separates the first usable messaging checkpoint from later assistant capabilities and defines the evidence required for each task.
