# Security and secrets

These are security requirements and proposed controls. They have not been implemented or validated in else.bot. [System prompt drafts](prompts.md) describe model behavior, and [webhook authorization](routines.md#webhook-authorization) defines routine access.

## First-release authentication

Use one owner bearer token and a loopback-only Rust API. Provision a random 32-byte token independently of `SECRET_KEY` and provider credentials. Keep it in a private file, load its digest at startup, and require it on every owner route and message stream. The desktop can use an explicit private token file during development. OS keyring integration can follow without changing the protocol.

For a VPS or home server, the app reaches the loopback API through an authenticated SSH tunnel or an HTTPS reverse proxy that rewrites `Host` to the loopback address (the [README](../README.md#connect-the-app-to-a-server) has a Caddy example). The API itself keeps its loopback bind and `Host` check. The desktop sends the owner token over plain HTTP only to a loopback origin, verifies certificates, and follows no redirects. It saves the origin and token in a private `0600` profile. Development must not add an anonymous network-access mode.

Provider sessions receive short-lived tool grants bound to a bot, conversation, task, and fixed allowlist. The API derives identity from the grant and checks current state before dispatch. Configured delegation and memory access specify permitted target bots. The owner token, database credentials, and `SECRET_KEY` stay outside the provider process. No user accounts, OAuth service, role editor, or general policy engine are needed initially.

Deleting a bot records a tombstone, revokes grants, cancels active work, and stops its computer. Retain its chats, memory, and saved files for a later explicit purge. A late result must not restore the bot or restart its VM.

Development agents work under the owner's authorization for their assigned repository and disposable development services. That authorization is separate from runtime bot permissions. The [authentication research](research/authentication-and-permissions.md) records the proposed setup, task checks, development policy, and 39 passing policy-model checks. Actual route enforcement, keyring behavior, and host process isolation remain unverified.

## Prompt injection protections

Prompt injection protection is a system requirement. A system prompt alone cannot guarantee that injection is impossible. Separating instructions from data helps the model, while authorization and tool restrictions must also operate outside it. [OWASP prompt injection guidance](https://cheatsheetseries.owasp.org/cheatsheets/LLM_Prompt_Injection_Prevention_Cheat_Sheet.html).

For else.bot, apply these controls:

- Treat emails, web pages, files, webhook payloads, tool results, other bots' messages, and generated summaries as external or derived content. Their text cannot grant permissions or replace the owner's instructions.
- Preserve source and trust information when saving memories and building summaries. Recalling an email or receiving a provider handoff must not turn its instructions into owner-approved preferences.
- Have the Rust API check tool arguments, bot identity, resource access, and routine scope before execution. Delegation must preserve the originating task's restrictions. Another bot's wider access must not let a routine expand its permissions.
- Enforce the owner's action policy in code. Any required approval must cover the actual action and target. Content claiming that the owner approved something is not an approval record.
- Keep native CLI tools, filesystem access, and network access within the same limits. A terminal or browser must not provide a route around connector permissions. Keep master keys and infrastructure credentials outside bot environments. (Provider CLIs no longer meet this on the host: their shell and file tools run with the owner's access; see [host provider processes](#host-provider-processes).)

These are proposed controls for else.bot, informed by [OWASP's agent security guidance](https://cheatsheetseries.owasp.org/cheatsheets/AI_Agent_Security_Cheat_Sheet.html). Their enforcement needs validation with the chosen CLI and sandbox runtime.

## Isolation and remote access

The proposed deployment should give each bot its own guest filesystem and browser profile. Native provider session files stay in separate bot directories on the host. The one host mount is the bot's own project directory, mounted at `~/work`. Cross-bot memory access should continue through else.bot's skills and API.

For the prototype, the runtime configuration should enforce these boundaries:

- Keep the host home directory, host desktop sockets, and Docker socket out of bot environments. A trusted service outside the sandbox manages its lifecycle. Docker documents the privileges involved in controlling its daemon. [Docker Engine security](https://docs.docker.com/engine/security/).
- Keep host runtime management separate from privileges inside the guest. For container alternatives, use a non-root user, avoid privileged containers, and retain syscall restrictions. [Container seccomp profiles](https://docs.docker.com/engine/security/seccomp/).
- Separate bot networks. Allow required public web traffic and authenticated access to else.bot's API. Block direct access to other bot environments, host administration services, private networks, and cloud metadata endpoints unless explicitly configured.
- Set CPU, memory, process, and disk limits per bot so one task cannot consume the whole server.
- Keep VNC, browser debugging, shell, and file APIs private. Route desktop access through an authenticated TLS proxy that checks which bot the owner is opening. Use separate credentials for each bot and replace example passwords.

Create bot environments with their own workspace. Any host mount must be explicit and limited to what the bot needs. Provide each bot's skills from else.bot's configuration and register only intended MCP integrations.

For VNC access, the proposed route is the client, else.bot's authenticated TLS proxy, a private connection to the guest, and noVNC inside the bot's VM. Validate port forwarding, WebSocket streaming, and reconnects with the chosen runtime. Desktop access still requires else.bot authentication.

These controls limit access to the host and other bot environments. They do not prevent a bot from misusing data or connectors it can already access. Those permissions still belong in else.bot's API.

## Host provider processes

Provider CLIs run directly on the host, as the API's user, with their own shell and file tools. This is a deliberate choice: the owner asks some bots to work on the machine itself (a bot that keeps the spreadsheets in `~/Documents/sheets`, a bot that changes the system's configuration), which a sandbox around the CLI cannot allow. The [decision record](decisions/0001-providers-run-on-the-host.md) explains the choice and the alternatives considered.

Each bot has a project directory on the host, `<ELSE_BOTS_DIR>/<bot id>` (`~/.else/bots/<bot id>` in an installation, `$ELSE_DATA_DIR/projects/<bot id>` by default). Development sessions set a private projects directory for each checkout and session. Every provider run starts there, and the same directory is mounted at `~/work` (`/home/elsebot/work` with the desktop image) in the bot's computer, so the host and the VM share one workspace. By default a bot does its work in its computer: code, scripts, routines, background jobs, and browsing. It acts on the host outside its project directory only when the owner, in their own words, asks for that work on their machine. Learned responsibilities, delegated tasks, and tool output grant no host access. That rule is stated in the bot's instructions, not enforced by the operating system.

The original host execution requirements were:

- Run provider processes with explicit working directories, isolated bot session storage, and an allowlist of environment variables. Keep `SECRET_KEY`, database administration credentials, backup keys, storage credentials, and unrelated host files inaccessible.
- Use the intended provider account through supported authentication. Keep authentication material outside VMs and prompt text. Sharing one account must not expose another bot's conversation files or settings.
- Configure or disable native shell, file, browser, network, and MCP capabilities that bypass else.bot's scoped tools. Apply operating-system restrictions to host files and processes. An approval prompt or a prompt instruction alone is insufficient.
- Enforce the [native sub-agent prohibition](providers.md#provider-native-sub-agents-are-deferred) at launch and resume. Expose no equivalent spawning route through built-in tools, plugins, hooks, or MCP. Only the API may schedule work for another registered bot.
- Give each CLI session scoped tool authorization tied to its bot, conversation, and task. The Rust API checks permissions and guest targets before dispatch. VM lifecycle privileges belong to the trusted service.
- Treat guest files, tool output, and callbacks as untrusted input. A guest cannot request arbitrary host commands, choose host paths, or obtain runtime management credentials.

The first and third requirements are dropped on purpose: host files are not made inaccessible, and the native shell and file tools are enabled without an operating-system restriction (POC-010 enforced both with bubblewrap until this change). The others still hold: separate session storage per bot, the supported shared login kept out of VMs and prompts, no native sub-agents, scoped task grants for else.bot's tools, and guest output treated as data.

What this means:

- A provider process can read and change anything the API's user can: the API's data directory (owner token, master key file, credentials, other bots' provider homes), the owner's files, and other bots' project directories. `SECRET_KEY` and `DATABASE_URL` are not in its environment, but the key file is readable.
- Protection against prompt injection (a web page, email, or file telling the bot to run a command) now rests on the model and its instructions for anything done with host tools. else.bot's own tools still check their task grant in the API.
- Network access is unrestricted, as for any program the user runs.

### Implemented host launch

Implemented in `apps/api/src/providers/host.rs`, `codex_home.rs`, and `claude_home.rs`:

| Control | Effect |
| --- | --- |
| Working directory `<ELSE_BOTS_DIR>/<bot id>` (mode 0700) | Every run starts in the bot's project, which its computer mounts at `~/work`. Native sessions are fingerprinted with this path, so a session never resumes in another directory. |
| Private provider home `<data dir>/bots/<bot id>/<provider>-home` (`CODEX_HOME`; Claude's settings and MCP files) | Codex's sessions, caches, and settings stay per bot and outside the project, so they never reach the computer. Configuration files are replaced atomically before each launch, never written through a planted symlink. |
| Codex: host `auth.json` symlinked into the provider home | The supported subscription login is shared without copying; token refreshes write through to the one file. |
| Claude: the host's own configuration directory (`~/.claude`, or the API's `CLAUDE_CONFIG_DIR`) | The login works wherever Claude Code keeps it (the Keychain on macOS). Native sessions are stored per project directory, so each bot resumes only its own; the adapter always names the session. The owner's settings, hooks, plugins, and `CLAUDE.md` are not loaded (`--setting-sources ""`, else.bot's system prompt). |
| `env_clear`, then `HOME`, `USER`, `LOGNAME`, `SHELL`, `PATH`, `LANG`, `LANGUAGE`, `LC_*`, `TZ`, `TMPDIR`, `TERM`, `XDG_*` directories, `DBUS_SESSION_BUS_ADDRESS`, `DISPLAY`, `WAYLAND_DISPLAY`, `SSH_AUTH_SOCK` from the API's environment, plus the launch's own (`CODEX_HOME` or `CLAUDE_CONFIG_DIR`, `ELSE_TASK_GRANT`) and the marker | Host work sees the owner's session (home, tools, desktop bus); the API's own settings (`DATABASE_URL`, `SECRET_KEY`, the `.env` values) are not passed. This is hygiene, not isolation. |
| Own process group, `ELSE_BOT_PROCESS` marker | Stop work and the turn's end terminate the whole group. Leftovers from a crashed API are found at startup by the installation marker in their environment, killed, and confirmed exited before reconciliation. On Linux they are found through `/proc` and pinned with a pidfd before the marker is re-checked, so a reused PID is never signalled. On macOS they are listed with `proc_listallpids`, their environment read with `KERN_PROCARGS2`, and each pinned by PID and start time, checked again right before the signal and while waiting, which leaves only that instant for a reused PID. Nothing ends a run when the API dies. |

Provider CLI versions do not determine availability. Startup checks that the executable runs and the host login is present. Each turn enforces the protocol and policy checks below.

The task grant reaches the CLI only through its environment, never its command line. Before each turn the adapter asks Codex for its effective MCP servers and refuses to run unless they are exactly else.bot's `elsebot` server offering exactly its tools (POC-013). The JSON-RPC client bounds message size and backlog, applies one deadline to writing and waiting, and refuses every server request (approvals, tool calls, user input) itself.

**Codex** (tested with 0.166): a turn's configuration sets approval `never`, sandbox `danger-full-access`, `shell_tool` on, web search disabled, `[agents] enabled = false`, one MCP server (`elsebot`, `enabled_tools` limited to else.bot's tools, the bearer token read from the environment), and features off for multi-agent (v1 and v2), apps, plugins, hooks, browser, computer use, image generation, and related tools. For `gpt-6.1-sol`, Codex calls MCP tools from JavaScript run by `codex-code-mode-host`, found beside the codex executable. Commands and file changes are allowed items; a web search, image, dynamic tool, an MCP call outside the allowlist, a native agent's thread or item, a model reroute, or any server request still ends the turn as `policy_violation`. Memory summaries (POC-015) keep the earlier lockdown: sandbox `read-only`, shell off, no MCP server, an ephemeral thread in a separate per-bot home, and any tool, command, file, or agent item or server request rejects the output. The live check `ELSE_LIVE_CODEX=1 cargo test -p else-api --test codex_live` asks the model to run `pwd`, write a file, ask a user-input question, and spawn two sub-agents: commands and the file write run in the project directory, no approval is requested, and no agent thread or item appears.

**Claude Code** (tested with 2.1.316, POC-020): a turn runs with `--tools` set to the built-in shell and file tools (`Bash`, `Read`, `Write`, `Edit`, `NotebookEdit`, `Glob`, `Grep`), those and else.bot's tools pre-approved by name (`--allowedTools`), `--permission-mode dontAsk`, no user, project, or local settings (`--setting-sources ""`), hooks and slash commands disabled, and `--strict-mcp-config` with only else.bot's server (its bearer header names `ELSE_TASK_GRANT`). `Agent`, `Task`, `TaskStop`, web, skill, and slash-command tools are absent and also denied in the settings. Each turn's `system/init` must report exactly those tools and server and the saved model; any other tool use, a permission request (denied), a task lifecycle event, an event attributed to a parent tool use, or another model fails the turn. With these flags the pre-approved tools also act outside the working directory without a prompt (checked with Claude Code 2.1.311). Handoff checkpoints run with no tools and no MCP server, verified from `init`. The login is checked at startup with `claude auth status`; on macOS it is the Keychain login of the owner's terminal.

Task grants are opaque `tg_` tokens held as digests in API memory, bound to one bot, conversation, and turn with a tool allowlist. They are revoked when the turn ends, on Stop work and deletion, and by any restart. The task-scoped tool API lives at `/tools/v1` and accepts only grants; owner routes answer a grant with `403 task_grant_forbidden`, and tool routes answer the owner token with `403`. Every tool call rechecks that the turn is still sending or running and its bot active. Identity comes only from the grant: unknown query fields are rejected and another bot's objects return `404`. Each turn's grant allows reading its conversation history and memory (POC-013); a cross-bot read additionally needs the owner's memory-access grant, and tool calls can neither write memory nor create owner corrections. These checks bound else.bot's tools only: a provider process could read the owner token from the data directory and call owner routes directly, so they are not a boundary against a misbehaving model.

## Master key and encrypted secrets

The initial deployment should read an installation master key from the Rust API's environment:

```dotenv
SECRET_KEY=<base64-encoded 32 cryptographically random bytes>
```

Provision this key once and preserve it across restarts and migrations. Keep it outside SQLite, source control, and ordinary application configuration exports. Supply it only to the trusted API and its backup worker. Provider CLIs and bot VMs do not inherit it, but provider CLIs run as the same user on the host and can read a key kept in the data directory (see [host provider processes](#host-provider-processes)).

Derive separate keys for recoverable database secrets and backup encryption using HKDF-SHA-256 with distinct purpose labels, such as `else.bot/secrets/v1` and `else.bot/backups/v1`. Use Rust `age` streaming encryption for backup artifacts, initially pinned to the tested `0.12.1`. Derive its per-set identity with a fresh public salt and the backup purpose label. Preserve salts, format versions, and key identifiers with encrypted data. The [backup research](research/backup-and-recovery.md#encryption-and-key-separation) specifies that format. Selection of the database-secret encryption library remains open. [HKDF purpose separation](https://www.rfc-editor.org/rfc/rfc5869.html#section-3.2), [authenticated encryption and key separation](https://cheatsheetseries.owasp.org/cheatsheets/Cryptographic_Storage_Cheat_Sheet.html).

The backup encryption covers SQLite exports, VM archives, and recovery manifests before upload. Include the encryption format and key identifier in the outer backup metadata so recovery can select the key before decrypting the manifest.

Startup must reject a missing or invalid key. A key that cannot decrypt existing state must produce an error without replacing the key or resetting that state.

The owner's recovery mechanism will be designed later. Restoring encrypted data requires the corresponding master key, recovered separately from the encrypted backup. Losing both the server's key and its separate recovery copy makes that data unrecoverable. Key rotation must retain access to every key version still needed by stored secrets or retained backups.

## Open design work

- How should the owner recover and rotate `SECRET_KEY` while keeping retained backups restorable?

The [backup design](backups.md) and [planned restore procedure](restore.md) depend on access to the corresponding key versions.
