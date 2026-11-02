# Conversation continuity

Research date: 2026-11-02. Scope: continuity for the first version's bot messaging, create bot, and delete bot flows.

Use one PostgreSQL conversation for each bot's initial chat. Keep its ordered history independent of Codex and Claude. Resume each provider's own native session when compatible. On a provider switch, request a bounded checkpoint from the outgoing provider, then send that checkpoint, selected history, and the owner's follow-up to the target provider. Use stored history when the checkpoint fails.

This preserves the accepted [provider requirements](../providers.md), [host and VM split](../architecture.md#host-and-bot-computers), [prompt drafts](../prompts.md#handoff-prompt-drafts), and [first-version scope](../product.md#first-version). Host CLIs retain subscription authentication and native session files. Bot Microsandbox VMs remain task computers behind scoped MCP tools and the Rust API. PostgreSQL, Rust, and GPUI stay unchanged. No workflow engine, provider SDK sidecar, native-history translator, or new product view is required.

Evidence labels apply throughout:

- VERIFIED means inspected upstream source, documented behavior, or an identified local check. It does not imply a live integration test.
- DESIGNED means the proposed else.bot behavior.
- UNVERIFIED means a named integration gap that needs a provider experiment.

## What the current T3 source establishes

VERIFIED. Both requested `orchestrator-v2` and previously documented `orchestration-v2` branch URLs returned 404 during this check. The directory `apps/server/src/orchestration-v2` exists on current `main`. The inspected revision is [commit `30654b3e91a51937d1dba18fc83af19d8003f5ca`](https://github.com/pingdotgg/t3code/commit/30654b3e91a51937d1dba18fc83af19d8003f5ca), committed at 2026-11-02 06:36:23 UTC. The [source manifest](../experiments/continuity/sources.json) records the pin and inspected files. A missing branch does not invalidate the prior commit's source links.

The comparison with [the prior `133b9b0` revision](https://github.com/pingdotgg/t3code/commit/133b9b0c72ce859e4c3ac25aec201d22d82a94ee) supports the existing design:

| Finding at the new pin | Consequence for else.bot |
| --- | --- |
| The app thread retains separate provider threads. A fresh native thread receives `full_thread_summary`; a returning thread receives `delta_since_target_last_seen`. Coverage uses prior delivered runs. [Switch planning](https://github.com/pingdotgg/t3code/blob/30654b3e91a51937d1dba18fc83af19d8003f5ca/apps/server/src/orchestration-v2/Orchestrator.ts#L1770). | Keep the conversation ID stable and track target session coverage. |
| `prepareProviderHandoff` still calls `selectHistory` and `renderHistory`. It creates no LLM summary request. [Handoff construction](https://github.com/pingdotgg/t3code/blob/30654b3e91a51937d1dba18fc83af19d8003f5ca/apps/server/src/orchestration-v2/ContextHandoffService.ts#L484). | Our outgoing LLM checkpoint is an additional requirement. T3's name `full_thread_summary` does not satisfy it. |
| History selection preserves whole items, favors recent user and assistant items and the first user item, and records omissions. Rendered items include source IDs and statuses. [Selection and rendering](https://github.com/pingdotgg/t3code/blob/30654b3e91a51937d1dba18fc83af19d8003f5ca/packages/provider-core/src/server/handoffBudget.ts#L177). | Copy the bounded selection idea and provenance, without copying its coding-specific record system. |
| Delivery persists pending state before injection. Ambiguous injection requires a replacement native thread. Comparing old and current `ContextHandoffDelivery.ts` found only the added `withInlineContext` helper. [Delivery](https://github.com/pingdotgg/t3code/blob/30654b3e91a51937d1dba18fc83af19d8003f5ca/apps/server/src/orchestration-v2/ContextHandoffDelivery.ts#L15). | Persist before sending; never blindly replay uncertain native input. |
| Codex uses app-server. `injectHistory` falls back only on JSON-RPC unknown-method error `-32601`; transport and payload errors remain failures. [Codex adapter](https://github.com/pingdotgg/t3code/blob/30654b3e91a51937d1dba18fc83af19d8003f5ca/apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts#L7144). | Start with inline handoffs. Native injection adds no required first-version behavior. |
| Claude uses `@anthropic-ai/claude-agent-sdk`, with distinct create and resume identities. Existing background work can prevent process replacement. [Claude adapter](https://github.com/pingdotgg/t3code/blob/30654b3e91a51937d1dba18fc83af19d8003f5ca/apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts#L1519), [process replacement](https://github.com/pingdotgg/t3code/blob/30654b3e91a51937d1dba18fc83af19d8003f5ca/apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts#L10306). | Rust can use the structured CLI directly. Keep long guest jobs in the API, outside the CLI's process tree. |

T3's current start service also separates prompt refusal from acceptance. It restores unsent handoffs for refusal before prompt consumption, and continues event ingestion if accepted-delivery bookkeeping fails. That distinction still matters with inline context. [Turn start](https://github.com/pingdotgg/t3code/blob/30654b3e91a51937d1dba18fc83af19d8003f5ca/apps/server/src/orchestration-v2/ProviderTurnStartService.ts#L1592).

## Small host adapter contract

DESIGNED. Two Rust adapters expose `open_or_resume`, `start`, `interrupt`, and a normalized event stream. Events identify the conversation, turn attempt, session generation, native session, native turn where available, and output item. Parse provider JSON at the adapter boundary. Treat unknown event types as unsupported telemetry rather than completed turns. Never infer completion from an assistant text chunk.

VERIFIED. The local offline probe found Codex `0.166.0` and Claude Code `2.1.316`. It generated Codex's installed JSON schemas and checked Claude's help. These checks establish local shapes, not authentication or behavior. [Probe results](../experiments/continuity/results.json).

For Codex, use a host child process with stdio. Send `initialize`, then `initialized`, then `thread/start` or `thread/resume`. Send `turn/start` and consume output through `turn/completed`. Cancellation uses `turn/interrupt`; its successful empty response requests cancellation, while the terminal turn event establishes the outcome. Use `thread/read` for recovery inspection. These are documented app-server operations. [Official OpenAI app-server documentation](https://learn.chatgpt.com/docs/app-server).

For Claude, start with one supervised CLI process per chat turn. Send one structured user message, close stdin, drain output, and retain the native session ID. Launch the next turn with explicit `--resume <native-id>`. Use `-p --input-format stream-json --output-format stream-json --verbose --replay-user-messages`. Allocate a UUID for a new session with `--session-id`; never use directory-relative `--continue` in the service. The flags, specific-session resume, and input echo are documented and present locally. [CLI reference](https://code.claude.com/docs/en/cli-reference), [session semantics](https://code.claude.com/docs/en/agent-sdk/sessions).

The adapter records `system/init` metadata, correlated user echo, assistant output, and terminal `result` separately. Initialization alone does not acknowledge the prompt. A terminal error result remains a failed turn even when the process exits successfully. On interruption, send SIGINT and drain the result. After a deadline, terminate the supervised process tree, mark its turn uncertain, and retire that native session. SIGTERM can leave an unfinished turn without a result; avoid automatic continuation of that turn on resume. Do not use `--bare` with subscription authentication: current documentation says bare mode skips OAuth credentials. [Programmatic CLI behavior](https://code.claude.com/docs/en/headless).

UNVERIFIED. Direct Claude control-request envelopes, interrupt receipts, exact user-echo correlation, and result ordering have not been exercised here. The first version can use process signals and explicit resume without implementing the SDK's control protocol. Keep an active process only if the later provider experiment proves that doing so simplifies the adapter.

## Minimum persistent records

DESIGNED. Six logical records suffice. Existing memory and VM job records remain separate.

| Record | Necessary fields |
| --- | --- |
| Conversation | `id`, `bot_id`, `selected_provider`, `generation`, deletion state. One initial chat per bot. |
| History item | `conversation_id`, monotonically increasing `seq`, `turn_id`, kind, body or artifact reference, source provider, execution status. Unique source event ID where provided. Checkpoints remain internal. |
| Turn attempt | Stable owner `request_id`, ordered owner `turn_no`, attempt number, request hash, owner item ID, provider, session ID, state, native turn ID, prompt version, error. Retries reference the same owner item and turn number. |
| Provider session | Conversation, provider, account reference, host runtime reference, native ID, revision, compatibility fingerprint, active or retired state, `considered_through_turn`. No credentials in this row. |
| Handoff | Target turn attempt, source and target sessions, frozen history boundary, checkpoint text and `llm` or `fallback` kind, prompt version, selected item IDs, omitted item IDs, payload hash, delivery state. |
| Action receipt | Stable `action_id`, originating turn, arguments hash, state, guest job ID or external operation ID, result reference, reconciliation evidence. |

Use a unique constraint on conversation plus owner request ID for the owner message. Reject a reused ID with different content or provider selection. Use conversation plus request ID plus attempt number for turn attempts. Keep received native event IDs unique within their session revision. Store streamed assistant text under one item ID so reconnects do not create duplicate bubbles.

The compatibility fingerprint covers provider account, runtime location, working directory, prompt version, tool policy, the saved model/effort/tier profile, and the disabled native-agent policy. Follow the current [provider settings contract](../providers.md#saved-execution-profile) when updating or resuming a session. A changed fingerprint creates a fresh native session. This also avoids stale instructions on Claude versions that retain a system-prompt snapshot across resume. [Claude prompt snapshot flag](https://code.claude.com/docs/en/cli-reference#system-prompt-flags).

`considered_through_turn` means settled history was evaluated for delivery through that owner turn. It does not mean every item reached the target. The handoff's selected and omitted IDs retain that distinction. Advance imported coverage only after correlated acceptance. After a normal turn settles, also record that session's own input and output coverage. A new or retired session starts at zero. Omitted history stays available through a bounded, bot-scoped history-read tool.

## Switch and resume algorithm

DESIGNED. Run one durable queue per conversation inside the Rust API process. PostgreSQL rows provide recovery; a process mutex serializes normal execution. A single API instance is enough. Bind each queued owner message to its requested provider so later picker changes cannot reroute it. The worker watches provider events and queued switch requests together, so a switch can interrupt its active turn.

```text
enqueue(request_id, conversation, provider, text):
	persist one owner item and queued attempt, or return existing request
	wake conversation worker

run(attempt):
	serialize this conversation
	if conversation is deleted: cancel queued attempt; return
	if prior turn is active:
		if switching provider:
			revoke old generation's tool authorization; interrupt old turn
			if old provider does not stop: terminate it; retire old session
		else: await prior turn completion
		persist drained output and confirmed or uncertain outcome
	frozen_history = settled history from owner turns before attempt.turn_no
	checkpoint = none
	if switching provider:
		checkpoint = outgoing_summary_no_tools(frozen_history, timeout)
		if summary failed or cannot enforce no-tools: checkpoint = fallback_history
	else if recovering a crashed turn:
		reconcile native state and action receipts before continuing
	target = resume compatible target session, else create replacement
	missed = frozen_history after target.considered_through_turn
	packet = checkpoint + selected missed items + action receipts + owner item
	persist handoff and attempt as sending before any provider write
	start target with packet and a new scoped tool authorization
	on correlated acceptance: mark accepted; advance considered-through cursor
	on output: persist under this attempt; stream committed sequence to desktop
	on terminal result: record completed, interrupted, or failed
	on uncertain delivery: revoke authorization; stop provider; retire session
```

The worker uses a frozen boundary for each packet, including ordinary same-provider turns. Freeze after the prior turn stops and its output is recorded. Group history by owner turn number because a queued follow-up can arrive before the previous assistant's final output. Exclude the current and later queued owner turns from historical selection. For a normal same-provider turn, no outgoing checkpoint is needed. For replacement after missing native history, select candidates from the full stored conversation. For returning Codex after Claude, select candidates after Codex's last accepted boundary. Append the current follow-up exactly once as the current request. Include current job receipts independently of the history cursor, since guest jobs can finish after their owner turn settles.

The outgoing checkpoint is a separate internal attempt with its own deadline, default 20 seconds. It receives recorded context and the existing [checkpoint prompt](../prompts.md#handoff-prompt-drafts). Deny every scoped tool call during this attempt and use the host adapter's verified no-tool profile. A fresh summary session on the outgoing provider is acceptable when safely resuming with that profile is unsupported. Never pass the unsent owner follow-up to the outgoing provider as a task. Summary failure does not stop the switch.

UNVERIFIED. Codex's installed stable `TurnStartParams` has no general `toolChoice: none` field. An instruction to summarize is insufficient to disable execution. The host-isolation experiment must establish the actual no-tool profile. Until established, select the deterministic fallback rather than expose tools during checkpoint generation. Claude's `--tools ""` disables built-ins but leaves MCP tools available; its summary profile must also deny MCP tools. [CLI tool restrictions](https://code.claude.com/docs/en/cli-reference).

DESIGNED. Start with configurable caps of 8 KiB for checkpoint text and 32 KiB for the complete imported-history packet. Reserve room for instructions, the owner's message, native context, and response. These byte caps bound transport, not model tokens. Use model context telemetry or a conservative configured limit. If context is too large, replace an old native session, omit historical items with IDs, and retain the owner's message intact. An oversized current message returns a clear size error before delivery. Keep exact action IDs and unresolved outcomes even when prose is omitted.

The fallback renders the goal, selected recorded messages, action statuses, artifact references, and omission references without an LLM. Never truncate JSON or pretend an excerpt is a complete item. A completed checkpoint is historical evidence, not a new system instruction. Bot memory remains independent of both native sessions and checkpoints.

## Failure rules and duplicate actions

DESIGNED. Turn states are `queued -> preparing -> sending -> running -> completed | interrupted | failed`. A lost connection after sending can produce `uncertain` from either `sending` or `running`. Handoff delivery states are `prepared -> sending -> accepted`, with `uncertain` for an ambiguous send. Refusal explicitly known to precede consumption can return to `prepared`. A timeout alone never establishes refusal.

| Failure | Recovery |
| --- | --- |
| Summary timeout, unavailable source, or invalid summary | Persist fallback kind and reason. Continue with bounded stored history. |
| Missing or incompatible target session | Retire it. Create a new native session and rebuild context from PostgreSQL. |
| Target accepted input but API crashed before recording acceptance | Reconcile native history if it establishes the exact attempt. Otherwise stop and retire that session. Prepare a new attempt after action reconciliation. |
| Process died during guest execution | Retrieve the existing job by its recorded ID. Provider exit does not cancel the guest job or release its job lease. |
| Unknown external effect | Inspect the operation's status or reconcile the resulting resource. Keep the action unknown until evidence settles it. Never blindly repeat it. |
| Bot deleted during a turn | Mark deleted, increment generation, deny new tool work, stop provider, and cancel queued turns. Apply the separate VM deletion policy to existing jobs and leases. |

Persist action identity before dispatch. A repeated `action_id` with the same argument hash returns its existing receipt or job status. Different arguments under that ID fail. A crash between external dispatch and receipt persistence leaves the action `unknown`; this state blocks redispatch. Use external idempotency keys when supported. For guest execution, persist the job before starting it, so recovery can find the same job.

Provider-native tool-call IDs can change on a new attempt. They cannot supply cross-provider action identity. Give the receiving provider the API's stable action IDs and require status inspection before further work. The API checks the conversation generation atomically when admitting a tool operation. Revocation prevents new calls from the old provider; already admitted jobs retain their receipts and independent leases.

These rules prevent replay of an identified action. They do not prove that a model with a new action ID cannot request the same real-world effect. Unknown outcomes must block the affected mutating operation until reconciled. For the messaging-only first version, ordinary message creation is a PostgreSQL transaction with a unique owner request ID.

Native resume restores provider context, not an automatic retry of unfinished work. After an interrupted turn, send explicit current instructions with its recorded outcomes. Keep the visible conversation unchanged and mark partial assistant output as interrupted. No hidden retry may erase output the owner already saw.

## Evidence and remaining integration checks

VERIFIED. Run `python docs/experiments/continuity/verify.py` from the repository root. The checked-in [results](../experiments/continuity/results.json) show eight passing synthetic cases plus the installed CLI shape probe. Cases cover request deduplication, the provider round trip, missing sessions, omissions, uncertain sends, stale tool authorization, checkpoint tool denial, and an action's lost-result window. The fixture runs no provider prompts and makes no external changes. Its tool denial is a model invariant, not proof of OS restrictions.

UNVERIFIED. The provider experiment still needs to prove subscription login and renewal under isolation, direct Claude event correlation and SIGINT handling, Codex event recovery, enforced checkpoint tool denial, missing-session recovery, and Codex to Claude to Codex with one continuing guest job. Native acknowledgment and `clientUserMessageId` must not be treated as idempotency guarantees without that evidence. No live provider call was made in this track.

The implementation can proceed with the records and transitions above once the provider and host-isolation tracks settle their adapter contracts. Keep native history injection, forks, merges, worktrees, background provider agents, enterprise identity, and distributed orchestration outside this version.
