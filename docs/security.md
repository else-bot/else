# Security and secrets

These are security requirements and proposed controls. They have not been implemented or validated in else.bot. [System prompt drafts](prompts.md) describe model behavior, and [webhook authorization](routines.md#webhook-authorization) defines routine access.

## First-release authentication

Use one owner bearer token and a loopback-only Rust API. Provision a random 32-byte token independently of `SECRET_KEY` and provider credentials. Keep it in a private file, load its digest at startup, and require it on every owner route and message stream. The desktop can use an explicit private token file during development. OS keyring integration can follow without changing the protocol.

For a VPS, start with an authenticated SSH tunnel to the same loopback API. Direct network deployment requires HTTPS. Development must not add an anonymous network-access mode.

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
- Keep native CLI tools, filesystem access, and network access within the same limits. A terminal or browser must not provide a route around connector permissions. Keep master keys and infrastructure credentials outside bot environments.

These are proposed controls for else.bot, informed by [OWASP's agent security guidance](https://cheatsheetseries.owasp.org/cheatsheets/AI_Agent_Security_Cheat_Sheet.html). Their enforcement needs validation with the chosen CLI and sandbox runtime.

## Isolation and remote access

The proposed deployment should give each bot its own guest filesystem and browser profile. Native provider session files stay in separate bot directories on the host. Cross-bot memory access should continue through else.bot's skills and API.

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

Provider CLIs run on the host. The micro-VM isolates guest tasks, while a separate restriction must contain each host CLI process. A skill instructing the model to use guest tools does not enforce that boundary.

The proposed host execution requirements are:

- Run provider processes with explicit working directories, isolated bot session storage, and an allowlist of environment variables. Keep `SECRET_KEY`, database administration credentials, backup keys, storage credentials, and unrelated host files inaccessible.
- Use the intended provider account through supported authentication. Keep authentication material outside VMs and prompt text. Sharing one account must not expose another bot's conversation files or settings.
- Configure or disable native shell, file, browser, network, and MCP capabilities that bypass else.bot's scoped tools. Apply operating-system restrictions to host files and processes. An approval prompt or a prompt instruction alone is insufficient.
- Enforce the [native sub-agent prohibition](providers.md#provider-native-sub-agents-are-deferred) at launch and resume. Expose no equivalent spawning route through built-in tools, plugins, hooks, or MCP. Only the API may schedule work for another registered bot.
- Give each CLI session scoped tool authorization tied to its bot, conversation, and task. The Rust API checks permissions and guest targets before dispatch. VM lifecycle privileges belong to the trusted service.
- Treat guest files, tool output, and callbacks as untrusted input. A guest cannot request arbitrary host commands, choose host paths, or obtain runtime management credentials.

The experiment must determine which supported CLI controls and host process restrictions satisfy these requirements while preserving subscription login and structured communication. If either CLI retains unrestricted host execution, this design has not met its isolation requirement. [Provider integration validation](providers.md#integration-validation) records the checks.

### Implemented Codex launch profile

Implemented in POC-010 for Codex 0.166.0 (`apps/api/src/providers/host.rs` and `codex_home.rs`). Every provider process runs under bubblewrap:

| Control | Effect |
| --- | --- |
| `--unshare-pid --unshare-ipc --unshare-uts --new-session` | The sandbox sees only its own processes and cannot signal or inspect the API or other bots. |
| `--die-with-parent`, own process group, `ELSE_BOT_SANDBOX` marker | The sandbox ends with its supervisor. Leftovers from a crashed API are found by scanning `/proc` for the installation marker, pinned with a pidfd, re-verified, killed, and confirmed exited at startup before reconciliation; a reused PID is never signalled. |
| Read-only `/usr` and the provider binary; a tmpfs `/etc` with only the resolver, hosts, NSS, user, time zone, linker, and certificate files | System programs are readable but immutable. Secrets or provider configuration under `/etc` (for example `/etc/codex` or an `/etc/else-bot` key file) do not exist inside. Symlinked files are bound by target, so no `/run` directory becomes visible. |
| tmpfs `/home`, `/tmp`, `/var`; no `/run` except DNS | The owner's home, the API data directory (owner token, master key, other bots), the Docker socket, and the session bus do not exist inside. |
| Read-write `bots/<id>/work` and `bots/<id>/codex-home` only | Each bot has its own working directory and `CODEX_HOME` (sessions, caches, settings). |
| Host `auth.json` bind-mounted into the bot's `CODEX_HOME` | The supported ChatGPT login is shared without copying; refreshes write through. The bot's home holds only an empty mount point. |
| `env_clear` and an allowlist (`HOME`, `PATH`, `LANG`, `TMPDIR`, `CODEX_HOME`, marker) | `SECRET_KEY`, `DATABASE_URL`, the owner token, and the API environment never reach the provider. |

Network access remains, because the CLI talks to its service. No PostgreSQL or owner credential is present; the only API credential is the turn's task grant (`ELSE_TASK_GRANT`), which reads that bot's permitted memory and dies with the turn. Bubblewrap's options, including that value, are passed through an inherited memfd (`--args 3`), so no host command line, which any local user can read, contains it. The per-bot `config.toml` and the empty login mount point are replaced atomically before each launch, never written through a symlink the provider may have planted. Before each turn the adapter asks Codex for its effective MCP servers and refuses to run unless they are exactly else.bot's `elsebot` server offering exactly its memory tools (POC-013). The JSON-RPC client bounds message size and backlog, applies one deadline to writing and waiting, and refuses every server request (approvals, tool calls) itself. The configuration sets approval `never`, sandbox `read-only`, web search disabled, `[agents] enabled = false`, one MCP server (`elsebot`, with `enabled_tools` limited to the three memory tools and the bearer token read from the environment), and features off for multi-agent (v1 and v2), shell, apps, plugins, hooks, browser, computer use, image generation, and related tools. `--strict-config` validates it against the installed version. For `gpt-6.1-sol`, Codex 0.166 calls MCP tools only from JavaScript run by `codex-code-mode-host`, a JIT-less V8 runtime exposing timers and tool callbacks, so that host is enabled and mounted read-only inside the same sandbox. Asked to list its tools there, the model reported `exec`/`wait`, `request_user_input`, and inside `exec` `apply_patch`, MCP resource readers (else.bot offers no resources), a clock, and the deferred memory tools. Codex 0.166 keeps `unified_exec` on regardless of configuration, but with the shell tool disabled none is exposed. Every tool call surfaces as an item the adapter checks: an MCP call outside the allowlist, a command, or a file change ends the turn as `policy_violation`, and Codex's own read-only sandbox refuses file edits. Live checks with the tools configured (`with_memory_tools_commands_edits_and_agents_still_cannot_run`, `code_mode_reaches_only_elsebot_tools`) asked for a command, a file write with `apply_patch`, a user-input request, a network fetch to a local canary, a host file read, and two sub-agents: no command, file, or agent item appeared, the work directory stayed empty, the canary received no connection, and the model reported each capability unavailable. The model declined some direct probes, so these checks show the configuration and observed behavior, not an exhaustive audit of the code-mode runtime. A bounded live check (`ELSE_LIVE_CODEX=1 cargo test -p else-api --test codex_live`) asks the model to run a shell command and spawn two sub-agents: no command, tool, file, or agent item appeared, no approval or tool request reached the API, no extra thread started, and the model reported both capabilities unavailable.

Task grants are opaque `tg_` tokens held as digests in API memory, bound to one bot, conversation, and turn with a tool allowlist. They are revoked when the turn ends, on Stop work and deletion, and by any restart. The task-scoped tool API lives at `/tools/v1` and accepts only grants; owner routes answer a grant with `403 task_grant_forbidden`, and tool routes answer the owner token with `403`. Every tool call rechecks that the turn is still sending or running and its bot active. Identity comes only from the grant: unknown query fields are rejected and another bot's objects return `404`. Each turn's grant allows reading its conversation history and memory (POC-013). Codex reaches those tools only through else.bot's MCP endpoint; a cross-bot read additionally needs the owner's memory-access grant, and tool calls can neither write memory nor create owner corrections.

## Master key and encrypted secrets

The initial deployment should read an installation master key from the Rust API's environment:

```dotenv
SECRET_KEY=<base64-encoded 32 cryptographically random bytes>
```

Provision this key once and preserve it across restarts and migrations. Keep it outside PostgreSQL, source control, and ordinary application configuration exports. Supply it only to the trusted API and its backup worker. Provider CLIs and bot VMs must not inherit it.

Derive separate keys for recoverable database secrets and backup encryption using HKDF-SHA-256 with distinct purpose labels, such as `else.bot/secrets/v1` and `else.bot/backups/v1`. Use Rust `age` streaming encryption for backup artifacts, initially pinned to the tested `0.12.1`. Derive its per-set identity with a fresh public salt and the backup purpose label. Preserve salts, format versions, and key identifiers with encrypted data. The [backup research](research/backup-and-recovery.md#encryption-and-key-separation) specifies that format. Selection of the database-secret encryption library remains open. [HKDF purpose separation](https://www.rfc-editor.org/rfc/rfc5869.html#section-3.2), [authenticated encryption and key separation](https://cheatsheetseries.owasp.org/cheatsheets/Cryptographic_Storage_Cheat_Sheet.html).

The backup encryption covers PostgreSQL exports, VM archives, and recovery manifests before upload. Include the encryption format and key identifier in the outer backup metadata so recovery can select the key before decrypting the manifest.

Startup must reject a missing or invalid key. A key that cannot decrypt existing state must produce an error without replacing the key or resetting that state.

The owner's recovery mechanism will be designed later. Restoring encrypted data requires the corresponding master key, recovered separately from the encrypted backup. Losing both the server's key and its separate recovery copy makes that data unrecoverable. Key rotation must retain access to every key version still needed by stored secrets or retained backups.

## Open design work

- How should the owner recover and rotate `SECRET_KEY` while keeping retained backups restorable?

The [backup design](backups.md) and [planned restore procedure](restore.md) depend on access to the corresponding key versions.
