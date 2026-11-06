# Shared MCP packages

MCP packages belong to the installation, so every bot can discover the capabilities another bot installed. The default library is `~/.else/mcp`. `ELSE_DATA_DIR` changes its parent directory.

Each package has an immutable id and four private files.

| File | Content |
| --- | --- |
| `manifest.json` | Name, description, installer, installation time, and transport |
| `SKILL.md` | Instructions for using the MCP |
| `README.md` | Setup documentation and API reference |
| `config.json` | HTTP endpoint and headers, or a stdio command, arguments, and environment |

`mcp_install` validates and publishes the complete package by directory rename. An identical installation returns the existing manifest. Different files under an existing id produce a conflict. The owner can remove that package in Settings before installing its replacement.

The library owns one actor per package. Its bounded mailbox serializes installation, removal, and requests for that package. Other packages run independently. An accepted mutation or request holds a maintenance admission until the actor finishes, even if its HTTP caller disconnects.

## Bot tools

The provider adapters continue to connect only to else.bot's MCP endpoint. That endpoint provides the shared library tools to both providers.

| Tool | Behavior |
| --- | --- |
| `mcp_list` | List package metadata, or read a package's skill and documentation by id |
| `mcp_install` | Save the package for all bots during an owner request |
| `mcp_request` | Discover or call tools, read resources, or retrieve prompts from an installed MCP |

`mcp_request` accepts `tools/list`, `tools/call`, `resources/list`, `resources/templates/list`, `resources/read`, `prompts/list`, and `prompts/get`. The `params` object follows the corresponding MCP schema. Remote responses are nested under `result`, separate from else.bot's own control fields. A `tools/call` requires an `effect` of `local`, `destructive`, or `external`. Destructive and external calls use the existing owner approval flow for the exact arguments.

Shared packages do not expand a task grant. Incoming email and social events cannot install or use shared MCPs. Routine runs can use existing packages but cannot install new ones. Tasks delegated from an owner request can install a package for that request. Saved documents and remote results are reference data, not permission.

HTTP uses Streamable HTTP. Stdio servers run on the API host, with an isolated environment containing a small set of host defaults and the package's explicit variables. Installation saves configuration without running the server or installing software dependencies. New backup sets include the MCP library even when empty, so CLI restore requires the destination data directory. Connection failures and timeouts never automatically repeat a mutation.

Credentials in `config.json` are absent from discovery results and the owner list. MCP config and call arguments are excluded from activity logs. Approval cards show the proposed arguments and remain in conversation history. Credentials belong in connection config, not call arguments or approval text. Package files have mode `0600` and their directories have mode `0700`.

## Owner API and backups

`GET /v1/settings/mcp` returns `{ "mcps": [...] }`. `DELETE /v1/settings/mcp/{id}` removes the shared package and closes its connection. Both routes require owner authentication. The desktop lists installed packages without claiming current connection health.

Coordinated backups include the shared library in one encrypted `mcp.tar.age` artifact. Restore recreates its package files under the destination data directory. An empty library is captured too, so restoring a later backup preserves removals. Older backups without a library artifact remain restorable.

## Verification

The focused test targets are `cargo test -p else-api --lib mcp:: --locked` and `cargo test -p else-api --test shared_mcp --test assistant_backup --test backup --locked`.
