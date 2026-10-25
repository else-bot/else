# Security and secrets

These are security requirements and proposed controls. They have not been implemented or validated in else.bot. [System prompt drafts](prompts.md) describe model behavior, and [webhook authorization](routines.md#webhook-authorization) defines routine access.

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

The proposed deployment should give each bot its own writable filesystem, browser profile, and provider session files. Cross-bot memory access should continue through else.bot's skills and API.

For the prototype, the runtime configuration should enforce these boundaries:

- Keep the host home directory, host desktop sockets, and Docker socket out of bot environments. A trusted service outside the sandbox manages its lifecycle. Docker documents the privileges involved in controlling its daemon. [Docker Engine security](https://docs.docker.com/engine/security/).
- Keep host runtime management separate from privileges inside the guest. For container alternatives, use a non-root user, avoid privileged containers, and retain syscall restrictions. [Container seccomp profiles](https://docs.docker.com/engine/security/seccomp/).
- Separate bot networks. Allow required public web traffic and authenticated access to else.bot's API. Block direct access to other bot environments, host administration services, private networks, and cloud metadata endpoints unless explicitly configured.
- Set CPU, memory, process, and disk limits per bot so one task cannot consume the whole server.
- Keep VNC, browser debugging, shell, and file APIs private. Route desktop access through an authenticated TLS proxy that checks which bot the owner is opening. Use separate credentials for each bot and replace example passwords.

Create bot environments with their own workspace. Any host mount must be explicit and limited to what the bot needs. Provide each bot's skills from else.bot's configuration and register only intended MCP integrations.

For VNC access, the proposed route is the client, else.bot's authenticated TLS proxy, a private connection to the guest, and noVNC inside the bot's VM. Validate port forwarding, WebSocket streaming, and reconnects with the chosen runtime. Desktop access still requires else.bot authentication.

These controls limit access to the host and other bot environments. They do not prevent a bot from misusing data or connectors it can already access. Those permissions still belong in else.bot's API.

## Master key and encrypted secrets

The initial deployment should read an installation master key from the Rust API's environment:

```dotenv
SECRET_KEY=<base64-encoded 32 cryptographically random bytes>
```

Provision this key once and preserve it across restarts and migrations. Keep it outside PostgreSQL, source control, and ordinary application configuration exports. Supply it only to the trusted API and its backup worker. Provider CLIs and bot VMs must not inherit it.

Derive separate keys for recoverable database secrets and backup encryption using HKDF-SHA-256 with distinct purpose labels, such as `else.bot/secrets/v1` and `else.bot/backups/v1`. Use an established authenticated encryption library and a supported streaming format for large VM archives. Preserve the required salts, nonces, format versions, and key identifiers with encrypted data. The exact library and archive format remain implementation decisions. [HKDF purpose separation](https://www.rfc-editor.org/rfc/rfc5869.html#section-3.2), [authenticated encryption and key separation](https://cheatsheetseries.owasp.org/cheatsheets/Cryptographic_Storage_Cheat_Sheet.html).

The backup encryption covers PostgreSQL exports, VM archives, and recovery manifests before upload. Include the encryption format and key identifier in the outer backup metadata so recovery can select the key before decrypting the manifest.

Startup must reject a missing or invalid key. A key that cannot decrypt existing state must produce an error without replacing the key or resetting that state.

The owner's recovery mechanism will be designed later. Restoring encrypted data requires the corresponding master key, recovered separately from the encrypted backup. Losing both the server's key and its separate recovery copy makes that data unrecoverable. Key rotation must retain access to every key version still needed by stored secrets or retained backups.

## Open design work

- How should the owner recover and rotate `SECRET_KEY` while keeping retained backups restorable?

The [backup design](backups.md) and [planned restore procedure](restore.md) depend on access to the corresponding key versions.
