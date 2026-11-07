# Shared MCP packages

MCP packages belong to the installation, so every bot can discover the capabilities another bot installed. The default library is `~/.else/mcp`. `ELSE_DATA_DIR` changes its parent directory.

Each package has an immutable id and four private configuration files. An OAuth connection adds one encrypted `oauth-00000000-0000-0000-0000-000000000000.json` file shared by every bot. Older per-bot `oauth-{bot_id}.json` files are adopted on first load: the newest becomes the shared sign-in and the rest are deleted.

| File | Content |
| --- | --- |
| `manifest.json` | Name, description, installer, installation time, and transport |
| `SKILL.md` | Instructions for using the MCP |
| `README.md` | Setup documentation and API reference |
| `config.json` | HTTP endpoint and headers, or a stdio command, arguments, and environment |

`mcp_install` validates and publishes the complete package by directory rename. An identical installation returns the existing manifest. Different files under an existing id produce a conflict unless the call sets `replace`. A replacement shows the owner an external approval card for that exact call, with no standing grant, that compares the current and new package. Header and environment values, URL queries, and command arguments stay off the card. After approval, the new package swaps in atomically. An unchanged connection config keeps every bot's saved sign-in and MCP tool permissions. A changed one closes the package's sessions and removes both.

One package id holds one account, shared by every bot. To use a second account of the same service, such as a work and a personal GitHub account, install the server again under another id, for example `github-work`. Each id signs in, keeps tool permissions, and appears in Settings separately.

The library owns one actor per package. It owns one connection and one OAuth session, shared by every bot. Its bounded mailbox serializes installation, removal, and requests for that package. Other packages run independently. An accepted mutation or request holds a maintenance admission until the actor finishes, even if its HTTP caller disconnects.

## Bot tools

The provider adapters continue to connect only to else.bot's MCP endpoint. That endpoint provides the shared library tools to both providers.

| Tool | Behavior |
| --- | --- |
| `mcp_list` | List package metadata, or read a package's skill and documentation by id |
| `mcp_install` | Save the package for all bots during an owner request, or replace it with owner approval |
| `mcp_connect` | Request owner sign-in for every bot through a native connection card |
| `mcp_connection_status` | Read the shared connection state without credentials |
| `mcp_request` | Discover or call tools, read resources, or retrieve prompts from an installed MCP |
| `mcp_wait` | Park the request on a long-running job in an installed MCP and continue it when the job stops |

`mcp_request` accepts `tools/list`, `tools/call`, `resources/list`, `resources/templates/list`, `resources/read`, `prompts/list`, and `prompts/get`. The `params` object follows the corresponding MCP schema. Remote responses are nested under `result`, separate from else.bot's own control fields. A `tools/call` requires an `effect` of `local`, `destructive`, or `external`. Destructive and external calls use the existing owner approval flow for the exact arguments.

## Waiting on long-running jobs

Some MCP tools start work that keeps running after the call returns, such as a T3 Code `delegate_task`. Their completion notices go to the server's own clients, not to else.bot, so a bot that ends its attempt would never hear back. `mcp_wait` closes that gap. A bot must not wait inside its attempt instead, by polling a status tool or calling a blocking wait tool such as T3 Code's `t3_thread_wait` through `mcp_request`. Each provider attempt ends after 30 minutes, delegated tasks included, so such a wait fails the request. The system prompt and the `mcp_request` and `mcp_wait` descriptions say so.

The bot names the MCP's status tool and arguments, a JSON pointer to the status in its result, and the values that mean the job still runs. The status is read from `structuredContent`, or else from the JSON in the first text block that parses. else.bot repeats the status call without the bot, so the server must declare repeating it safe. Its `tools/list` entry, found across pages, needs `readOnlyHint: true`, or both `destructiveHint: false` and `idempotentHint: true`. T3 Code's `task_status` is the second kind, because reading a finished task marks it delivered. Missing annotations count as unsafe, as MCP defines them. Any other tool is refused, and no approval or standing permission widens this. The declaration is checked again before every repeat, so a replaced package or upgraded server cannot turn a pending wait into calls to a tool that no longer qualifies. Such a wait fails without calling it.

Being safe to repeat does not make the first call free of effect. Like `mcp_request`, `mcp_wait` takes an `effect`. A `local` status call runs at once. A `destructive` or `external` one shows the owner an approval card for that exact call before anything reaches the MCP, and the bot retries after approval. The first status call runs inside `mcp_wait`, against the attempt that was running before any remote call. A job that already stopped returns its result at once. A running job stores one `mcp_waits` row, parks the attempt, and leaves the turn `waiting`.

The API watcher repeats due status calls on the API host every `interval_seconds` (15 to 3600, default 60) without a provider process. Each scan takes the oldest due wait of each package without a check in flight, at most four in flight overall, so one slow server holds one slot and many waits on one package cannot hide another. A sweep expires overdue waits without another status call. Stopping the API abandons checks in flight, and the next start repeats them. The wait settles as `finished` when the status leaves the running values or is missing, as `failed` after three consecutive failed calls or one that cannot succeed later, such as a removed MCP or revoked sign-in, and as `expired` after `timeout_minutes` (1 to 1440, default 240). Settling requeues the turn. The continuation receives the last status result as remote data. A maintenance pause does not count as a failed check, but the deadline keeps running. Rows survive restarts, and anything that ends the turn, including a newer owner message, cancels a pending wait. A delegated task can wait too. Its delegation stays pending, so the delegating bot gets the task's reply only when the task ends. Each continuation is a new provider attempt with its own deadline, so time spent waiting does not count against it. A waiting task holds neither its bot's owner conversation nor its other tasks. Stopping the delegating request ends the task and cancels its wait.

Shared packages do not expand a task grant. Incoming email and social events cannot install or use shared MCPs. Routine runs can use existing packages but cannot install new ones. Tasks delegated from an owner request can install a package for that request. Saved documents and remote results are reference data, not permission.

HTTP uses Streamable HTTP. Stdio servers run on the API host, with an isolated environment containing a small set of host defaults and the package's explicit variables. Installation saves configuration without running the server or installing software dependencies. New backup sets include the MCP library even when empty, so CLI restore requires the destination data directory. Connection failures and timeouts never automatically repeat an `mcp_request` call. The only call else.bot repeats on its own, even after a timeout, is an `mcp_wait` status call whose server declares it safe to repeat, described below.

Credentials in `config.json` are absent from discovery results and the owner list. MCP config and call arguments are excluded from activity logs. Approval cards show the proposed arguments and remain in conversation history. Credentials belong in connection config, not call arguments or approval text. Package files have mode `0600` and their directories have mode `0700`.

## Owner API and backups

`GET /v1/settings/mcp` returns `{ "mcps": [...] }`. `DELETE /v1/settings/mcp/{id}` removes the shared package and closes its connection. Both routes require owner authentication. The desktop lists installed packages without claiming current connection health.

## OAuth connections

An OAuth connection belongs to the installation owner and serves every bot. Signing in from any bot's card connects all of them, and disconnecting from any bot's card disconnects all of them. Tool permissions stay per bot.

An HTTP 401 starts protected-resource and authorization-server discovery. The client supports authorization-server metadata, OpenID Connect discovery, and public dynamic client registration. Providers must support PKCE S256. GitHub uses a pre-registered app configured on the API server. Other providers without dynamic registration return a setup error.

`mcp_request` returns `auth_required` and `mcp_connect` returns `pending_owner_auth` when sign-in is required. Both include the MCP id, safe connection metadata, and `parked: true`. A durable native card shows the provider, requested scopes, and purpose. A task delegated from an owner request shows its card in the target bot's chat. The desktop compares the grant metadata with the saved card before opening authorization. A changed grant requires a fresh card.

The desktop listens on a random loopback port and forwards the callback through the authenticated owner API. The API validates single-use state, expiry, issuer, and PKCE, exchanges the code, and encrypts tokens using an installation key derived for the package. No authorization URL, code, verifier, or token reaches the agent. Discovery and token HTTP clients reject redirects and bound response sizes. Only literal loopback HTTP endpoints are allowed alongside HTTPS for OAuth.

After successful consent, the desktop settles the connection card and the waiting attempt continues. The agent retries the original read. Access tokens attach to the package's shared HTTP transport and refresh automatically. Revoked consent returns `reauth_required`. A `tools/call` never replays after sign-in. Only `mcp_wait` repeats a call, and only a status tool its server declares safe to repeat.

Connection cards recover the server's account state before enabling controls. If saving credentials succeeds but settling the card fails, retry only confirms the card. Reopening the app recovers and confirms a connected account from the same provider with at least the card's scopes. A different account blocks approval but still lets the owner decline the card. Cancelling a card clears pending authorization before recording denial and never denies a connection that already completed.

Owner-only routes are `GET` and `DELETE /v1/bots/{bot_id}/mcp/{id}/connection`, `POST /v1/bots/{bot_id}/mcp/{id}/oauth/start`, `POST /v1/bots/{bot_id}/mcp/{id}/oauth/complete`, `DELETE /v1/bots/{bot_id}/mcp/{id}/oauth` to cancel the pending flow and read the resulting connection status, and `DELETE /v1/bots/{bot_id}/mcp/{id}/oauth/{request_id}` to cancel one known flow. Start takes a loopback `redirect_uri`. Complete takes the request id and callback fields. Connection status is `disconnected`, `authorizing`, `connected`, or `reauth_required`. Disconnect from any bot's route removes the shared tokens and closes the shared transport. It does not revoke the grant at the provider.

Coordinated backups include the shared library in one encrypted `mcp.tar.age` artifact. Restore recreates its package files under the destination data directory. An empty library is captured too, so restoring a later backup preserves removals. Older backups without a library artifact remain restorable.

## Git in bot computers

The GitHub MCP's sign-in also authenticates git over HTTPS in a bot's computer. `computer_run` and `computer_job_start` take an optional `github` argument naming the installed GitHub MCP. That command then runs with ordinary `git clone`, `fetch`, `pull`, and `push` authenticated for `https://github.com`, including inside scripts such as a release script, which need no changes.

The token reaches the guest like a task secret. It is sent over the runtime's stdin as `ELSE_GITHUB_TOKEN`, an environment variable of that one command or job. `GIT_CONFIG_COUNT` variables clear any other github.com credential helper and add one that answers from that variable, with username `x-access-token`. The token is never in a command line, a remote URL, a file, or the repository's `.git` directory. `GIT_TERMINAL_PROMPT=0` turns a missing or rejected credential into an error instead of a hang.

Output is masked inside the computer before it leaves. An isolated Python filter (`-I -S`, so user site packages cannot run first) replaces the token with `[secret ELSE_GITHUB_TOKEN]` in a foreground command's stdout and stderr and in a job's combined output file, which is the only output a job stores. The filter holds back enough bytes to catch a token split across reads. A foreground command's output is masked again on the host. A job's stored tail is not, because the host re-masks jobs only by task secret name, and else.bot does not keep the token on the server for a second pass. A job whose launch script fails is masked on the host with the token it still holds in memory, so the stored error never shows it. Masking prevents accidental disclosure, not a hostile command. Any command given the token can read it.

Only a sign-in made through the server's registered GitHub app qualifies, so no other provider's token is ever sent to github.com. It needs the `repo` scope, which GitHub's MCP requests by default. Each command first makes a read-only `tools/list` call with the token, so a revoked or expired sign-in is caught before git runs. An expired token refreshes. A disconnected or revoked sign-in shows the owner the same connection card `mcp_request` does and runs nothing. Only work that may use shared MCPs may name it. Incoming email and social events cannot. An organization that restricts OAuth apps must approve else.bot's app before its private repositories clone. GitHub's `403` names that organization.

### Push approval is a convention, not a permission boundary

else.bot does not enforce what git may push. The credential helper sees the same request for a fetch and a push, so any command that names `github` can push wherever the signed-in account can. The approval card depends on the bot declaring the command's `effect` honestly, as it does for every other command. The `github` description tells the bot that pushing a release, a tag, or the default branch publishes it, so the call is `external`, and that deleting a remote branch or tag is `destructive`. A command mislabeled `local` runs without a card. Preventing an unapproved push technically needs an authenticating git proxy that inspects `git-receive-pack`. That is deferred.

A release script such as `release.sh` bundles branch pushes, tag pushes, and a rollback that can delete a remote tag. The bot must run it as one `external` call, after the owner approves the complete operation, with the approval context listing every branch and tag it may push or delete. It must not split the script into separately labeled steps. This also relies on the bot following instructions.

GitHub is the enforced boundary. The OAuth app's token reaches every repository the signed-in account can, with that account's permissions. Narrow it where practical:

- Sign the GitHub MCP in as a dedicated machine account with write access only to the repositories bots work on.
- Protect `main`, `dev`, and release tags (for example `v*`) with [repository rulesets](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/about-rulesets) that restrict updates, creation, and deletion to a bypass list.
- To let bots release without giving everyday work that access, install the GitHub MCP a second time under another id, such as `github-release`. Sign it in as an account on the ruleset bypass list, and name it only for approved release runs. The everyday id's account then cannot change protected branches or tags, whatever the bot declares.

## Set up GitHub sign-in

Official API releases embed a dedicated GitHub OAuth App registration. The owner clicks **Connect in browser** without configuring credentials. GitHub's remote MCP server does not support dynamic client registration and requires a client secret for token exchange. See [GitHub's host integration guide](https://github.com/github/github-mcp-server/blob/main/docs/host-integration.md).

For source builds or a custom registration:

1. [Register a dedicated OAuth App](https://github.com/settings/applications/new) for else.bot. Set its callback URL to `http://127.0.0.1/mcp/callback`. GitHub allows the desktop to select a different loopback port for each sign-in.
2. Generate a client secret in the app's settings.
3. Set `ELSE_GITHUB_CLIENT_ID` and `ELSE_GITHUB_CLIENT_SECRET` in the API server's environment. Both values replace the embedded registration together. For direct API runs, you can use a private `.env` file. For an installed service, configure its service environment. Keep credentials outside shared MCP package files and chat. To embed your own registration, follow the [build instructions](../../../../CONTRIBUTING.md#run-a-development-session).
4. Restart the API server. Retry **Connect in browser** in the desktop connection card.

The API accepts these credentials only for `https://api.githubcopilot.com` resources with GitHub's official issuer, authorization endpoint, and token endpoint. The secret accompanies code exchange and refresh requests only. It does not appear in the browser URL, bot metadata, or saved tokens. GitHub Enterprise endpoints need a separate registration integration.

The embedded app credentials are extractable from the API binary. PKCE binds each authorization code to its sign-in attempt. One sign-in serves every bot, with one encrypted token file. Restoring tokens requires the same OAuth App registration, either embedded or configured on the new host. Changing the client ID requires signing in again. Organization policy may require approval of the OAuth App before repository access succeeds.

## Verification

The focused test targets are `cargo test -p else-api --lib mcp:: --locked` and `cargo test -p else-api --test mcp_oauth --test shared_mcp --test assistant_backup --test backup --locked`.
