# Desktop and Codex tasks

These tasks complete the [first usable POC](../../TODO.md#first-usable-poc). Follow the [runbook](README.md). Keep the UI to bot management and messaging.

## POC-008: GPUI window and API connection

Depends on: POC-004.

Pin compatible GPUI and platform dependencies and replace the desktop starter with a native window. Add a small HTTP client, development server configuration, and private owner-token loading. Show connection state and a bot sidebar from the real API. Keep network work outside render callbacks.

Acceptance:

- Build and open the window on the owner's Linux desktop using documented prerequisites and commands.
- Connect to the real authenticated API and render saved bots. Bad credentials and an unavailable server give useful errors.
- Credentials stay out of URLs and logs and are not forwarded across redirects or changed server origins.
- Focus, basic keyboard navigation, and shutdown work. Closing the window leaves API work running.
- Record the tested dependency revision and platform requirements. A compile-only check does not complete the task.

References: [GPUI boundary and window design](../research/desktop-and-feature-contracts.md), [development guide](../development.md).

## POC-009: Desktop bot management and messaging

Depends on: POC-006, POC-007, POC-008.

Add create/delete dialogs, the selected transcript, composer, send and Stop work controls, provider selection, and live updates (first SSE, now WebSocket channels). Use the synthetic adapter to prove the full client/API path before Codex is connected. Keep drafts per bot and stable IDs for optimistic messages.

Acceptance:

- Create two bots, message both, switch selection, and delete a bot through the UI. Opening chats starts no VM.
- Enter, Shift+Enter, paste, composition, text selection, and long-message wrapping behave as specified.
- Stream replies, reconnect, prepend older history, and retain scroll position without duplicate bubbles. Preserve a draft during a failed or uncertain send.
- The provider picker affects the next accepted message. Queue, failure, cancellation, and deletion states remain visible.
- Restart the desktop and recover saved history. No email, notes, kanban, or remote desktop view is introduced.

References: [Desktop contract and acceptance cases](../research/desktop-and-feature-contracts.md).

## POC-010: Restricted host provider execution

Depends on: POC-002, POC-007.

Implement the trusted host process launcher and scoped tool boundary. Keep provider authentication on the host while separating bot session directories, task grants, and API secrets. Prove supported provider controls plus operating-system restrictions before allowing sensitive owner data. Select a mechanism that preserves subscription login and structured CLI communication.

Acceptance:

- Launch a supervised harmless process with explicit paths and an environment allowlist. The master key, owner token, database credentials, and unrelated host files are inaccessible.
- Use disposable canaries to test actual denied host reads, native task commands, sibling-session reads, and access to runtime management.
- The tool boundary derives bot/task identity from trusted context. Forged IDs, revoked grants, and foreign objects fail. Task grants cannot use owner management routes.
- Parent exit, cancellation, and API restart leave descendants discoverable and terminable. Host capacity is released only after confirmed exit.
- Disable provider-native sub-agents at launch and resume. Prove that an explicit spawn request cannot expose native agent tools or create child agents. Inspect native agent/thread lifecycle events as well as processes. Reject modes that require them. Preserve the ban under inherited configuration and settings changes.
- Verify the installed Codex launch profile preserves supported login without copying credentials into guests or prompts. Record any genuine blocker; environment filtering or a prompt instruction alone cannot pass this task.

References: [Host process requirements](../security.md#host-provider-processes), [task grants](../research/authentication-and-permissions.md), [process lifecycle](../research/execution-lifecycle.md).

## POC-011: Codex conversation adapter

Depends on: POC-005, POC-006, POC-007, POC-010.

Implement the host Codex app-server adapter with version-checked structured messages, prompt configuration, event normalization, interruption, and native session resume. Persist session identity and compatibility information, including the effective model profile and fixed native-agent policy. Expose real availability through capabilities. Keep tool access limited to operations already implemented.

Acceptance:

- Use `gpt-6.1-sol`, reasoning `medium`, and `serviceTier = priority` explicitly on create, resume, and turn start. Check returned settings where exposed and reject silent substitution.
- Send bounded synthetic messages through the API using the installed subscription. Persist and stream actual responses through the existing UI contract.
- Two bots keep separate native sessions and working directories. A follow-up resumes the correct bot's context.
- Interrupt and drain an actual turn. Confirm the terminal outcome, reap the process as needed, and preserve partial text.
- Restart the API and recover compatible sessions. A missing session rebuilds context from PostgreSQL without duplicating the owner's request.
- Terminal events determine completion. Never save raw private reasoning or infer action success from an assistant text chunk.
- An owner profile update survives restart and affects only new work. Native sub-agents stay disabled even if prompt text asks for parallel agents or a different model.
- Unknown delivery does not trigger a hidden retry. Document live-test commands, CLI version, model settings, and bounded usage.

References: [Continuity adapter contract](../research/conversation-continuity.md), [tested prompts](../research/prompts-and-delegation.md), [protocol fixtures](../experiments/continuity/verify.py).

## POC-012: First usable POC acceptance

Depends on: POC-009, POC-011.

Run the first checkpoint through the actual desktop, API, PostgreSQL, and host Codex. Add a repeatable smoke procedure using synthetic conversations and owned test data. Keep subscription inference explicit so routine automated checks do not consume it silently.

Acceptance:

- Create two differently named bots with roles and converse with each through Codex. Their context and histories stay separate.
- Verify the saved Medium Fast profile survives restart, owner updates apply to subsequent work, and neither bot spawns provider-native sub-agents.
- Restart the desktop and API twice. Recover the same bot IDs, default pointer, ordered messages, and usable conversations without resetting data.
- Demonstrate a failed connection, unknown send outcome, Stop work, and deletion during active work. No duplicate message, orphan process, or resurrected bot remains.
- Delete the main and last bot in a disposable installation. Startup respects the empty state and retains tombstoned data.
- Run feature tests and workspace checks. Record reproducible UI steps, actual outcomes, and remaining limitations in the handoff.
- Update development instructions and the README to describe running behavior. Mark the first usable POC complete only after this actual demo passes.

References: [Desktop acceptance](../research/desktop-and-feature-contracts.md#proposed-acceptance-cases-and-evidence), [first-version scope](../product.md#first-version), [startup preservation](../restore.md#startup-must-preserve-existing-state).
