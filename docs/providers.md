# Providers and conversation continuity

The provider choices below are requirements. The integration design is proposed. The [November 2 continuity research](research/conversation-continuity.md) pins current T3 Code source, defines minimal records and pseudocode, and passes eight synthetic continuity checks. Local CLI protocol probes passed. A live Codex to Claude to Codex handoff remains unverified.

else.bot must let the owner use different AI providers through their installed, dedicated CLIs and existing subscriptions. Initial support is limited to the `codex` and `claude` CLI agents.

The host running else.bot provides both CLIs. Provider processes, subscription authentication, and native session files live on the host. Each run starts in the bot's project directory, with the CLI's own shell and file tools (see [host provider processes](security.md#host-provider-processes)). Bot VMs provide isolated task execution through else.bot's tools. They do not need provider CLIs or subscription credentials.

This is a core requirement for avoiding provider lock-in. Changing providers should preserve the assistant's memory, configuration, and conversations.

Long-term continuity comes from the bot's persistent memory. Switching providers during an ongoing task also needs a handoff of the current task context.

For example, I might start a task with Codex and switch to Claude before it is finished. When I switch and send my follow-up, else.bot should automatically ask the outgoing provider to summarize the ongoing task. Claude receives that summary and my follow-up in the same else.bot conversation.

## Saved execution profile

The initial Codex development profile is GPT 6.1 Sol Medium Fast:

| Setting | Saved value | Adapter mapping |
| --- | --- | --- |
| Provider | `codex` | Host Codex adapter. |
| Model | `gpt-6.1-sol` | Explicit `model` on thread creation, resume, and turn start. |
| Reasoning effort | `medium` | `model_reasoning_effort` in Codex configuration and `effort` on turn start. |
| Service tier | `priority` | `serviceTier` in app-server requests. The displayed name is Fast. |

Persist one default profile per provider in SQLite, with a revision. Seed the Codex profile only during explicit installation setup or a versioned migration that adds missing settings. Ordinary startup must preserve saved values. Initially all bots use these provider defaults. Per-bot overrides and a desktop model-settings screen can wait.

Expose owner-authenticated `GET` and `PATCH /v1/settings/providers/{provider_id}` for reading and updating the profile. Validate the exact model ID and its supported effort and tier against the selected adapter's capabilities. Runtime task grants and prompt text cannot update settings. Reject unsupported or unavailable profiles clearly. Do not silently select another model, effort, or speed tier.

Snapshot the effective profile and revision when accepting a turn. Queued and running attempts retain their snapshots. A settings change applies to subsequently accepted work, including new summary and delegated tasks. Record requested and provider-reported settings with each attempt when available. Configuration acceptance is not proof of backend scheduling speed.

Apply explicit values when creating or resuming native sessions and starting turns. Include the profile in session compatibility checks. If a session cannot adopt the requested profile safely, replace that native session and rebuild context from SQLite. Preserve the visible conversation and guest computer.

OpenAI documents the reasoning setting and maps the Codex `fast` tier alias to the request value `priority`. Use the canonical saved value above. The local model metadata advertises `medium` and `priority` for `gpt-6.1-sol`. [Codex configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference), [recorded model metadata](experiments/prompts/model-probe.json).

The earlier prompt experiments used High reasoning. Preserve their prompts, settings, and results as historical evidence. They do not validate the new Medium default. Adapter acceptance tests must exercise the saved development profile.

## Provider-native sub-agents are deferred

Each bot uses one provider-native agent at a time. Disable the CLI's own sub-agent spawning, automatic delegation, teams, and equivalent agent-launch tools for both Codex and Claude. This is a fixed POC execution policy. A model-setting update must not enable it. Models or modes that require native sub-agents are unsupported for now.

Apply restrictions through the trusted adapter on every process launch and session resume. Keep inherited user/project configuration, plugins, hooks, and unrestricted command tools from reopening agent-launch routes. A prompt prohibition alone is insufficient. If the adapter cannot enforce the policy for a CLI version, report it unavailable rather than enabling native sub-agents.

| CLI | Initial enforcement approach |
| --- | --- |
| Codex | Set `agents.enabled = false`. For the inspected 0.166.0 runtime, also explicitly disable `features.multi_agent` and `features.multi_agent_v2` in the per-process profile. Verify the exposed tools and actual behavior on create and resume. |
| Claude | Use an explicit built-in tool allowlist that excludes agent spawning. The initial host design can use `--tools ""` and route task operations through scoped MCP. Deny `Agent` explicitly through `--disallowedTools`; verify equivalent legacy tools and other launch routes for the pinned version. |

Codex documents `agents.enabled` as the switch for multi-agent tools. Claude documents denying `Agent` to prevent sub-agent delegation. Its built-in tool restriction does not restrict MCP tools, so allow only the intended else.bot MCP operations. [Codex controls](https://learn.chatgpt.com/docs/config-file/config-reference), [Claude sub-agents](https://code.claude.com/docs/en/sub-agents), [Claude tool flags](https://code.claude.com/docs/en/cli-reference).

Local configuration inspection confirmed the Codex multi-agent flags resolve to false with explicit overrides. It did not run inference or prove that every native spawn route is blocked. Implementation acceptance requires adversarial requests and actual tool, native agent/thread lifecycle, and process observations for each provider, including resumed sessions. Native sub-agents may share a process, so an unchanged process count alone proves nothing.

Codex skills are scoped to the bot's project. A separate `CODEX_HOME` does not stop Codex from discovering the owner's `~/.agents/skills`, ancestor-repository skills, or bundled system skills. Before starting or resuming a thread, the adapter disables every discovered skill except repository-scoped skills within that bot's project. The same filter applies to memory summaries and handoffs. Skills installed for the bot in its own `.agents/skills` remain available. This policy is part of native-session compatibility, so sessions created before the filter rebuild from recorded conversation history.

This policy concerns agents spawned inside a bot's CLI. Planned delegation between registered else.bot bots still goes through the Rust API, scoped tasks, and scheduler. It does not use native CLI sub-agents. Development agents working on the repository remain governed by their assigned development instructions.

## First-version continuity

Start with one conversation per bot and one active provider turn per bot. SQLite owns the ordered conversation. Each provider has a replaceable native session with its own delivered-history cursor. Host adapters handle create, resume, start, interrupt, and normalized events.

Keep one durable turn queue in SQLite and one worker inside the Rust API. A unique client request ID prevents duplicate owner messages. Persist pending handoff state before sending. A lost acknowledgment requires reconciliation rather than a blind replay. Preserve action receipts and guest job IDs across provider changes.

For the first version, deliver checkpoint text and bounded selected history inline before the owner's follow-up. Use an outgoing LLM checkpoint only when the adapter can enforce a no-tool summary attempt. Otherwise use the deterministic history fallback. Native history injection, forks, worktrees, and distributed orchestration can wait.

A root owner task may span several provider attempts. Asynchronous delegation parks a parent attempt, frees its host slot, runs the child, and schedules a continuation without another owner message. The root remains working while it waits. Each continuation uses the same recorded request and child outcomes rather than repeating the original action. See the [delegation contract](research/prompts-and-delegation.md#one-hop-delegation).

The updated source pin is [T3 commit `30654b3`](https://github.com/pingdotgg/t3code/commit/30654b3e91a51937d1dba18fc83af19d8003f5ca). Its handoff builder still selects and renders history without an LLM summary. The [research report](research/conversation-continuity.md) contains current source links, adapter details, pseudocode, and failure rules. The earlier source review below remains historical evidence.

## T3 Code conversation research

Inspected `orchestration-v2` on October 25, 2026, at [T3 Code commit `133b9b0`](https://github.com/pingdotgg/t3code/commit/133b9b0c72ce859e4c3ac25aec201d22d82a94ee). The findings below describe that revision.

T3 Code owns the visible conversation and tracks separate native provider threads underneath it. A provider switch selects or creates the target provider thread. A fresh native thread receives context from the stored history; a returning thread receives the intervening work through `delta_since_target_last_seen`. The application conversation keeps its identity throughout. [Provider selection and handoff planning](https://github.com/pingdotgg/t3code/blob/133b9b0c72ce859e4c3ac25aec201d22d82a94ee/apps/server/src/orchestration-v2/Orchestrator.ts#L1580).

The CLIs sit behind adapters with different transports:

| Provider | T3 Code integration |
| --- | --- |
| Codex | A child process connected through the Codex app-server client. Its adapter can inject historical messages with `thread/inject_items`, falling back when that method is unsupported. [Process connection](https://github.com/pingdotgg/t3code/blob/133b9b0c72ce859e4c3ac25aec201d22d82a94ee/apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts#L1409), [history injection](https://github.com/pingdotgg/t3code/blob/133b9b0c72ce859e4c3ac25aec201d22d82a94ee/apps/server/src/orchestration-v2/Adapters/CodexAdapterV2.ts#L6730). |
| Claude | `@anthropic-ai/claude-agent-sdk` controls the Claude Code process through `query`. T3 Code retains native session IDs and resumes established sessions when reopening them. [SDK query](https://github.com/pingdotgg/t3code/blob/133b9b0c72ce859e4c3ac25aec201d22d82a94ee/apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts#L916), [session resume](https://github.com/pingdotgg/t3code/blob/133b9b0c72ce859e4c3ac25aec201d22d82a94ee/apps/server/src/orchestration-v2/Adapters/ClaudeAdapterV2.ts#L9331). |

Despite the name `full_thread_summary`, the inspected handoff builder does not ask an LLM to summarize. It selects stored history and renders it with source IDs and execution statuses. Selection favors the latest user and assistant items, then the first user item, then recent items that fit the budget. It keeps selected items intact and records omissions. [Handoff builder](https://github.com/pingdotgg/t3code/blob/133b9b0c72ce859e4c3ac25aec201d22d82a94ee/apps/server/src/orchestration-v2/ContextHandoffService.ts#L410), [history selection and rendering](https://github.com/pingdotgg/t3code/blob/133b9b0c72ce859e4c3ac25aec201d22d82a94ee/packages/provider-core/src/server/handoffBudget.ts#L127).

Delivery uses native history injection when available. Otherwise, T3 Code places the historical context before the owner's current message. It tracks delivered item IDs and pending delivery per native thread. An uncertain injection requires replacing that native thread before retrying, which avoids blindly injecting the same history again. [Delivery tracking](https://github.com/pingdotgg/t3code/blob/133b9b0c72ce859e4c3ac25aec201d22d82a94ee/apps/server/src/orchestration-v2/ContextHandoffDelivery.ts#L27), [inline context and acceptance](https://github.com/pingdotgg/t3code/blob/133b9b0c72ce859e4c3ac25aec201d22d82a94ee/apps/server/src/orchestration-v2/ProviderTurnStartService.ts#L1190).

## Simplified direction for else.bot

Use one SQLite conversation per ongoing exchange with a bot. Use each CLI's native threads or sessions to retain provider context, and resume them when compatible. Scope those sessions to the bot, conversation, provider account, and host runtime environment. Keep bot session files and working directories separate on the host, even when bots use the same provider account. A provider switch keeps the same bot, memory, conversation, and computer.

The Rust API should own conversation state and provider selection. Each provider adapter should handle process communication, session creation and resume, streamed responses, tool approvals, interruption, and errors. Start with Codex's app-server protocol. For Claude, the proposed Rust integration is the CLI's structured input and output, using `-p --input-format stream-json --output-format stream-json --verbose` and an explicit `--resume <session-id>` when reopening a session. This adapts T3 Code's approach without requiring its TypeScript SDK inside the Rust API. Verify the complete control exchange against pinned CLI versions during the integration experiment. [Claude CLI formats and resume flags](https://code.claude.com/docs/en/cli-reference), [streaming session behavior](https://code.claude.com/docs/en/agent-sdk/streaming-vs-single-mode).

Keep the initial history delivery simple. Include a labeled handoff before the new user message for both providers. Native history injection can become an adapter capability later. Keep provider protocol details inside the adapters.

The database needs these records, with exact table names left for development:

| Record | State to preserve |
| --- | --- |
| Conversation | Bot identity, active provider selection, and ordered conversation history. |
| Turn | Stable request ID, owner message, provider used, saved profile revision and effective model settings, execution status, assistant output, and recorded tool outcomes. |
| Provider session | Conversation, provider and account identity, native session ID, runtime location, compatible execution profile, and confirmed history coverage. |
| Provider settings | Provider, exact model ID, reasoning effort, service tier, and revision. Owner-controlled defaults survive startup and backup restoration. |
| Handoff | Source and destination sessions, covered history range, summary, selected and omitted item IDs, prompt version, and delivery status. |

On a provider switch:

1. Save the owner's follow-up and target provider. Serialize execution within the conversation. Finish or confirm interruption of the active provider turn before handing it over. Retain completed action results and the IDs of any guest jobs that continue under their own leases.
2. Ask the outgoing provider for a bounded task summary with tool execution disabled. Save the result as an internal handoff record. This summary step is specific to else.bot.
3. Reuse or resume the target provider's native session. Select the history it missed. For a fresh session, select context from the whole conversation. If native resume fails because the session is unavailable or incompatible, create a replacement and rebuild context from SQLite.
4. Assemble the summary, selected history, artifact references, and the owner's follow-up. Apply configurable size limits and reserve room for the new message and response. Record omitted history so the bot can retrieve it through else.bot's API or a skill.
5. Persist delivery as pending before sending. Mark the accepted handoff and its history coverage only after provider confirmation. Track selected and omitted items separately so skipped content is still retrievable.
6. Save the receiving provider's output and action results into the same conversation. Release each VM activity lock when the associated guest work finishes. Background guest jobs retain their own leases. A provider turn does not require a VM lock for its full duration.

If the outgoing provider is unavailable, times out, or returns an unusable summary, construct the handoff deterministically from stored history. Preserve the complete conversation independently of summaries. Make the summary timeout, context budget, history selection limits, and prompt templates configurable.

If delivery becomes uncertain, recover with a fresh native session and recorded context. Check the status of any uncertain external action before repeating it. Stable request IDs and delivery records help prevent duplicates, but a provider acknowledgment does not prove that every external action completed.

A switch transfers recorded context and results. Native reasoning state, live tool handles, and running execution cannot move between providers. Reattach required files or provide accessible artifact references. Load the bot's instructions, memory access, and available skills for each fresh session.

This design needs a conversation queue, persisted state transitions, and two adapters. T3 Code's coding-specific worktrees, file checkpoints, and conversation fork and merge machinery are outside the initial scope.

## Host-to-VM tools and skills

The proposed communication path is the host CLI, scoped else.bot tools exposed through MCP, the Rust API, and the Microsandbox guest-agent channel. Skills teach the CLI how to use this path. Tools execute operations and enforce permissions independently of the skill text.

The initial tool contract needs these operations. Exact tool names remain open.

| Operation | Required behavior |
| --- | --- |
| Inspect the bot computer | Return its desired and actual state, available capabilities, and active jobs without starting it. |
| Execute guest work | Acquire a lease, start or resume the assigned VM if allowed, and execute with explicit guest working directory, timeout, and resource limits. Return a job ID. |
| Observe or cancel a job | Stream or retrieve bounded output, report exit status, and confirm cancellation. Keep a lease while the job still runs. |
| Read, write, or transfer files | Resolve paths within the assigned guest. Transfer artifacts through scoped API references without exposing arbitrary host paths. |
| Use the browser or desktop | Route browser actions, screenshots, and input to the assigned VM. Share its activity lease rules with VNC access. |
| Finish guest work | Release the caller's lease without stopping unrelated jobs or desktop sessions. Let the lifecycle manager apply the idle policy. |

The API derives bot and task identity from the authenticated tool session. Model-supplied identifiers cannot grant access to another bot's VM. Guest output is untrusted task data. No guest operation grants access to host administration or runtime management.

The computer skill should explain guest paths, file transfers, job tracking, cancellation, and lease cleanup. It should make the execution location clear for every operation. Memory, notes, email, and other application tools can run without starting a VM. Guest processes do not need a second AI agent to interpret requests.

A provider switch reuses the same bot computer and recorded job IDs. The receiving CLI checks existing job status through tools before submitting more work. Switching providers does not itself restart the VM or rerun commands.

The CLIs' own shell and file tools run on the host with the owner's access; browser and web tools stay disabled. Guest work goes through this route, and the bot's instructions keep host work to its project directory unless the owner asks for work on their machine. The [decision record](decisions/0001-providers-run-on-the-host.md) explains why this is not enforced by a sandbox.

## Handoff prompts

The outgoing summary and receiving continuation templates are in [prompt drafts](prompts.md#handoff-prompt-drafts). Bot computers and activity locks are described in [sandbox lifecycle](sandboxes.md).

## Integration validation

Synthetic handoff checks and installed CLI shape probes now pass. The next live integration experiment should use host-installed CLIs and cover:

- Subscription login, token renewal, and native session resume on a headless host, with bot session state kept separate.
- GPT 6.1 Sol Medium Fast is passed explicitly and recorded. Updated saved settings affect only new work, survive restart, and never silently fall back.
- Native sub-agent tools and equivalent launch routes remain disabled after create, resume, settings updates, and provider switches. Record actual process and tool observations.
- Both providers following the computer skill and calling the scoped tools for guest commands, files, and browser work.
- A conversation and memory operation while the VM remains stopped, followed by a guest task that acquires and releases a VM lease.
- Attempts to use native CLI tools to read host secrets, execute arbitrary host commands, or access another bot's files. Verify actual access is denied.
- Codex to Claude to Codex in one conversation, process restart, an unavailable outgoing provider, a missing native session, an interrupted task, and an uncertain delivery.
- A provider switch while a guest job exists. The next provider retrieves its status and completed results without submitting the work again.

Check that the visible conversation stays intact and that completed actions are not repeated. These cases have not yet been tested in else.bot.
