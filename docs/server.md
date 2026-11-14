# Manage the server

## Commands

| Command | Action |
| --- | --- |
| `elsebot update` | Install the newest release |
| `elsebot status` | Show installed and running versions |
| `elsebot start` | Start the server |
| `elsebot stop` | Stop the server and running turns |
| `elsebot restart` | Restart the server |
| `elsebot logs` | Follow server logs |
| `elsebot paths` | Show installation paths |
| `elsebot db` | Open the database for read-only SQL |
| `elsebot reset` | Erase local data and return to first-run setup |
| `elsebot remove` | Uninstall everything on this machine |

## Update

```sh
elsebot update
```

An update waits for bots to finish their current work. If it reports that the server is busy, run it again later. The installer keeps your settings and data, backs up the database before changing it, and rolls back if the new server fails to start.

You can also update the app and each connected server from **Settings → Updates**.

## Where data lives

| Path | Content |
| --- | --- |
| `~/.else/data` | Conversations, memory, credentials and bot computer state |
| `~/.else/bots` | Bot project files |
| `~/.else/config/.env` | Server settings |
| `~/.else/config/desktop.json` | The app's saved connection |

## Backups

The server makes an encrypted backup every day and keeps the last seven in `~/.else/data/backups`. Change the schedule and retention in **Settings → Backups**.

To make one by hand, stop the server first:

```sh
elsebot stop
elsebot api backup
elsebot start
```

Restoring needs `~/.else/data/secret-key`. Keep a copy of it somewhere safe, apart from the backups, and copy finished backups off the machine. Run `elsebot api --help` for the restore commands.

## Reset

`elsebot reset` erases this machine's local state so you can start over. Quit the app first. Type `reset` at the prompt, or pass `--yes`.

Reset deletes conversations, memory, credentials, bot projects, computers, backups, logs and desktop preferences. It also removes Claude session transcripts for this installation's bot projects. It stops the server and leaves it stopped. Open the app and choose **Start local** to begin again.

Reset keeps the installed software, the service registration, `config/.env`, your provider CLI logins, remote servers and any backup copies you made elsewhere. It refuses custom storage paths outside `~/.else`.

## Uninstall

`elsebot remove` closes the app, stops the server and bot computers, and deletes the installation, services, launcher entry and all local data. Type `remove` at the prompt, or pass `--yes`.

It keeps shared system packages, provider CLI logins, remote servers and remote backups.
