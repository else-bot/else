# Desktop server connections

The desktop saves named server connections and selects one for conversations and server settings. Servers and App updates belong to the desktop app. Email, Socials, MCP, and Backups belong to the selected server and show its name, address, and connection state.

## Ownership

`config::ServerRegistry` owns saved IDs, names, credentials, and the selected ID. `ProfileStore` validates and atomically persists the registry in a private `desktop.servers.json` beside the existing desktop configuration. Loading a legacy single profile writes its migrated registry before exposing the stable ID. The original file remains a selected-server projection readable by older desktop releases. Reconnecting with an older app cannot overwrite the authoritative server list. Changes made in an older app do not replace the newer app's saved connections. Only that migrated server inherits the previous layout file. Each server subsequently writes its own layout file. Panel widths follow the window across switches; category order and collapse state stay with their server.

`connection::Shell` owns the current messaging session and retains visited sessions by server ID. Each session keeps its immutable API client and complete `ElseApp`, including composer drafts and pending sends. Switching verifies the target and saves selection before changing the displayed session. Failed or obsolete attempts cannot replace the current session. Hidden sessions release computer views and recreate them when shown. Saved servers, monitors, and retained sessions are bounded by the 64-connection registry limit.

Server settings use entities and view IDs bound to their original client. Switching retires those settings views and loads the new server's views before showing their target card. Unfinished settings require a discard decision. Address or credential changes cannot retire a session that still has unsent drafts or pending messages. Renames preserve its ID and session.

`connection::servers::Connections` owns one monitor for each saved server. Each monitor accepts a bounded, coalesced refresh command and publishes its latest observation through a watch channel. Removing or replacing a connection drops its monitor subscription. A completion must still match its saved profile. Monitors authenticate capabilities and independently check readiness and version/work reports. They do not open chat streams for unvisited servers.

Health, version, release status, and selection are separate facts. An online server can have an update available. Offline versions show their last successful observation. Older servers without version reporting show Version unavailable. A missing release check never means Up to date. Unsupported API versions are refused before switching.

## Update targets

`updates::Component` selects App or Server. Each updater owns its snapshot and bounded commands. App updates skip server work checks and offer a desktop restart. Server updates use only the launcher's trusted local installation, recheck active work, and let the installer restart that server. An address alone never grants local installation authority. Unmanaged remote servers show a command to run on their own host.

The installer separates requested components from the installed inventory. Explicit `--app-only` and `--server-only` operations affect only that component. The default command updates the installed inventory.

Each installation has `app-current` and `server-current` release pointers and corresponding previous pointers. Migration initializes both installed components from the exact legacy release before either changes. Legacy `current` follows the server where present, so an existing service keeps its target during an app update. Pruning retains active and previous targets for both components. A server-only update preserves the published desktop app. An app-only update leaves server processes running and preserves server configuration values. Installation wrappers can migrate to the component paths without changing the server release.

Installation mutations serialize within that installation home. Separate installations have separate locks and logs. A killed installer can leave a lock directory; the refusal names it so an operator can inspect it before removal. Older installers that cannot honor component pointers are refused after migration.

## Verification

The registry tests cover legacy migration, stable IDs, private permissions, validation, invalid-input preservation, and layout isolation. Monitor tests exercise independent authenticated servers, authorization failures, readiness failures, unsupported APIs, and missing version reports. Installer fixtures exercise independent component updates, legacy migration, rollback, retention, and platform packaging. Native desktop checks exercise the switcher and settings against disposable servers.
