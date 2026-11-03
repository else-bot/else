# Implement the POC one task at a time

Use [TODO.md](../../TODO.md) as the completion queue. Use the [handoff log](progress.md) to resume work. Task details link to existing design decisions and identify what to build and prove. Use this runbook when the owner assigns an implementation target. Adding or revising tasks does not execute them.

## Start or resume

1. Read the owner's current instructions, repository instructions, TODO.md, and the latest handoff. Run `git status --short` and inspect existing changes before editing.
2. Identify the assigned delivery target. POC-001 through POC-032 are marked complete, including VM automation and sidebar categories (POC-025 through POC-032); no further milestone is queued. Preserve completed checkboxes and their evidence.
3. Resume the active task if its unfinished changes belong to this work. Otherwise choose the first unchecked task in the assigned target whose dependencies are complete and which has no unresolved blocker.
4. Read that task's references and inspect the affected code. Verify the installed tool and dependency interfaces before relying on version-specific research.
5. Record the active task and its immediate next action in the handoff. Implement that task, verify its acceptance checks, and record the result.
6. Check off the task only after its acceptance passes. Commit the code, documentation, checkbox, and handoff together with the task ID in the subject. Continue to the next eligible task until the assigned target is complete.

Use one implementation writer per checkout. If another agent owns unfinished changes, preserve them and use a separate assigned checkout. Do not race edits to the queue or handoff. A stale active-task entry is a reason to inspect the branch and files, not to discard them.

If a task spans sessions, make coherent partial commits with its task ID and leave it unchecked. Record the remaining acceptance checks and exact next action. Preserve a failed check as evidence until a verified fix replaces it.

Routine implementation choices, project dependencies, local builds, and disposable development services are part of the assigned work. Continue within that scope without per-task confirmation. Keep commits local unless the owner also asks to push. Remote services, production credentials, and unrelated host configuration still require appropriate authorization. Platform tool permissions continue to apply.

## Keep the implementation small

Follow [repository boundaries](../architecture.md#repository-boundaries). Start inside `apps/api` and `apps/desktop`. Add a module or shared crate when working code needs that boundary. Keep the API executable's startup separate from application operations, and keep HTTP, PostgreSQL, CLI, and VM details out of application rules.

Runtime bots start with the saved Codex profile `gpt-6.1-sol` / `medium` / `priority`, displayed as Medium Fast. Apply it explicitly and persist owner updates. The [provider policy](../providers.md#saved-execution-profile) is authoritative. Older High-reasoning experiment scripts preserve historical evidence and must not become an implicit runtime default.

Disable provider-native sub-agents in both CLIs. Else.bot delegation uses registered bots and API-managed tasks. Development-agent orchestration follows the owner's development instructions and is separate from this runtime policy.

Use one API process, PostgreSQL, and one scheduler. Add schema through versioned migrations as tasks need it. The desktop uses the API and never accesses the database directly. Provider CLIs run on the host. Guest computers execute scoped tasks. Do not add a broker, workflow engine, extra database, or agent framework to complete this backlog.

Use the [desktop contract](../research/desktop-and-feature-contracts.md) for routes, messages, WebSocket channels, and cancellation. Treat its field names as a coherent proposed contract. If implementation requires a change, update server, client, tests, and the contract together. Record the reason in the handoff. Do not maintain two competing behaviors.

Research fixtures provide useful cases and algorithms. Their PASS results do not complete an implementation task. Adapt the relevant cases to exercise the real API, database, process, or UI.

Declare real tool behavior accurately. The prompt experiment's read-only, non-destructive MCP annotations apply only to its scripted server. Mutating delegation and guest tools need correct metadata and verified authorization through the real API.

## Verify before marking done

A completed task has working behavior, its listed acceptance checks, relevant documentation updates, and recorded evidence. Compilation alone cannot prove persistence, authentication, cancellation, or provider continuity.

Run applicable workspace checks after code changes:

```sh
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test --workspace --locked
```

Run the tests relevant to the task and finish required checks. Repeat them when a later change affects their result. Add behavioral tests for important failure and concurrency paths. Do not add tests that only repeat a constant or mirror implementation details.

Record exact commands or reproducible UI steps, observed results, and the application revision or task commit subject. Use synthetic messages and canary files. Keep credentials, private conversations, raw model reasoning, and unredacted environment dumps out of evidence. Live inference uses the installed subscription only when that provider work is assigned. Keep it bounded and record the CLI, model, and settings used.

Use the [development setup](../development.md). Integration tests need their own database or Compose project and owned resources. Default service data must survive normal startup and shutdown. Never use `down --volumes`, `.replace()`, broad container pruning, or database resets against an existing installation to make a check pass.

Task completion includes stopping disposable services and test processes owned by that task. Preserve the owner's development services and saved data. Give long-lived test jobs explicit cleanup and deadlines.

## Run GUI checks in a contained desktop

Before launching or driving a GUI app or any test that may open a window, follow the environment's GUI-isolation instructions. On this host, read the omabox skill and run GPUI checks in its contained desktop. Never redirect display variables to the owner's real session. Bot desktop input and screenshots must target the assigned micro-VM's display. These are distinct from the development desktop used to test the GPUI client.

## Handle interruptions and blockers

Before a context handoff, record the active task, changed paths, completed checks, exact unfinished action, and any running processes or owned service names. Another agent must be able to resume without rereading the full conversation.

If blocked, leave the task unchecked. Record the failing command or reproduction, evidence, attempted fix, and the exact missing permission, credential, dependency, or decision. Continue with another eligible task inside the assigned target when possible. Do not bypass a security requirement, weaken an acceptance check, or turn an unresolved gap into a checked box.

If no task is eligible, report the concrete blocker. If the assigned target is complete, report its acceptance evidence and stop. Continuing into another target follows the owner's scope, not an assumption that an unchecked item is automatically authorized.

## Completion gates

| Target | Required evidence |
| --- | --- |
| First usable POC | POC-001 through POC-012 complete. Real GPUI, PostgreSQL, and host Codex support two bots, create/delete, durable chat, stop work, and restart recovery. |
| Assistant POC | POC-001 through POC-024 complete. Add verified memory, real one-slot delegation, isolated guest work, Codex/Claude switching, and encrypted remote recovery. |
| VM automation and sidebar categories | POC-025 through POC-032 complete. Prove guest runtimes and scripts, model-visible screenshots and desktop input, CDP scripts against the visible browser, and categories preserved through restart and backup/restore. |

A real implementation can be ready for its first demo while later research integrations remain unfinished. Report the completed target explicitly.

## Continuation prompt

Give an implementation agent this prompt to work through the next milestone:

```text
Work through TODO.md using docs/implementation/README.md.
Target VM automation and sidebar categories, POC-025 through POC-032.
Read docs/implementation/progress.md and inspect the current checkout first.
Resume the active task or pick the first eligible unchecked task.
Implement one task at a time, prove its acceptance checks, update the queue
and handoff, and make a focused commit with its task ID.
Continue without per-task confirmation until the target passes or no eligible
task remains. Keep commits local. Preserve existing data and unrelated work.
```

Earlier POC targets retain their original definitions and completed evidence. Do not reset them to make the new queue eligible. See [follow-up.md](follow-up.md) for known limitations outside this milestone.
