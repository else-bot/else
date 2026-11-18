# else.bot verification map

| Feature | Public user entry | Fast driver | Extra proof required |
| --- | --- | --- | --- |
| [Setup and profiles](setup.md) | Connection page, welcome card, Change model | `--tests installation onboarding bot_profiles` | Native welcome card (`--welcome --fixture-provider`) and model picker |
| [Bots](bots.md) | New bot, sidebar, Bot details, delete dialog | Default `bots` lane; `--tests bots bot_deletion bot_rules` | Ctrl+Shift+N, Ctrl+Shift+B and Ctrl+Shift+D |
| [Messaging and stop](messaging.md) | Composer, stop button | Default `messaging` lane; `--tests turns steering cancel ws` | Visible streaming, joining, and Ctrl+Shift+X |
| [Categories](categories.md) | Ctrl+Shift+G, Move dialog, header menu | Default `categories` lane; `--tests categories ws` | Ctrl+Shift+G, Ctrl+Shift+M, header menu, drag |
| [Persistence and recovery](persistence.md) | Restart; backup/restore CLI; Settings → Backups | Default restart; `--tests sqlite backup backup_settings runtime_lock assistant_backup recovery memory_tree memory_summary` | Native reconnect, real VM recovery, S3 and live summaries |

The default command covers four API journeys, not the whole map. `--full`
runs existing offline workspace tests but still requires native checks for UI
changes. Use the feature files to select all affected entry points. A missing
precondition or driver is an uncovered path, never a passing shortcut. Real
provider, VM, guest browser, installer and remote-backup checks remain in the
existing live targets and `scripts/test-e2e.py`; this map does not claim them.

These user-facing surfaces have no feature file yet. Treat them as uncovered
by this map, not as verified:

- Servers: the server rail, Ctrl+1..9, the Ctrl+K quick switcher, server
  icons and updates (`apps/desktop/src/connection/server_rail.rs`,
  `apps/desktop/src/quick_switcher.rs`).
- Settings (Ctrl+,): Usage, Vault, Skills, MCP, Computers and Updates
  (`apps/desktop/src/connection/settings.rs`).
- Owner exchanges in chat: questions, permissions, outgoing drafts, secret
  requests, waits and computer handoffs (`apps/api/src/http/questions.rs`,
  `outgoing_drafts.rs`, `task_secrets.rs`, `waits.rs`). The fixture's
  `[ui:*]` markers in the [native desktop guide](../native-desktop.md) drive several.
- Routines, reminders and triggers (`apps/api/src/http/routines.rs`,
  `webhooks.rs`).
- Mail and social connectors (`apps/desktop/src/email.rs`,
  `apps/desktop/src/socials.rs`), which mostly need external accounts.

For native input on macOS without driving the owner's desktop, use
[one Lume guest per run](../lume.md). Several worktrees may use the same session
name because they have separate VMs, APIs, app bundles, and evidence. Guest
desktop verification does not establish real bot-computer behavior or a live
provider connection.
