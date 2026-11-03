# else.bot

else.bot is a personal AI agent that learns to reflect its owner. One main assistant understands your personal, family, and work context, remembers what matters, and delegates tasks to other bots when needed.

You should be able to talk to it without choosing the right bot or repeating your circumstances each time. The assistant learns from daily use and becomes proactive enough to suggest what to do next.

POC-001 through POC-032 are marked complete: the GPUI messaging app, Rust API, PostgreSQL storage, memory, delegation, bot computers, provider switching, backup/recovery, guest programming tools and script workflows with task secrets, visual desktop control (screenshots and input), CDP scripting against the visible browser, and sidebar categories are implemented. The [acceptance record](docs/implementation/acceptance.md) describes the checks and their limits.

The first version is a messaging app for one owner. It should let you talk to multiple bots, create a bot, and delete a bot. Provider continuity and persistent memory support those conversations. Email, projects, notes, routines, mobile apps, and additional desktop views come later. The [research review](docs/research/README.md) records the starting design and verification results.

## Why I want to build it

I like how Grok Bot works. It is easy to use, has many connectors, and lets bots share memories and talk to each other. Its mobile and desktop apps have notifications built in. Talking to a bot and having it do things feels like the right starting point.

But I struggle to use it for everything. I am tied to one provider, which makes moving to another AI provider difficult. I also have to choose between multiple bots instead of talking to one main assistant that knows me and decides who should handle the task.

I also find myself needing separate apps for projects and notes so the bots can use them. I want those tools inside else.bot, alongside email and conversation.

## One assistant across family life and work

For example:

> Remind me to pick up the kids on Friday because my wife works late that day.

else.bot should recognize the family context and create the reminder in the right place. The same assistant should understand your job, companies, and ongoing work, then help decide what to tackle next.

Each bot has its own memory and responsibilities. The main assistant knows which bots exist and can delegate work or retrieve relevant context through their skills.

## The tools belong in the conversation

Messaging is the main entry point. The broader product should add mobile clients, notifications, and views for the things the assistant works with:

- Email, with help organizing messages, creating reminders, and drafting replies.
- Projects and kanban boards for tracking work across multiple projects.
- Notes you can ask bots to write and retrieve later.
- Routines that run on a schedule or respond to events through webhooks.

Each bot also has an isolated computer with Chromium, a desktop, and a terminal. You can open its desktop through VNC. Its files and browser state persist between tasks while idle computers can release system resources.

Each bot has a project directory on the host (`~/.else/bots/<bot id>`), mounted as `~/work` in its computer. Bots work in their computers by default. When you ask, a bot can also work on your machine: edit the spreadsheets in a folder you name, or change your system's settings.

## Provider choice and ownership

else.bot should work with AI providers through CLIs installed on the host and your existing subscriptions. The CLIs run on the host as your user, starting in the bot's project directory, with their own shell and file tools; bots operate their isolated computers through scoped tools. Keeping host work to what you ask for is up to the bot's instructions, not a sandbox: a bot can read and change what your user can. The [decision record](docs/decisions/0001-providers-run-on-the-host.md) explains this trade-off. You can switch providers during a task while keeping one conversation and handing over the context needed to continue.

The starting Codex profile is GPT 6.1 Sol Medium Fast. Provider/model settings are saved in PostgreSQL and can be updated by the owner. Provider-native sub-agents are disabled for the POC. See the [execution policy](docs/providers.md#saved-execution-profile).

The assistant runs on a server or dedicated hardware at home. Its memory, configuration, conversations, and bot computers belong to the owner. Encrypted backups should let you restore the assistant on a new machine and resume from the latest completed backup.

## Install

On Linux (Debian 12 or newer, Ubuntu 24.04 or newer, Arch Linux / Omarchy; x86_64):

```sh
curl -fsSL https://raw.githubusercontent.com/else-bot/else/main/install.sh | bash
```

It installs the latest release into `~/.else`, asking once before it installs any missing system package with sudo:
- build tools, Docker with Compose, the PostgreSQL 18 client tools (from apt.postgresql.org on Debian and Ubuntu), and the desktop's libraries;
- in `~/.else`: its own Rust toolchain (your Rust is not touched), Microsandbox 0.7.13 (checksum-verified), and release builds of `else-api` and `else-desktop`;
- PostgreSQL 18 as the Docker Compose project `else-bot` with a generated password;
- the guest VM image (about 15 minutes the first time);
- `else-api setup`, and the `else-api` systemd user service on `127.0.0.1:7878`.

Bot computers need `/dev/kvm`; the installer offers to add you to the `docker` and `kvm` groups (log in again afterwards). Install and log in to the Codex CLI (`codex login`) and, optionally, Claude Code; the installer checks for them but does not install them. Then open the app with `elsebot desktop` or from your launcher (else.bot).

On macOS (Apple silicon), the same command installs everything on the Mac itself: the API, the desktop, PostgreSQL in Docker, Microsandbox, and the guest image. Before running it, install:
- Xcode with the Metal toolchain (the desktop compiles Metal shaders);
- OrbStack or Docker Desktop, running;
- the PostgreSQL 18 client tools. If they are missing, the installer offers to run `brew install postgresql@18`.

Bot computers use the Hypervisor framework, so there is no `/dev/kvm` step. The API runs as a background process that `elsebot` manages (`elsebot start`, `stop`, `logs`). It does not start at login yet; run `elsebot start` after a reboot. arm64 Linux is not tested yet.

### Update

Run the same command again, or `elsebot update`. It builds the new release while the current one keeps running, then:
- refuses, changing nothing, while a bot is working (a turn running or a command running in a bot computer); run it again later;
- otherwise stops the API from taking new work, stops it, backs up first when the release changes the database, switches to the new release, and starts it; if it does not come up, it switches back;
- keeps the previous release in `~/.else/versions`.

Messages queued during the update run afterwards; background jobs in the bot computers keep running. `elsebot update --version 0.0.1` (or `ELSE_VERSION=0.0.1`) installs a given release.

### Where things are

```text
~/.else/
  src/          the release's source (git checkout at the release tag)
  versions/     built releases; current -> versions/<version>
  bin/          elsebot, else-api, else-desktop
  config/       .env (settings and database password), readonly.env (elsebot db), both 0600; compose.yaml
  data/         credentials, bot computers, artifacts, backups
  bots/         each bot's project directory (bots/<bot id>), mounted as ~/work in its computer
  toolchain/    Rust, only for building else.bot
  microsandbox/ the VM runtime
  build/        build cache
  logs/         install, update, and build logs
```

`elsebot db` logs in as `else_readonly`, a database role with SELECT on every table (new ones included) whose sessions are read-only; it cannot change anything, which makes it the way to inspect an installation, for example when reporting a bug.

Outside `~/.else`: the system packages, the database's Docker volume and the guest image in Docker, the systemd unit `~/.config/systemd/user/else-api.service`, and the links `~/.local/bin/elsebot` and `~/.local/share/applications/else-bot.desktop`.

### elsebot

| Command | What it does |
| --- | --- |
| `elsebot status` | Installed and running versions; whether the API is up |
| `elsebot desktop` | Open the app |
| `elsebot update` | Install the latest release (see above) |
| `elsebot start`, `stop`, `restart` | The API service; `stop` is graceful |
| `elsebot logs` | Follow the API log |
| `elsebot api backup` | Any `else-api` command with this installation's settings |
| `elsebot db` | Read-only SQL on the installation's database (`psql`), e.g. `elsebot db -c 'select count(*) from bots'` |
| `elsebot paths` | Where everything is |

### Versions and releases

There is one version for else.bot, `[workspace.package] version` in `Cargo.toml`; both binaries report it (`else-api version`, `else-desktop --version`, and `/healthz`). A release is a `vX.Y.Z` tag: `scripts/release.sh patch` (or `minor`, `major`), run from a clean `main` that matches `origin/main`, bumps the version (0.0.1 becomes 0.0.2, 0.1.0, or 1.0.0), refreshes `Cargo.lock`, commits `chore(release): vX.Y.Z`, tags it, and pushes `main` and the tag together after asking (`-y` does not ask). Installations pick it up on their next update.

`scripts/test-install.sh [debian-12|debian-13|ubuntu-24.04|ubuntu-26.04|arch]` installs and updates in fresh containers through a pipe, as an unprivileged sudo user, against releases built from the working tree (see its header).

## Try the POC

You need Rust, Docker, and a logged-in `codex` CLI (`codex login`). From the repository root:

```sh
test -e .env || cp .env.example .env
docker compose up -d --wait
cargo run -p else-api -- setup     # once: credentials, migrations, main bot
cargo run -p else-api -- serve     # leave running
cargo run -p else-desktop          # in another terminal
```

The window lists your bots. Press Ctrl+Shift+N to create one, select it, and type in the composer; Enter sends. Replies stream from Codex (GPT 6.1 Sol Medium Fast by default). Ctrl+Shift+X stops work and Ctrl+Shift+D deletes the selected bot while keeping its history. Each Codex session runs on the host in the bot's project directory, with its own shell and file tools, scoped else.bot tools, and no native sub-agents. For a throwaway installation that does not touch your data, use `scripts/demo.sh` as described in the [development guide](docs/development.md#run-the-desktop). The [smoke procedure](docs/implementation/smoke.md) walks through every POC behavior.

## Repository and local setup

The Cargo workspace separates the API and desktop client. Development services live under `infra/`.

```text
Cargo.toml
Cargo.lock
apps/
	api/          Axum API, scheduler, PostgreSQL store, provider adapters, migrations
	desktop/      GPUI desktop client
infra/
	compose.yaml  PostgreSQL
scripts/
	demo.sh       disposable demo installation
docs/
.env.example
```

Check and test the workspace (the tests need the Compose PostgreSQL; live Codex tests run only with `ELSE_LIVE_CODEX=1`):

```sh
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
```

See [local development](docs/development.md) for prerequisites, configuration, run commands, and database persistence. The [architecture notes](docs/architecture.md#repository-boundaries) explain directory ownership and the intended dependency direction.

## Implementation backlog

[TODO.md](TODO.md) is the ordered queue for implementation. The three POC milestones are marked complete, the last being [POC-025 through POC-032](docs/implementation/05-vm-automation-and-categories.md), VM automation and sidebar categories. Each task has dependencies, references, and acceptance checks.

The [agent runbook](docs/implementation/README.md) explains how to select a task, verify it, commit progress, and continue. The [handoff log](docs/implementation/progress.md) records active work and evidence across sessions.

## Design notes

The detailed notes separate requirements, decisions, proposals, and research results. Experiments and validation work are marked where they remain incomplete.

| Topic | Details |
| --- | --- |
| [Implementation backlog](TODO.md) | Ordered POC tasks, completion gates, and autonomous-agent handoff. |
| [Local development](docs/development.md) | Workspace commands, PostgreSQL setup, configuration, and database lifecycle. |
| [Research review](docs/research/README.md) | Seven research tracks, local evidence, the starting design, and remaining integration checks. |
| [Product requirements](docs/product.md) | Context recognition, delegation, built-in tools, and bot computers. |
| [Architecture](docs/architecture.md) | Rust API, desktop client, and the preference for native iOS. |
| [Providers and conversations](docs/providers.md) | Codex and Claude integration, T3 Code research, sessions, and provider switching. |
| [System prompts](docs/prompts.md) | Prompt structure, bot roles, summary and continuation drafts, and validation. |
| [Security and secrets](docs/security.md) | Prompt injection controls, isolation, the master key, and open recovery design. |
| [Routines and webhooks](docs/routines.md) | Scheduled work, event inputs, and independent webhook bearer tokens. |
| [Memory and storage](docs/memory.md) | UniiChat-based memory, saved views, and PostgreSQL verification. |
| [Bot computers](docs/sandboxes.md) | Runtime research, Rust deployment, VM locks, and resource limits. |
| [Backup design](docs/backups.md) | Database exports, VM snapshots, encryption, retention, and size estimates. |
| [Restore procedure](docs/restore.md) | Planned recovery on a new server and startup that preserves saved state. |
| [VM verification](docs/experiments/bot-vm-verification.md) | Local desktop, lifecycle, snapshot, and Rust SDK checks, including measured backup sizes. |
