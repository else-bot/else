# Set up local development

The repository contains two Rust binary starters and a PostgreSQL Compose service. Both binaries currently print `Hello, world!`. The API does not serve requests, and the desktop has no GPUI dependency or window yet.

## Prerequisites

Install Rust with Cargo and rustfmt, and Docker Engine with Docker Compose. The bootstrap was checked with Rust 1.98.0 and Compose 5.6.1. Run the commands below from the repository root.

Provider CLIs and KVM are needed when implementing provider and VM features. The current starters have no external Rust dependencies.

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

For a client running on the host, use `127.0.0.1`, the configured port, database, user, and password. The default connection URI is `postgresql://else:else_dev_only@127.0.0.1:5432/else`. Percent-encode credentials if you construct a URI with different values. The starter API does not read a database URL yet.

## Check and run the Rust starters

Check both workspace members and their formatting:

```sh
cargo check --workspace --locked
cargo fmt --all -- --check
```

Run either binary:

```sh
cargo run -p else-api
cargo run -p else-desktop
```

Each prints `Hello, world!` and exits. API framework, database access, migrations, GPUI dependencies, and application features are added in subsequent implementation work.

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
