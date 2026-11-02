# Authentication and permissions for the first desktop release

Research date: November 2, 2026. This report proposes the smallest useful single-owner design. It does not change application configuration or grant permissions on this host.

DESIGNED means a recommendation for implementation. VERIFIED means inspected primary documentation or a named local experiment. UNVERIFIED means an integration still needs evidence. The local fixture verifies its own policy model, not an implemented else.bot service.

## Keep one owner and two kinds of runtime access

DESIGNED: bind the Rust API to `127.0.0.1` by default and require an owner bearer token, including during development. The GPUI desktop stores that token and sends it with each request. Runtime provider sessions receive separate, short-lived task grants. Neither account registration nor an OAuth server is needed for this release.

The UI covers messaging with multiple bots and creating or deleting a bot. Initial deletion is a soft delete with immediate revocation and cancellation. Chats, memory, and files remain until a later explicit purge. Email, notes, kanban, routines, mobile, and granular approval screens remain deferred. PostgreSQL, Rust GPUI, host provider CLIs, native sessions, and per-bot Microsandbox computers remain accepted constraints. This preserves the [architecture](../architecture.md), [host execution requirements](../security.md#host-provider-processes), and [tool contract](../providers.md#host-to-vm-tools-and-skills).

There are three distinct credentials in this design:

| Credential | Who receives it | Authority |
| --- | --- | --- |
| Owner token | Desktop and trusted local owner tooling | All first-release owner operations |
| Task grant | One restricted host provider session or its trusted MCP bridge | Its recorded bot, conversation, task, and fixed tool allowlist |
| Provider subscription credentials | Supported host provider authentication machinery | Provider login and renewal, independent of else.bot authentication |

`SECRET_KEY` remains the API's encryption master key. It authenticates no client and grants no tool access. Generate the owner token independently. Provider CLIs and VMs receive neither credential. This preserves [the accepted master-key design](../security.md#master-key-and-encrypted-secrets).

## Owner authentication and storage

DESIGNED: provision one random owner token, using 32 random bytes encoded as unpadded base64url. `OWNER_TOKEN_FILE` is a proposed configuration name for the trusted API's private token file. The server hashes the provisioned value with SHA-256 in memory and compares fixed-size digests in constant time. No owner account table, password hashing flow, JWT signing key, or token service is needed.

VERIFIED: Rust's `getrandom::fill` obtains bytes from the system random source and returns an error on failure. `subtle::ConstantTimeEq` provides constant-time equality, with a length caveat for slices. Hashing first gives two equal-length digests. [getrandom documentation](https://docs.rs/getrandom/latest/getrandom/fn.fill.html), [subtle documentation](https://docs.rs/subtle/latest/subtle/trait.ConstantTimeEq.html).

DESIGNED: explicit installation setup creates the file once in a private directory. Ordinary startup rejects a missing or malformed token. It never regenerates credentials or modifies saved data. On Unix, create the file with `create_new(true)` and mode `0600` in a directory restricted to the owner or API service. Reject symlinks, unexpected ownership, and group or world access when loading existing files. A separate API service user needs its own provisioned copy rather than broader permissions.

VERIFIED: Rust documents that `create_new` atomically refuses an existing path, including a symlink. `OpenOptionsExt::mode` applies only when creating a new file, subject to the process umask. It does not repair existing permissions. The local Rust probe confirms creation at `0600` and refusal to overwrite. [OpenOptions reference](https://doc.rust-lang.org/std/fs/struct.OpenOptions.html#method.create_new), [Unix permissions reference](https://doc.rust-lang.org/std/os/unix/fs/trait.OpenOptionsExt.html#tymethod.mode).

DESIGNED: the desktop normally uses an OS credential store, keyed by an explicit server profile. `keyring` with its `v1` feature is a small candidate. Keep the configured origin separate from the token. Never send that token to a changed origin automatically, follow redirects with it, or disable certificate validation.

VERIFIED: the current `keyring` documentation describes Keychain on macOS, Windows Credential Manager, and Secret Service on Unix. It exposes set, get, and delete operations. `Entry::new` can fail when no default store is available. These are documented capabilities, not a test of this desktop. [Keyring platform choices](https://docs.rs/keyring/latest/keyring/v1/index.html), [credential-store errors](https://docs.rs/keyring/latest/keyring/v1/struct.Entry.html#method.new).

DESIGNED: development must also work without a desktop keyring daemon. A proposed `ELSE_OWNER_TOKEN_FILE` desktop override explicitly selects a private file. A developer launcher can supply an `ELSE_OWNER_TOKEN` process environment value when convenient, but the file is preferable. Keep real values out of versioned `.env` files, shell arguments, terminal output, fixtures, and logs. A tracked sample contains variable names and placeholders only. An unavailable keyring produces a clear connection error with the explicit file option. It never enables anonymous requests or silently writes plaintext credentials elsewhere.

The owner token stays valid until replaced. Rotation stops the API, replaces the private file, updates the desktop entry, and restarts the API. Existing streams close with the process, and transient task grants disappear. This is sufficient for development and a private single-owner installation. Rotation does not replace `SECRET_KEY`, provider credentials, or webhook tokens.

## Personal computer and VPS connections

DESIGNED: loopback HTTP is allowed only for the local desktop or the two local ends of an authenticated SSH tunnel. A VPS keeps the same loopback bind. The owner forwards a local port over SSH and uses the owner token through that tunnel. SSH credentials and the owner token stay separate. No public API port is necessary.

Direct LAN or Internet use requires an explicit deployment choice: authenticated HTTPS at a configured reverse proxy, with the Rust API still bound privately. Treat an encrypted private tunnel as the alternative to public exposure. The server must reject a non-loopback plaintext bind. A development flag, source IP, `localhost` name, or forwarded header never bypasses bearer authentication. Keep PostgreSQL, runtime administration, VNC, and browser-debugging ports private.

VERIFIED: OWASP recommends HTTPS for protected APIs, endpoint authorization, avoiding credentials in URLs, and disabling unnecessary CORS. The loopback HTTP allowance above is a narrow design exception for local transport. [OWASP REST security](https://cheatsheetseries.owasp.org/cheatsheets/REST_Security_Cheat_Sheet.html).

DESIGNED: accept exactly one `Authorization: Bearer <token>` credential. Reject missing, malformed, duplicated, unknown, expired, or revoked credentials with `401`. An authenticated caller lacking permission receives `403`. Use a generic `404` for inaccessible object IDs. Never accept tokens from query parameters. Redact authorization headers, credentials, and provider session contents from tracing. Bound request sizes and failed-auth rates without adding an external rate-limit service.

The native client needs no CORS permission or authentication cookies. Reject unexpected `Origin` values and validate `Host` against the configured listener or public origin. A trusted proxy must overwrite forwarding headers rather than accepting client assertions. These checks supplement bearer authentication. They do not identify the owner.

Protect message streams and future desktop proxies with the same authorization. The native client can attach a bearer header when opening a stream. A stream subscription remains tied to its bot and closes when that authorization becomes invalid. Future browser-based VNC needs an explicit authenticated proxy flow, not a token in its URL. OWASP documents WebSocket origin validation, authorization per message, and session termination. [OWASP WebSocket security](https://cheatsheetseries.owasp.org/cheatsheets/WebSocket_Security_Cheat_Sheet.html).

## Fixed owner operations and task-scoped tools

DESIGNED: keep the policy in ordinary Rust functions. An authenticated owner can list, create, and delete bots and read or send their messages. Runtime bots cannot call those management operations. There is no role editor, permission DSL, account hierarchy, or external policy engine.

The trusted adapter issues a random opaque task credential. The API stores only its digest in a process-local map with this server-owned record:

```text
bot_id, conversation_id, task_id, provider_session_generation,
allowed_tools, expires_at, revoked
```

Use an initial 15-minute lifetime. The trusted adapter renews it while the same authorized task remains active. Renewal checks current state and needs no owner popup. A model cannot create or widen a grant. Restart invalidates the map. The API reconciles saved tasks and guest job IDs before issuing replacement grants, without resubmitting uncertain work.

For each tool call, the API resolves the credential, checks current bot and task state, and derives identity from the grant. It checks the object's stored bot and task relationship. A payload's `bot_id`, a job ID, a native provider session ID, or a guest path never grants access. The default allowlist covers the bot's own conversation and assigned computer tools. Cross-bot memory, delegation, connectors, and webhook triggers receive no implicit permission from this grant.

The main assistant's prompt may choose delegation and memory lookup when configured permissions allow them. For these tools, the grant also records explicit permitted memory targets and delegation targets, copied from trusted configuration and limited by the originating task. The API checks the operation and target on every call. Delegation creates a task grant for the receiving bot within those restrictions. It does not give the caller direct access to the receiving bot's computer or unrelated data. Prompt text, summaries, and a receiving bot's wider standing permissions cannot expand the originating authority. This keeps prompt-driven delegation available without implicit cross-bot access or a general permission framework.

An internal stdio MCP bridge can bind identity to its trusted process context instead of exposing an HTTP bearer to the model. Either transport calls the same authorization function. Network MCP remains a private adapter interface for this release, with per-session task credentials. A claim of general remote MCP interoperability would require separate protocol research.

Guest job output, artifact downloads, file transfers, screenshots, and cancellation must check the same object relationships as execution. Resolve file paths inside the assigned guest, including symlink behavior. No request accepts arbitrary host paths, host shell commands, runtime sockets, or VM management credentials. VM lifecycle requests use the trusted manager's handle for that bot.

VERIFIED: OWASP recommends denying unspecified access, checking permissions on every request, and validating object relationships. Its agent guidance calls for restricted tools and enforcement outside model instructions. These support the proposed small policy; they do not require an enterprise RBAC system. [OWASP authorization](https://cheatsheetseries.owasp.org/cheatsheets/Authorization_Cheat_Sheet.html), [OWASP agent security](https://cheatsheetseries.owasp.org/cheatsheets/AI_Agent_Security_Cheat_Sheet.html).

If Axum is selected, middleware can authenticate and attach a principal for handlers. Resource authorization still belongs in the service function called by HTTP, MCP, and background dispatch. Verify all route groups are wrapped, including streams. Axum applies router layers to previously added routes. [Axum middleware documentation](https://docs.rs/axum/latest/axum/middleware/index.html).

## Stop and deletion must invalidate queued work

DESIGNED: use the existing bot and task records, plus a computer generation counter. Keep one per-bot mutex in the single API process to order dispatch, owner stop, and deletion. PostgreSQL stores the durable state. Do not add a distributed lock service.

Authentication at request admission is insufficient. Queued guest operations record the current computer generation. Immediately before dispatch, under the same mutex, recheck the credential, task, bot state, desired computer state, and generation. Hand the accepted operation to the trusted manager before releasing that dispatch section. Do not hold the mutex while waiting for a long guest job to finish.

The operations have distinct effects:

| Operation | Immediate authorization effect | Completion condition |
| --- | --- | --- |
| Cancel task | Mark task cancelling and revoke its grants | Provider execution and guest jobs for that task have stopped |
| Stop computer | Persist owner-stopped desired state and increment computer generation | Manager confirms no guest work or desktop session remains active |
| Delete bot | Persist a deletion tombstone, revoke grants, and increment computer generation | Provider sessions, streams, jobs, and leases are closed, and the computer is stopped. Chats, memory, and files remain |

A stopped computer still permits conversation work. The tools cannot implicitly wake it. Explicit resume allows new guest requests, while the generation check rejects work queued before the stop. This preserves [the existing lifecycle requirement](../sandboxes.md#lifecycle-and-recovery).

Deletion first makes the bot unavailable to new work, then finishes cancellation and shutdown. Persist the tombstone and track pending shutdown until it succeeds. Do not reuse bot IDs. A retry finishes the same shutdown. Retain chats, memory, files, and the stopped VM's saved filesystem. Data removal belongs to a later explicit purge. A late provider result or callback checks current task and bot state and cannot append to retained conversations or recreate the VM. A provider handoff revokes the outgoing grant before issuing the incoming grant, while retaining guest job IDs.

Stop or delete can follow an operation already accepted by the VM manager. Authorization cannot undo a command that already ran. Return a pending result until the manager confirms cancellation or shutdown. Recheck desired state after an in-flight VM start and stop it if necessary. On API restart, reconcile deleting and owner-stopped records before dispatch or `connect_or_create`. This is a small serialization rule, not a claim of instantaneous distributed cancellation.

## Development agents have a separate workspace authorization

DESIGNED: a development agent improving else.bot works under the owner's repository authorization. It is not an else.bot runtime bot and does not inherit production access. The following proposed policy can be adopted in the repository instructions or development launcher later. This research worker does not install it.

> The owner authorizes development agents to inspect and edit the assigned else.bot checkout, run builds and tests, install project dependencies, and start disposable local development services. Write access covers the assigned workspace and task scratch directories. Use synthetic fixtures and a separate development database, token, and bot storage. Continue these authorized actions without requesting permission again. Never replace an existing checkout change. Current task-specific file restrictions take precedence. This authorization excludes production data, production credentials, unrelated home files, host configuration, privileged host administration, publishing, and messages or external writes unless the owner separately authorizes them.

Keep that authorization in the development launcher's supported workspace and command permissions. Use a reusable rule for already authorized development commands. Do not obtain convenience by disabling runtime bot isolation or granting unrestricted host access. Environment or sandbox approval still applies when a command exceeds the actual configured permissions. A prompt or repository instruction cannot override the operating system or the agent platform.

Runtime bots remain restricted even when the owner is also the developer. The API launches provider processes with `Command::env_clear`, an explicit executable, a bot session directory, and an environment allowlist. That allowlist excludes the owner token, `SECRET_KEY`, database credentials, and backup credentials. Supported provider authentication stays on the host, separately from bot session storage and all VMs.

VERIFIED: Rust documents environment inheritance by default and `env_clear` to remove it. The local probe confirms a harmless child sees only one synthetic allowlisted variable. This proves environment filtering, not file or process isolation. [Rust process environment control](https://doc.rust-lang.org/std/process/struct.Command.html#method.env_clear).

UNVERIFIED: a provider process under the same Unix user may still access readable files, process state, or desktop credentials. File mode `0600` and environment filtering alone do not isolate it from the API. The existing host-containment requirement remains an implementation gate. Validate supported provider controls plus OS restrictions against secret-file reads, sibling sessions, native shell execution, and network access. If subscription login needs broad host access, stop and resolve that conflict before claiming runtime isolation. This report does not run either CLI or alter host policy.

## Evidence and remaining checks

VERIFIED: [check.py](../experiments/auth/check.py) passed 39 deterministic policy checks. It covers owner management, missing and malformed credentials, duplicate headers, cross-bot and cross-task access, foreign jobs, expiry, revocation, provider handoff, rotation, restart invalidation, and stop or delete dispatch ordering. It also proves the model permits conversation reads while the computer is stopped. The [Rust probe](../experiments/auth/std_probe.rs) passed private-file creation and environment filtering on Rust `1.98.0`. [Captured results](../experiments/auth/results.txt).

The rerun command requires Python 3.10 or newer, Rust, and a Unix host with `/usr/bin/env`:

```sh
python3 -u docs/experiments/auth/check.py
```

The fixture uses fixed fake tokens, logical time, a sequential model of dispatch ordering, and temporary files. It launches no provider, server, VM, or network connection. Temporary binaries and the fake credential are removed afterward. It does not prove concurrent Rust locking, cryptographic timing, network binding, HTTP parsing, path confinement, TLS, keyring behavior, VM cancellation, retained-data deletion, or configured cross-bot grants.

UNVERIFIED implementation acceptance checks remain bounded:

- Exercise actual owner routes, MCP calls, message streams, and artifact routes without credentials, with a task credential, and with a foreign object ID.
- Race actual dispatch against task cancellation, computer stop, deletion, provider handoff, and API restart. Confirm stale operations cannot wake a stopped computer or publish late results.
- Test the GPUI keyring on the target Linux desktop and the explicit file override without Secret Service. Test token replacement and server-origin changes.
- Verify loopback binding and authenticated SSH-tunnel operation. If direct network deployment is added, verify HTTPS, proxy header handling, and rejection of plaintext remote binds.
- Prove host CLI containment while preserving supported subscription login and isolated native sessions. The synthetic environment probe is insufficient.

Routines remain later. Preserve their [accepted independent webhook-token design](../routines.md#webhook-authorization): a webhook token triggers only its saved routine and bot. Owner tokens and task grants must never authenticate webhook trigger requests, and webhook tokens must never authenticate owner or computer-tool routes. No webhook endpoint is needed in the first release.
