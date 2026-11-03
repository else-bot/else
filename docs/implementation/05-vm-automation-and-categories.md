# VM automation and sidebar categories

These tasks extend the completed assistant POC. They add programming tools, script workflows, visual control of the bot's desktop, guest-local CDP scripting, and persistent sidebar categories. Follow the [agent runbook](README.md) and use [TODO.md](../../TODO.md) for completion status.

This is a task plan. No feature in this file is complete merely because it is described here. The completed POC and its [known limitations](follow-up.md) remain separate evidence.

## Current implementation

The [guest image](../../infra/guest/Dockerfile) already includes Python 3, Chromium, a desktop, and VNC. The [guest tools](../../apps/api/src/http/guest_tools.rs) can write and read files, run commands, track background jobs, fetch artifacts, and perform basic browser operations. Node.js, npm, and Rust are not installed by that image.

Chromium already starts with CDP on guest loopback port 9222 in [supervisord.conf](../../infra/guest/supervisord.conf). [else-browser](../../infra/guest/else-browser) uses it for navigation, selector clicks, typing, and visible text. POC-027 extends this into a supported script interface. It does not publish the debugger outside the VM.

Current guest MCP results use text content. A screenshot path or base64 embedded in text does not establish that the model received an image. POC-028 adds and verifies that path. POC-029 then uses it to control the same desktop visible through VNC, including applications outside Chromium.

## POC-025: Guest programming toolchain

Depends on: POC-024.

Extend the custom image with Node.js and npm, Python 3 with pip and venv, and Rust with rustc and Cargo. Include common task utilities: Git, curl, jq, archive tools, make, a C/C++ compiler and linker, pkg-config, and certificates. Keep the list small and document installed versions and image provenance.

Install tools during the image build and expose them to the non-interactive `elsebot` user. Projects, package environments, and caches live in the guest. Host provider CLIs and credentials stay outside it. Add a lightweight capability/version report so the bot can discover available tools through its existing computer tools.

Acceptance:

- Build from pinned sources and reproduce the documented tool versions. `node`, `npm`, `python3`, `pip` in a venv, `rustc`, and `cargo` work as the unprivileged task user without login-shell setup.
- Through actual guest tools, run a Node script, create a Python venv and run a script, and compile/run a Rust program. Demonstrate a project dependency install for each ecosystem with bounded test packages.
- Stop/start preserves projects and environments. A snapshot restore preserves their files and usable toolchain configuration.
- New bots receive the new image. Provide an explicit, recoverable upgrade path for existing computers, preserving workspaces and browser state. Ordinary startup never replaces their disks or silently resets their profiles.
- Measure image/archive size and representative build CPU, RAM, and disk use. Document which workloads need higher limits than the existing desktop budget.
- Provider-native sub-agents remain disabled. No `codex`, `claude`, host runtime socket, or provider credential is installed in the VM.

Working areas: `infra/guest`, `scripts/build-guest-image.sh`, computer capability reporting, live guest tests, and development documentation.

References: [Guest lifecycle](../sandboxes.md), [tool boundary](../providers.md#host-to-vm-tools-and-skills), [saved provider policy](../providers.md#provider-native-sub-agents-are-deferred).

## POC-026: Bot-authored scripts and batch jobs

Depends on: POC-025.

Teach the bot to create project files in its workspace, choose an installed runtime, install project dependencies when needed, and execute its code through the existing job tools. Reuse command, file, artifact, lease, timeout, and cancellation machinery. Use background jobs for longer work rather than extending a foreground call indefinitely.

Include an example where the owner asks for a scripted email to ten recipients. The capability is writing and executing a script with authorized task credentials. It does not require a built-in email client or a permanent mail service. Verification uses ten synthetic recipients and a local capture-only SMTP fixture inside the guest; it sends nothing to real people.

Acceptance:

- An actual host Codex bot writes and executes a multi-file script in its own VM, reads the result, and returns a generated artifact. The owner can inspect the saved source.
- The script example delivers ten messages to the capture fixture, and the test checks recipient/content counts against actual captured messages. Exercise Node and Python examples and a compiled Rust task.
- A partial failure records completed and uncertain results. A repeated prompt or resumed job checks existing receipts rather than blindly resending. Stable message IDs alone must not be presented as an exactly-once guarantee.
- Invalid credentials, dependency errors, nonzero exits, and timeouts produce useful outcomes. Stop work terminates both foreground and background script work and their descendants within a documented bound.
- Long jobs retain leases across provider completion or disconnection. Files survive stop/start, and an API restart does not launch a duplicate job.
- Example credential injection is task-specific and avoids command arguments and saved source. The examples do not print secrets or copy host provider/API credentials into the guest. An explicit owner send request is not followed by redundant per-recipient prompts.

Working areas: computer skill/prompt, guest job tools, live provider/VM tests, and script examples.

References: [Execution lifecycle](../research/execution-lifecycle.md), [current job implementation](../../apps/api/src/computers/jobs.rs), [existing cancellation limitations](follow-up.md#behavior-to-revisit).

## POC-027: Guest-local Chromium CDP scripting

Depends on: POC-025.

Make the existing CDP endpoint discoverable and documented for bot-authored Node and Python scripts. Provide a small connection example/helper with readiness checks, bounded waits, target selection, and reconnection after browser restart. Attach to the persistent visible Chromium instance instead of opening an unrelated headless browser.

Scripts connect to guest `127.0.0.1:9222`, including its debugger WebSocket endpoint. Execute them through managed guest jobs so active browser work counts toward VM use and backup coordination. Coordinate mutations with existing browser/desktop operations; start with serialized control and document direct-client concurrency limits.

Acceptance:

- Node and Python scripts connect to the existing browser, select/create the intended tab, navigate a local fixture, query information, fill a form, and collect a result. Verify its tab/session is the one visible through VNC.
- Browser readiness, a closed target, navigation timeout, and browser restart return actionable errors or reconnect safely. A lost acknowledgment does not cause automatic repetition of a mutating action.
- CDP stays on guest loopback, with no published host/public debugger port or access to another bot's browser. Scripts cannot supply an arbitrary host debugger endpoint through the API.
- Preserve the persistent bot profile, browser sandbox, and checkpoint behavior. If an updated browser needs a different automation profile path, migrate existing state explicitly rather than discarding it.
- Stop work cancels the managed script; its lease and any browser activity are reconciled. Backups wait for a consistent browser checkpoint.
- Update capabilities, the computer skill, and examples with the endpoint and tested library/browser versions. Native provider browser tools and sub-agents stay disabled.

Chrome documents remote-debugging switches and the dedicated-profile requirement for recent Chrome versions. Verify the actual pinned Chromium build rather than assuming identical behavior. [Remote debugging](https://developer.chrome.com/docs/devtools/remote-debugging/local-server/), [profile changes](https://developer.chrome.com/blog/remote-debugging-port), [CDP reference](https://chromedevtools.github.io/devtools-protocol/).

Working areas: `infra/guest/supervisord.conf`, `infra/guest/else-browser`, guest helpers/examples, computer lifecycle, and tests.

References: [Current CDP helper](../../infra/guest/else-browser), [managed guest execution](../../apps/api/src/computers/jobs.rs).

## POC-028: Desktop screenshots delivered to the model

Depends on: POC-024.

Add a scoped screenshot operation for the bot's full desktop framebuffer, matching the desktop served by VNC. Return dimensions, coordinate scale, capture identity, time, and image bytes through a supported multimodal tool response or provider image-input path. Keep images in the scoped artifact store when durable retrieval is needed.

Do not pass host file paths to disabled native image tools. Keep a bounded image budget separate from text-tool limits. The provider must actually receive pixels. A file name, URL, text description, or base64 string presented as text cannot pass acceptance.

Acceptance:

- Capture the actual guest desktop, including a window outside the page viewport. Dimensions and scale match the frame the owner sees through VNC.
- An actual model identifies a freshly randomized visual marker rendered only into pixels, without DOM text, OCR output, or a scripted answer being supplied as an alternative.
- Verify the image path for Codex and Claude using supported configured models. An incompatible model/profile reports the missing capability without silently switching models or weakening host restrictions.
- Foreign bot/artifact references, stale/revoked grants, excessive image size, unavailable displays, and owner-stopped computers have explicit outcomes. Only an authorized computer lease may start or capture the assigned VM.
- Log bounded capture metadata and artifact references, not raw base64 in memory or ordinary text journals. Define retention and encrypted-backup coverage for durable screenshots.
- Resume, cancellation, and transport error tests show that image delivery does not reenable native sub-agents or native host file/browser tools.

MCP supports image content blocks; provider interoperability still needs the actual tests above. [MCP tool result types](https://modelcontextprotocol.io/specification/2025-11-25/server/tools#image-content).

Working areas: screenshot helper, guest tools/MCP result handling, provider adapters/tool inventories, artifact storage, and live vision tests.

References: [Current MCP result handling](../../apps/api/src/http/mcp.rs), [provider adapters](../../apps/api/src/providers).

## POC-029: Visual mouse and keyboard control

Depends on: POC-025, POC-028.

Add pointer movement, click, drag, scroll, key/chord, and text-input operations against the bot's own desktop. They act on the same guest display as VNC. Support an observe, act, recapture loop so the model can navigate visually when selectors are unavailable. Keep CDP as a complementary browser interface.

Acceptance:

- A real bot observes its desktop, clicks a control positioned differently on each run, types text, scrolls, and reads the resulting state from a new screenshot.
- Complete a local website search/information task and operate an application outside the browser. The visual case supplies no DOM selectors or hidden target coordinates to the model.
- Validate coordinates against frame dimensions and handle resize/scale changes with a new capture. Bound action batches and the number of unattended iterations per task.
- Key/chord and text input have distinct semantics. Cancellation or disconnect releases pressed keys/buttons and cancels queued input. Input remains scoped to the assigned VM, never the host desktop or clipboard.
- Serialize conflicting manual VNC and bot input. Stop work, owner stop, deletion, and expired grants prevent further input; report uncertain action outcomes without blindly replaying clicks.
- Reuse leases, activity reporting, artifact references, and source-trust rules. Text inside screenshots is task data and cannot grant permissions.

Working areas: guest input helper, computer tools, provider skill, mutation coordination, and live desktop-control fixtures. No new GPUI remote desktop tab is required.

References: [Screenshot task](#poc-028-desktop-screenshots-delivered-to-the-model), [existing guest display](../../infra/guest/supervisord.conf), [remote access boundaries](../security.md#isolation-and-remote-access).

## POC-030: Persistent bot categories and API

Depends on: POC-024.

Add flat, owner-managed categories to PostgreSQL and one nullable category ID per bot. Initial categories have a stable ID and a trimmed nonempty name up to 80 characters; reject case-insensitive duplicate names. Existing bots migrate to the unassigned group. Categories organize the sidebar only: they change neither identity, role, permissions, memory ownership, provider profile, nor VM location.

Provide owner-authenticated list/create/rename/delete operations and assignment/unassignment. A minimal route set is `GET/POST /v1/categories`, `PATCH/DELETE /v1/categories/{id}`, and `PATCH /v1/bots/{id}/category`. Reuse stable create request IDs and existing error/auth/backup-admission conventions. Extend bot records and the existing WebSocket bot-list snapshot to include category metadata and assignments.

Acceptance:

- Real migrations preserve every existing bot/conversation and work on repeated startup. Empty categories can be created and retained.
- Matching create retries return one category; conflicting retries and invalid/duplicate names fail predictably. Unknown categories and deleted bots cannot acquire invalid assignments or be revived.
- Assign, move, and unassign bots. Delete a category transactionally by unassigning its bots; never delete or cancel those bots or their data.
- All connected clients receive committed category/assignment changes, including renaming or deleting an empty category. Reconnect returns a consistent category/bot snapshot.
- Task grants cannot mutate owner category settings. Grouping creates no implicit cross-bot access or permission inheritance.
- Backup/restore preserves IDs, names, assignments, and the saved main-bot pointer. Test changes during a turn and across API restart without interrupting chat or guest jobs.

Working areas: migrations, bot/category application operations, PostgreSQL adapters, owner routes, WebSocket snapshots, and integration tests.

References: [Current bot store](../../apps/api/src/db/bots.rs), [WebSocket channels](../../apps/api/src/http/ws.rs), [backup design](../backups.md).

## POC-031: Grouped sidebar and category controls

Depends on: POC-030.

Render category headers and an unassigned group in the GPUI sidebar. Add small create/rename/delete controls and a category selector when creating or moving a bot. Use deterministic category-name and bot-name ordering with IDs as tie-breakers. Nesting, drag-and-drop, and custom ordering can wait.

Acceptance:

- Create two categories, assign multiple bots, rename a category, move a bot, and remove a category through the actual UI. Empty categories remain visible; unassigned bots remain accessible.
- Selected bot, draft, history position, main marker, unread/activity state where present, and running work survive a move or category rename.
- Refresh after restart and observe updates from a second API client. Errors, pending requests, and reconnection do not duplicate groups or lose selection.
- Deleting a category clearly states that bots are kept. Deleting a bot still uses the existing soft-delete contract. Category controls work with keyboard focus and preserve existing bot-management shortcuts.
- Bots with older records or no category render in the unassigned group. The desktop never writes to PostgreSQL directly.
- Run GPUI checks in the contained desktop required by the environment, using omabox on this host. Do not drive the owner's real session.

Working areas: desktop API types/reducer, sidebar/dialogs, WebSocket refresh handling, and contained UI smoke steps.

References: [Current sidebar](../../apps/desktop/src/app.rs), [bot API](../../apps/desktop/src/api.rs), [category contract](#poc-030-persistent-bot-categories-and-api).

## POC-032: Automation and categories acceptance

Depends on: POC-026, POC-027, POC-029, POC-031.

Add a repeatable integration scenario for this milestone with synthetic content, a local web fixture, and capture-only mail. Keep live inference and VM/UI checks explicit. Preserve the existing POC acceptance suite and record which capabilities were exercised through each provider.

Acceptance:

- A bot authors and runs code with the installed toolchain, produces an artifact, and delivers exactly ten observed test messages to the local capture fixture. Exercise a partial failure without blind retransmission.
- A guest script uses CDP to search the local site through the visible browser. A separate visual task controls a randomized desktop via screenshot feedback and actual pointer/keyboard input, including an application outside Chromium.
- Organize these bots into sidebar categories. Move a busy bot and restart the API/desktop without changing conversation, job, or category identity.
- Capture and restore a coordinated backup, then verify category assignments, project files/environments, usable tooling, and browser profile state. No live CDP/input session is assumed portable; reconnect safely.
- Run the relevant completed-POC regressions. Confirm isolation between two bot computers, the host boundary, owner-stopped behavior, resource limits, and disabled native sub-agents still hold.
- Record measured image/snapshot size and task latency, actual model/CLI/image versions, commands, UI steps, limitations, and owned-resource cleanup. Do not mark the milestone complete from mocked screenshots, DOM-only automation, or synthetic provider responses.

References: [Existing acceptance procedure](acceptance.md), [known limitations](follow-up.md), [development setup](../development.md).
