# 0001: Provider CLIs run on the host

Status: accepted (2026-11-03). Supersedes the bubblewrap provider sandbox of POC-010.

## Context

Codex and Claude Code ran inside a bubblewrap sandbox: a private `/home`, a tmpfs `/etc`, read-only system directories, only the bot's own directories writable, and no native shell or file tools at all. All real work went through else.bot's tools into the bot's computer (its Microsandbox VM). That kept the API's secrets and the owner's files out of reach, but it also blocked work the owner wants from some bots:

- a bot that keeps the owner's spreadsheets, which must read and edit files in `~/Documents/sheets` once told where they are;
- a bot that manages the owner's operating system, which must change its configuration when asked.

Neither is possible from a VM or from a sandbox that hides the owner's home. Bubblewrap is also Linux-only, so on macOS the providers could not run at all.

## Decision

Provider CLIs run directly on the host as the API's user, the way coding-agent desktop apps (for example T3 Code) run them, with their own shell and file tools enabled and never asking for approval.

- Each bot has a project directory, `<ELSE_BOTS_DIR>/<bot id>` (`~/.else/bots/<bot id>` in an installation, `~/.else-dev/bots/<bot id>` in a development checkout). Every provider run and native session starts there.
- The same directory is mounted at `~/work` in the bot's computer (`msb create --mount-dir`), so it is one workspace on the host and in the VM.
- By default a bot works in its computer: code, scripts, routines, cron-like and background jobs, and Chromium. It may use host tools on its own project directory, and acts on the rest of the host only when the owner asks for work on their machine or its responsibilities say so. This is stated in the bot's instructions, not enforced by the operating system.
- What stays enforced: per-bot provider homes (sessions, settings) in the API's data directory; the shared subscription login linked, never copied; an environment allowlist that drops the API's own settings; no native sub-agents, web, plugins, or other MCP servers; else.bot's tools behind per-turn task grants; memory summaries and handoff checkpoints with no tools at all.

## Alternatives considered

- **Keep bubblewrap.** Safe, but it rules out the host work above and leaves macOS without providers.
- **A per-bot "host access" toggle, off by default.** Bots without it would keep the lockdown. It needs owner settings, API and desktop work, and two tool policies to verify; the owner preferred instructions only, so every bot can help on the host when asked.
- **Seatbelt (`sandbox-exec`) on macOS.** It would restore isolation on macOS only, and would have the same conflict with host work.
- **A development-only flag to run without a sandbox.** Briefly implemented for macOS; it would have kept two launch paths and still blocked host work in installations.

## Consequences

- A provider process can read and change anything the API's user can, including the API's data directory (owner token, master key file, credentials), the owner's files, and other bots' project directories. A bot that follows an injected instruction can act with the owner's access; what limits it is the model and its instructions. See [security](../security.md#host-provider-processes).
- else.bot's task grants still bound its own tools, but they are no longer a boundary against a misbehaving provider process, which could read the owner token.
- Nothing ends a provider run when the API dies; leftovers are swept at the next startup (Linux through `/proc` and pidfds, macOS through libproc, pinned by start time).
- The installer no longer installs or checks bubblewrap and sets `ELSE_BOTS_DIR` (kept if the owner changed it).
- Backup sets include each bot's project directory (`project-<bot>.tar.age`), and a restored computer is bound to its restored project; a computer created with the mount cannot be restored without that binding.
- Only computers created after this change get the mount. An image upgrade recreates the computer with the mount; files in an older computer's `~/work` are unpacked into the new computer's mount, which moves them into the host project directory.
- Claude Code runs with the host's own configuration directory, as in a terminal, so its login works on macOS too (the Keychain entry is tied to that directory). Its native sessions sit in `~/.claude/projects/`, one directory per bot project.
- Native sessions created before this change do not resume: their fingerprint named the sandbox runtime. Conversations continue from PostgreSQL.
