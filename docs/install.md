# Install

## Requirements

| Host | Supported |
| --- | --- |
| Linux | Debian 12+, Ubuntu 24.04+, Arch Linux or Omarchy, on AMD64 or ARM64 |
| macOS | Apple silicon. Git is required, and Python 3 is needed for the app bundle |

Bot computers on Linux need access to `/dev/kvm`. On macOS they use Apple's Hypervisor framework.

## Run the installer

Run it as your normal user, not root:

```sh
curl -fsSL https://raw.githubusercontent.com/else-bot/else/main/install.sh | bash
```

The installer asks what this machine runs:

- Both the server and the app. Pick this for a single machine.
- Server only, for home hardware or a VPS.
- App only, to connect to a server on another machine.

To skip the prompt, append `-s -- --both`, `-s -- --server-only` or `-s -- --app-only` to `bash`.

The installer picks the newest stable release, downloads its binaries into `~/.else`, and checks their SHA256 checksums and versions. If a release has no binaries yet, it builds that tag from source.

| Flag | Effect |
| --- | --- |
| `--from-source` | Always build locally. Needs Rust and a C toolchain, plus Xcode's Metal toolchain for the macOS app |
| `--version X.Y.Z` | Install a specific release |

The installer asks before it installs missing system packages, and asks whether to start the server at login. Server installs also get Microsandbox and the bot computer image. App-only installs need neither, and need no provider CLI.

On Linux, log out and back in if the installer added you to the `kvm` group.

## Sign in to a provider

Bots run on a provider CLI installed on the server. Install at least one and sign in as the user who runs else.bot:

| Provider | Sign in |
| --- | --- |
| [Codex CLI](https://developers.openai.com/codex/cli) | `codex login` |
| [Claude Code](https://claude.com/product/claude-code) | Run `claude` and finish sign-in |
| Grok Build | `grok login` |
| [Cursor](https://cursor.com) | Add a Vault entry with an API key (see below) |

The installer checks for these CLIs but does not install them. Provider CLIs run as your user and can read your host files. Bot computers are isolated VMs.

### Cursor

Cursor runs through Cursor's own SDK instead of a CLI. It needs Node.js 22.13 or newer on the server, with `npm` beside it. Set `ELSE_NODE_BIN` when the right `node` is not first on the server's `PATH`.

1. Create an API key at [cursor.com/dashboard](https://cursor.com/dashboard).
2. In else.bot, open **Settings › Vault** and add an entry named `provider-cursor` with the variable `CURSOR_API_KEY` set to the key.

The server then installs the pinned `@cursor/sdk` with npm into its data directory, once, and lists your account's models within the hour, or at the next restart. else.bot does not ship the SDK, because its license does not allow it. If the install fails, the failed turn shows the `npm install` command to run by hand. Bots cannot read Vault entries whose names start with `provider-`, so the key never reaches their commands.

Cursor turns load no Cursor settings, rules or MCP servers from your account or the bot's project. They get else.bot's tools, plus the shell and file tools on turns you start. Turns started by an incoming event get only else.bot's tools. Subagents are always off.

## Open the app

Open else.bot from your app launcher, from `~/Applications/else.bot.app` on macOS, or run:

```sh
elsebot desktop
```

Next, read [Getting started](getting-started.md).
