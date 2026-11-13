# Desktop visual fixtures

Capture a fixed set of 24 native GPUI screenshots in an isolated Omarchy desktop.
The scripts live here; generated state, logs, builds and images live in the ignored
`.else-dev/visual-desktop/` directory. Python 3, Bash, omabox and prebuilt API and
desktop executables are required. Run commands from the repository root on Linux.

## Start, seed and capture

```bash
BOX=$(scripts/visual-desktop/up.sh)
scripts/visual-desktop/shoot.sh "$BOX" .else-dev/visual-desktop/shots/r0 --no-build
```

`up.sh` starts `scripts/dev.py run` from a local mirror using a detached process
session and redirects output to `.else-dev/visual-desktop/run.log`. It waits for
the running JSON manifest, seeds the session, and prints only the box name to
stdout. A healthy recorded session is reused and reseeded. The manifest and
launcher PID are in `session.json` and `up.pid`. The scripts leave the box running.

The default executables are `.else-dev/.build/debug/else-api` and
`.else-dev/.build/debug/else-desktop`. Set `API_BINARY` and `DESKTOP_BINARY` to use
other prebuilt executables. Set `HARNESS_STATE_DIR` to choose another evidence
directory; use the same value for every command in that session.

Reseed the current session without starting another box:

```bash
scripts/visual-desktop/reseed.sh
```

Or seed a separately managed dev session explicitly:

```bash
scripts/visual-desktop/reseed.sh box-1 /home/sbx/else-dev/SESSION
```

To rebuild the desktop before capturing, omit `--no-build`. The build stays under
the harness state directory. To capture an already built replacement, set
`HARNESS_DESKTOP_BINARY=/absolute/path/to/else-desktop` and use `--no-build`.
Both paths copy the executable into the box and relaunch it with the original
desktop environment. The API keeps running so its pending cards survive.

Use a new `HARNESS_STATE_DIR` for a fresh fixture after the previous session has
ended. Each new directory owns its own detached launcher; do not use this to
silently replace a session that someone else is using.

## Fixture and capture contract

The seed uses real owner API endpoints for categories, bots, file upload and a
running synthetic turn. Tool-only cards use stable SQLite fixture IDs with
foreign keys and existing triggers enabled. Reseeding the unmodified fixture
preserves row counts. The seed checks SQLite integrity and foreign keys, and
reads every bot conversation back through the API to check snapshot decoding.

The fixture contains seven bots, two categories, two delegations, a created-bot
card, rules and a diff, routines, a reminder, a webhook trigger, pending choice
and permission requests, a secret request, an email draft, an owner notice, a
failed turn, a real uploaded Markdown file and a formatted release plan.
Release Monitor holds a real `[synthetic:hang]` attempt; its directive is removed
from the displayed message after the attempt starts. Fitness Coach stays empty.

Capture uses a 1440×900 logical window at scale 2, producing 2880×1800 PNGs.
Dark uses `cursor-dark`; light uses `catppuccin-latte`. The script restores the
dark theme, hides the pointer, enables Do Not Disturb and clears notifications
inside the box. It resets only the disposable app profile's presentation state.
An `.incomplete` marker remains if capture fails; do not score that directory.
Use a fresh output directory for each comparison round.

| File | Intended screen |
| --- | --- |
| `dark-01-release-plan.png` | Markdown headings, numbered list, table, inline code, Rust block and uploaded file chip |
| `dark-02-bot-activity.png` | Research and review delegations, council activity, bot created, rules update and routine update |
| `dark-03-approval-and-failure.png` | Failed repository review and pending external publication permission |
| `dark-04-email-and-notice.png` | Blocked owner notice and complete outgoing email draft approval |
| `dark-05-owner-choices.png` | Pending owner question with three choices and free-text answer |
| `dark-06-secret-request.png` | Travel credentials request with context and secret input |
| `dark-07-working.png` | Release Monitor's real running synthetic turn |
| `dark-08-bot-details.png` | Upper Details panel: identity, model, rules and routines |
| `dark-09-schedules-and-triggers.png` | Lower Details panel: routines, reminder and webhook trigger |
| `dark-10-computer.png` | Computer tab and its unavailable state |
| `dark-11-empty-conversation.png` | Fitness Coach with no messages |
| `dark-12-create-bot.png` | Filled Calendar Assistant dialog with purple House avatar |
| `dark-13-settings-servers.png` | Servers page with the live local server |
| `dark-14-settings-updates.png` | App updates page for a development build |
| `dark-15-settings-email.png` | Email connection setup |
| `dark-16-settings-socials.png` | Telegram and Discord connection setup |
| `dark-17-settings-mcp.png` | MCP inventory and search |
| `dark-18-settings-skills.png` | Skills inventory and search |
| `dark-19-settings-backups.png` | Backup status, schedule and retention controls |
| `dark-20-rules-diff.png` | Rules History dialog with removed and added lines |
| `light-01-release-plan.png` | Release plan and attachment in light theme |
| `light-02-bot-details.png` | Details panel in light theme |
| `light-03-create-bot.png` | Filled create-bot dialog in light theme |
| `light-04-settings-servers.png` | Servers settings in light theme |

## Limits and layout maintenance

- Pending tool cards are screenshot fixtures without live in-memory tool grants.
  Do not approve, send or answer them. Restarting the API reconciles and cancels
  pending requests; use a fresh dev session after an API restart.
- Only the synthetic provider runs in this environment. Provider setup warnings
  in Details and Create bot are expected. The create dialog is never submitted.
- Guest desktop access is disabled by `dev.py run`; Computer captures the real
  unavailable state. No remote computer screen is seeded. Email, Socials, MCP,
  Skills and Backups show their initial setup or empty inventory states.
- The file attachment is real and downloadable; there is no image attachment.
  Elapsed times, server ports, timestamps and working animation frames vary.
  This harness supplies screenshots for visual review, not exact pixel equality.
- Shortcuts drive Settings (`Ctrl+,`), the bot panel (`Ctrl+Shift+B`), new bot
  (`Ctrl+Shift+N`), dismissal and sidebar navigation. Named AT-SPI nodes drive
  Settings navigation, House shape and History where available. GPUI currently
  reports physical bounds; `tree_point.py` divides them by `TREE_SCALE` (default 2).
- Controls missing accessible nodes use documented logical coordinates in
  `shoot.sh`: main sidebar anchor `(130,100)`, Details `(1152,98)`, Computer
  `(1230,98)`, rules summary `(1260,428)`, purple swatch `(515,474)` and Backups
  `(285,469)`. Settings has fallback rows at x=285. Transcript and panel scrolling
  also use fixed anchors and wheel deltas. Recalibrate these after layout changes
  and visually inspect every PNG before scoring.
- `up.py` checks a unique source anchor before mounting its dev-runner mirror.
  If `dev.py` changes shape, update that adapter instead of running stale code.
