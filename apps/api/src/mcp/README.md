# Shared MCP packages

MCP packages belong to the installation, so every bot can discover the capabilities another bot installed. The default library is `~/.else/mcp`. `ELSE_DATA_DIR` changes its parent directory.

Each package has an immutable id and four private configuration files. OAuth connections add one encrypted `oauth-{bot_id}.json` file per authorized bot.

| File | Content |
| --- | --- |
| `manifest.json` | Name, description, installer, installation time, and transport |
| `SKILL.md` | Instructions for using the MCP |
| `README.md` | Setup documentation and API reference |
| `config.json` | HTTP endpoint and headers, or a stdio command, arguments, and environment |

`mcp_install` validates and publishes the complete package by directory rename. An identical installation returns the existing manifest. Different files under an existing id produce a conflict unless the call sets `replace`. A replacement shows the owner an external approval card for that exact call, with no standing grant, that compares the current and new package. Header and environment values, URL queries, and command arguments stay off the card. After approval, the new package swaps in atomically. An unchanged connection config keeps every bot's saved sign-in and MCP tool permissions. A changed one closes the package's sessions and removes both.

One package id holds one OAuth account per bot. Static credentials saved in headers or environment are shared by every bot. To use a second account of the same service, such as a work and a personal GitHub account, install the server again under another id, for example `github-work`. Each id signs in, keeps tool permissions, and appears in Settings separately.

The library owns one actor per package. It owns one shared stdio transport and separate HTTP OAuth sessions for each bot. Its bounded mailbox serializes installation, removal, and requests for that package. Other packages run independently. An accepted mutation or request holds a maintenance admission until the actor finishes, even if its HTTP caller disconnects.

## Bot tools

The provider adapters continue to connect only to else.bot's MCP endpoint. That endpoint provides the shared library tools to both providers.

| Tool | Behavior |
| --- | --- |
| `mcp_list` | List package metadata, or read a package's skill and documentation by id |
| `mcp_install` | Save the package for all bots during an owner request, or replace it with owner approval |
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

An HTTP 401 starts protected-resource and authorization-server discovery. The client supports authorization-server metadata, OpenID Connect discovery, and public dynamic client registration. Providers must support PKCE S256. GitHub uses a pre-registered app configured on the API server. Other providers without dynamic registration return a setup error.

`mcp_request` returns `auth_required` and `mcp_connect` returns `pending_owner_auth` when sign-in is required. Both include the MCP id, safe connection metadata, and `parked: true`. A durable native card shows the provider, requested scopes, purpose, and bot access. The desktop compares the grant metadata with the saved card before opening authorization. A changed grant requires a fresh card.

The desktop listens on a random loopback port and forwards the callback through the authenticated owner API. The API validates single-use state, expiry, issuer, and PKCE, exchanges the code, and encrypts tokens using an installation key derived for the package and bot. No authorization URL, code, verifier, or token reaches the agent. Discovery and token HTTP clients reject redirects and bound response sizes. Only literal loopback HTTP endpoints are allowed alongside HTTPS for OAuth.

After successful consent, the desktop settles the connection card and the waiting attempt continues. The agent retries the original read. Access tokens attach to that bot's HTTP transport and refresh automatically. Revoked consent returns `reauth_required`. Mutating `tools/call` operations never replay automatically.

Connection cards recover the server's account state before enabling controls. If saving credentials succeeds but settling the card fails, retry only confirms the card. Reopening the app recovers and confirms a matching connected account. Cancelling a card clears pending authorization before recording denial and never denies a connection that already completed.

Owner-only routes are `GET` and `DELETE /v1/bots/{bot_id}/mcp/{id}/connection`, `POST /v1/bots/{bot_id}/mcp/{id}/oauth/start`, `POST /v1/bots/{bot_id}/mcp/{id}/oauth/complete`, `DELETE /v1/bots/{bot_id}/mcp/{id}/oauth` to cancel the pending flow and read the resulting connection status, and `DELETE /v1/bots/{bot_id}/mcp/{id}/oauth/{request_id}` to cancel one known flow. Start takes a loopback `redirect_uri`. Complete takes the request id and callback fields. Connection status is `disconnected`, `authorizing`, `connected`, or `reauth_required`. Disconnect removes local tokens and closes that bot's transport. It does not revoke the grant at the provider.

Coordinated backups include the shared library in one encrypted `mcp.tar.age` artifact. Restore recreates its package files under the destination data directory. An empty library is captured too, so restoring a later backup preserves removals. Older backups without a library artifact remain restorable.

## Set up GitHub sign-in

Official API releases embed a dedicated GitHub OAuth App registration. The owner clicks **Connect in browser** without configuring credentials. GitHub's remote MCP server does not support dynamic client registration and requires a client secret for token exchange. See [GitHub's host integration guide](https://github.com/github/github-mcp-server/blob/main/docs/host-integration.md).

For source builds or a custom registration:

1. [Register a dedicated OAuth App](https://github.com/settings/applications/new) for else.bot. Set its callback URL to `http://127.0.0.1/mcp/callback`. GitHub allows the desktop to select a different loopback port for each sign-in.
2. Generate a client secret in the app's settings.
3. Set `ELSE_GITHUB_CLIENT_ID` and `ELSE_GITHUB_CLIENT_SECRET` in the API server's environment. Both values replace the embedded registration together. For direct API runs, you can use a private `.env` file. For an installed service, configure its service environment. Keep credentials outside shared MCP package files and chat. To embed your own registration, follow the [build instructions](../../../../CONTRIBUTING.md#run-a-development-session).
4. Restart the API server. Retry **Connect in browser** in the desktop connection card.

The API accepts these credentials only for `https://api.githubcopilot.com` resources with GitHub's official issuer, authorization endpoint, and token endpoint. The secret accompanies code exchange and refresh requests only. It does not appear in the browser URL, bot metadata, or saved tokens. GitHub Enterprise endpoints need a separate registration integration.

The embedded app credentials are extractable from the API binary. PKCE binds each authorization code to its sign-in attempt. Each bot still needs separate owner consent and has its own encrypted tokens. Restoring tokens requires the same OAuth App registration, either embedded or configured on the new host. Changing the client ID requires signing in again. Organization policy may require approval of the OAuth App before repository access succeeds.

## Verification

The focused test targets are `cargo test -p else-api --lib mcp:: --locked` and `cargo test -p else-api --test mcp_oauth --test shared_mcp --test assistant_backup --test backup --locked`.
