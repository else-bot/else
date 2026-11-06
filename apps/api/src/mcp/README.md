# Shared MCP packages

MCP packages belong to the installation, so every bot can discover the capabilities another bot installed. The default library is `~/.else/mcp`. `ELSE_DATA_DIR` changes its parent directory.

Each package has an immutable id and four private configuration files. OAuth connections add one encrypted `oauth-{bot_id}.json` file per authorized bot.

| File | Content |
| --- | --- |
| `manifest.json` | Name, description, installer, installation time, and transport |
| `SKILL.md` | Instructions for using the MCP |
| `README.md` | Setup documentation and API reference |
| `config.json` | HTTP endpoint and headers, or a stdio command, arguments, and environment |

`mcp_install` validates and publishes the complete package by directory rename. An identical installation returns the existing manifest. Different files under an existing id produce a conflict. The owner can remove that package in Settings before installing its replacement.

The library owns one actor per package. It owns one shared stdio transport and separate HTTP OAuth sessions for each bot. Its bounded mailbox serializes installation, removal, and requests for that package. Other packages run independently. An accepted mutation or request holds a maintenance admission until the actor finishes, even if its HTTP caller disconnects.

## Bot tools

The provider adapters continue to connect only to else.bot's MCP endpoint. That endpoint provides the shared library tools to both providers.

| Tool | Behavior |
| --- | --- |
| `mcp_list` | List package metadata, or read a package's skill and documentation by id |
| `mcp_install` | Save the package for all bots during an owner request |
| `mcp_connect` | Request owner sign-in for this bot through a native connection card |
| `mcp_connection_status` | Read this bot's connection state without credentials |
| `mcp_request` | Discover or call tools, read resources, or retrieve prompts from an installed MCP |

`mcp_request` accepts `tools/list`, `tools/call`, `resources/list`, `resources/templates/list`, `resources/read`, `prompts/list`, and `prompts/get`. The `params` object follows the corresponding MCP schema. Remote responses are nested under `result`, separate from else.bot's own control fields. A `tools/call` requires an `effect` of `local`, `destructive`, or `external`. Destructive and external calls use the existing owner approval flow for the exact arguments.

Shared packages do not expand a task grant. Incoming email and social events cannot install or use shared MCPs. Routine runs can use existing packages but cannot install new ones. Tasks delegated from an owner request can install a package for that request. Saved documents and remote results are reference data, not permission.

HTTP uses Streamable HTTP. Stdio servers run on the API host, with an isolated environment containing a small set of host defaults and the package's explicit variables. Installation saves configuration without running the server or installing software dependencies. New backup sets include the MCP library even when empty, so CLI restore requires the destination data directory. Connection failures and timeouts never automatically repeat a mutation.

Credentials in `config.json` are absent from discovery results and the owner list. MCP config and call arguments are excluded from activity logs. Approval cards show the proposed arguments and remain in conversation history. Credentials belong in connection config, not call arguments or approval text. Package files have mode `0600` and their directories have mode `0700`.

## Owner API and backups

`GET /v1/settings/mcp` returns `{ "mcps": [...] }`. `DELETE /v1/settings/mcp/{id}` removes the shared package and closes its connection. Both routes require owner authentication. The desktop lists installed packages without claiming current connection health.

## OAuth connections

Installation and account access are separate. An OAuth connection belongs to the installation owner and authorizes only the bot named on its connection card. Other bots need their own consent. Static credentials explicitly saved in shared configuration keep their existing shared behavior.

An HTTP 401 starts protected-resource and authorization-server discovery. The client supports authorization-server metadata, OpenID Connect discovery, and public dynamic client registration. Providers must support PKCE S256 and dynamic registration. Missing registration support returns a setup error rather than asking for tokens in chat.

`mcp_request` returns `auth_required` and `mcp_connect` returns `pending_owner_auth` when sign-in is required. Both include the MCP id, safe connection metadata, and `parked: true`. A durable native card shows the provider, requested scopes, purpose, and bot access. The desktop compares the grant metadata with the saved card before opening authorization. A changed grant requires a fresh card.

The desktop listens on a random loopback port and forwards the callback through the authenticated owner API. The API validates single-use state, expiry, issuer, and PKCE, exchanges the code, and encrypts tokens using an installation key derived for the package and bot. No authorization URL, code, verifier, or token reaches the agent. Discovery and token HTTP clients reject redirects and bound response sizes. Only literal loopback HTTP endpoints are allowed alongside HTTPS for OAuth.

After successful consent, the desktop settles the connection card and the waiting attempt continues. The agent retries the original read. Access tokens attach to that bot's HTTP transport and refresh automatically. Revoked consent returns `reauth_required`. Mutating `tools/call` operations never replay automatically.

Owner-only routes are `GET` and `DELETE /v1/bots/{bot_id}/mcp/{id}/connection`, `POST /v1/bots/{bot_id}/mcp/{id}/oauth/start`, `POST /v1/bots/{bot_id}/mcp/{id}/oauth/complete`, and `DELETE /v1/bots/{bot_id}/mcp/{id}/oauth/{request_id}`. Start takes a loopback `redirect_uri`. Complete takes the request id and callback fields. Connection status is `disconnected`, `authorizing`, `connected`, or `reauth_required`. Disconnect removes local tokens and closes that bot's transport. It does not revoke the grant at the provider.

Coordinated backups include the shared library in one encrypted `mcp.tar.age` artifact. Restore recreates its package files under the destination data directory. An empty library is captured too, so restoring a later backup preserves removals. Older backups without a library artifact remain restorable.

## Verification

The focused test targets are `cargo test -p else-api --lib mcp:: --locked` and `cargo test -p else-api --test mcp_oauth --test shared_mcp --test assistant_backup --test backup --locked`.
