# else.bot verification map

| Feature | Public user entry | Fast driver | Extra proof required |
| --- | --- | --- | --- |
| [Setup and profiles](setup.md) | Welcome screen, profile picker | `--tests installation onboarding bot_profiles` | Native welcome/profile picker |
| [Bots](bots.md) | New bot, sidebar, delete dialog | Default `bots` settings lane; `--tests bots` | Ctrl+Shift+N and Ctrl+Shift+D |
| [Messaging and stop](messaging.md) | Composer, Stop work | Default `messaging` lane; `--tests turns cancel ws` | Visible streaming and Ctrl+Shift+X |
| [Categories](categories.md) | New category, category picker | Default `categories` lane; `--tests categories ws` | Ctrl+Shift+G, Ctrl+Shift+M, sidebar menu |
| [Persistence and recovery](persistence.md) | Restart; backup/restore CLI | Default restart; `--tests sqlite backup assistant_backup recovery memory_tree memory_summary` | Real VM recovery, S3 and live summaries |

The default command covers four API journeys, not the whole map. `--full`
runs existing offline workspace tests but still requires native checks for UI
changes. Use the feature files to select all affected entry points. A missing
precondition or driver is an uncovered path, never a passing shortcut. Real
provider, VM, guest browser, installer and remote-backup checks remain in the
existing live targets and `scripts/test-e2e.py`; this map does not claim them.
