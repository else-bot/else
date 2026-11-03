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

## Provider choice and ownership

else.bot should work with AI providers through CLIs installed on the host and your existing subscriptions. Bots operate their isolated computers through scoped tools when a task needs them. You can switch providers during a task while keeping one conversation and handing over the context needed to continue.

The starting Codex profile is GPT 6.1 Sol Medium Fast. Provider/model settings are saved in PostgreSQL and can be updated by the owner. Provider-native sub-agents are disabled for the POC. See the [execution policy](docs/providers.md#saved-execution-profile).

The assistant runs on a server or dedicated hardware at home. Its memory, configuration, conversations, and bot computers belong to the owner. Encrypted backups should let you restore the assistant on a new machine and resume from the latest completed backup.

## Try the POC

You need Rust, Docker, `bubblewrap`, and a logged-in `codex` CLI (`codex login`). From the repository root:

```sh
test -e .env || cp .env.example .env
docker compose up -d --wait
cargo run -p else-api -- setup     # once: credentials, migrations, main bot
cargo run -p else-api -- serve     # leave running
cargo run -p else-desktop          # in another terminal
```

The window lists your bots. Press Ctrl+Shift+N to create one, select it, and type in the composer; Enter sends. Replies stream from Codex (GPT 6.1 Sol Medium Fast by default). Ctrl+Shift+X stops work and Ctrl+Shift+D deletes the selected bot while keeping its history. Each Codex session runs in a bubblewrap sandbox with scoped else.bot tools, no native host shell, and no native sub-agents. For a throwaway installation that does not touch your data, use `scripts/demo.sh` as described in the [development guide](docs/development.md#run-the-desktop). The [smoke procedure](docs/implementation/smoke.md) walks through every POC behavior.

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

Check and test the workspace (the tests need the Compose PostgreSQL and `bwrap`; live Codex tests run only with `ELSE_LIVE_CODEX=1`):

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
