# else.bot

else.bot is a personal AI assistant with a native desktop app. Chat with your assistant, create bots for different tasks, and switch between Codex and Claude Code. Bots keep their conversations and memory, delegate work, and use their own computers with a browser and terminal.

The server stores your data on your machine. Provider CLIs run as your user and can access your host files. Bot computers are isolated VMs; the provider processes themselves have your user account's permissions.

## Install

Supported hosts are Linux with Debian 12+, Ubuntu 24.04+, or Arch Linux / Omarchy on AMD64 or ARM64, and macOS on Apple silicon. macOS installations need Git and, for the app bundle, Python 3. Linux bot computers need access to `/dev/kvm`.

Run as your normal user:

```sh
curl -fsSL https://raw.githubusercontent.com/else-bot/else/main/install.sh | bash
```

Choose what this machine runs:

- Both the server and app, the default for a single machine.
- Server only, for home hardware or a VPS.
- App only, to connect to a server elsewhere.

To choose without the prompt, append `-s -- --both`, `-s -- --server-only`, or `-s -- --app-only` to `bash`.

The installer downloads the selected release binaries into `~/.else` and verifies their SHA256 checksums and versions. It asks before installing missing runtime packages and whether to start the server automatically. It also installs Microsandbox and downloads the bot computer image for server installations. App-only installations do not need the VM runtime or provider CLIs. Rust, compiler packages, and Xcode's Metal toolchain are needed only with `--from-source`, including when installing an older release without prebuilt binaries.

Install the Codex CLI or Claude Code on the server and sign in as the user who runs else.bot. For Codex, run `codex login`. For Claude Code, run `claude` and finish sign-in. The installer checks for these CLIs but does not install them.

On Linux, log out and back in if the installer adds you to the `kvm` group. On macOS, bot computers use Apple's Hypervisor framework.

## Start chatting

Open else.bot from your launcher, `~/Applications/else.bot.app` on macOS, or:

```sh
elsebot desktop
```

For a local server, choose **Start local**. The app reads this installation's owner key and starts the server if needed. Complete the welcome screen to name your assistant and choose its provider and model.

Select a bot and type a message. Enter sends; Shift+Enter adds a line. Use the bot settings to change its provider or model, and open its computer to see its desktop. Closing the app leaves the server running.

Use Ctrl on Linux and Command on macOS for these shortcuts:

| Shortcut | Action |
| --- | --- |
| Ctrl/Command+Shift+N | Create a bot |
| Ctrl/Command+Shift+X | Stop the selected bot's work |
| Ctrl/Command+Shift+B | Open bot settings |
| Ctrl/Command+Shift+G | Create a sidebar category |
| Ctrl/Command+Shift+M | Move a bot to a category |
| Ctrl/Command+, | Open app settings |

## Connect Telegram or Discord

Ask your assistant to connect Telegram or Discord and say what you want it to do. For example, "Connect Telegram so you can summarize messages that mention you and draft replies." The assistant opens a private connection card in the conversation.

For Telegram, create a bot with [@BotFather](https://t.me/BotFather) using `/newbot`. For Discord, create an application in the [Developer Portal](https://discord.com/developers/applications), copy its bot token, and install the bot in your server. Enter the token in the card's private field. Tokens authenticate the platform bot, and the API stores them encrypted. The assistant does not receive them. Keep tokens out of ordinary chat messages.

Choose the conversations the assistant may access and describe its work in the instructions field. These instructions teach the assistant how to behave. They cannot grant access beyond the selected destinations and permissions. Sending a message still requires your approval of its draft.

A Telegram bot receives messages in chats it joins, subject to Telegram's privacy settings. It does not connect your personal inbox or import arbitrary older chat history. Add the bot to a group and send a message to make that destination available. A Discord bot can access only channels allowed by its server permissions. Reading ordinary server message content also requires the Message Content Intent in the Developer Portal.

Open **Settings → Socials** to manage connections, edit each bot's instructions and access, replace a token, pause a connection, or disconnect it.

## Connect to another machine

The server listens on `127.0.0.1:7878`. Use an SSH tunnel to reach it from your app machine:

```sh
ssh -N -L 7878:127.0.0.1:7878 you@server
```

Keep the tunnel open. On the server, read the owner key:

```sh
cat ~/.else/data/owner-token
```

In the app, choose **Edit configuration**, enter `127.0.0.1:7878`, paste the key, and choose **Connect to server**. The app saves a connection after the server accepts it. Use **Change server** to connect elsewhere.

For access through a domain, put an HTTPS reverse proxy in front of the server. It must forward HTTP and WebSocket requests to `127.0.0.1:7878` and rewrite the upstream `Host` header to that address. The app requires HTTPS for non-loopback addresses and verifies the server certificate. Keep the owner key private; it grants access to your installation.

## Update and manage the server

```sh
elsebot update
```

Updates require bots to finish their current work first. If an update reports that the server is busy, run it again later. The installer keeps your settings and data, backs up before database changes, and rolls back if the new server fails to start.

| Command | Action |
| --- | --- |
| `elsebot status` | Show installed and running versions |
| `elsebot start` | Start the server |
| `elsebot stop` | Stop the server and running turns |
| `elsebot restart` | Restart the server |
| `elsebot logs` | Follow server logs |
| `elsebot paths` | Show installation paths |
| `elsebot db` | Open the database for read-only SQL |

## Data and backups

Conversations, memory, credentials, and bot computer state live under `~/.else/data`. Bot project files live under `~/.else/bots`. Server settings are in `~/.else/config/.env`; the app saves its connection in `~/.else/config/desktop.json`.

The server makes encrypted backups daily and keeps seven completed sets by default in `~/.else/data/backups`. To make one manually, stop the server first:

```sh
elsebot stop
elsebot api backup
elsebot start
```

Keep a separate, secure copy of `~/.else/data/secret-key` with your recovery material. Backups need that key to restore. Copy completed backup sets off the machine. To see the restore commands and their arguments, run `elsebot api --help`.

For source builds, development sessions, tests, and releases, see [CONTRIBUTING.md](CONTRIBUTING.md).
