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

The installer checks for these CLIs but does not install them. Provider CLIs run as your user and can read your host files. Bot computers are isolated VMs.

## Open the app

Open else.bot from your app launcher, from `~/Applications/else.bot.app` on macOS, or run:

```sh
elsebot desktop
```

Next, read [Getting started](getting-started.md).
