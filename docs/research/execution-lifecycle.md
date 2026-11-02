# Execution lifecycle

Research date: November 2, 2026. Status: DESIGNED, with VERIFIED synthetic checks and existing runtime evidence. No application code, provider invocation, or VM launch belongs to this track.

Use one Rust API process with one scheduler and PostgreSQL. Keep three independent lifetimes: a host provider turn, a guest job, and a renewable VM lease. A provider turn does not reserve a VM. A running guest job owns its own lease, even after its provider turn ends. Stop an idle VM to reclaim RAM. Preserve its identity, files, and browser profile.

This preserves the accepted [host execution model](../architecture.md#host-and-bot-computers), [provider continuity](../providers.md#simplified-direction-for-elsebot), [sandbox lifecycle](../sandboxes.md#vm-locks-and-resource-management), [stop and soft-delete rules](authentication-and-permissions.md#stop-and-deletion-must-invalidate-queued-work), and [startup preservation rules](../restore.md#startup-must-preserve-existing-state). Initial UI work remains multiple-bot messaging and bot creation and deletion. Background jobs here are commands started by a message, not a routines system. Email, notes, kanban, routines, and mobile remain later.

## Evidence and limits

VERIFIED below means either an observed local fixture result or a specific existing runtime test. DESIGNED means a proposed else.bot rule. UNVERIFIED means an integration check still needs to run.

| Status | Evidence | Consequence |
| --- | --- | --- |
| VERIFIED, existing runtime experiment | Microsandbox v0.7.13 stopped and restarted with files intact. Pause preserved a guest process. Stop removed the worker. Pause retained about 190 MiB in the small service test. [Local measurements](../experiments/bot-vm-verification.md#resource-behavior). | Idle stop is the default. Pause does not release its memory reservation. |
| VERIFIED, existing runtime experiment | Named Rust SDK reconnection preserved files and existing resource settings. CLI-created detached guests survived the creating process. [Runtime and SDK checks](../experiments/bot-vm-verification.md). | Stable bot environment names are usable. Detached Rust ownership across API crashes still needs testing. |
| VERIFIED, local artifact inspection | Saved help from the pinned v0.7.13 bundle says graceful-stop timeout fails without killing. The [evidence receipt](../experiments/lifecycle/runtime-evidence.json) retains the help hash and relevant text. [Pinned release](https://github.com/superradcompany/microsandbox/releases/tag/v0.7.13). | A timed-out stop retains its reservation and requires observation. |
| Documented, integration UNVERIFIED | Current SDK docs distinguish attached and detached lifetime, describe managed job lookup, and say cancelling a job wait leaves the job running. [Sandbox ownership](https://docs.microsandbox.dev/sdk/rust/sandbox), [managed jobs](https://docs.microsandbox.dev/sdk/rust/execution#managed-jobs). | Use detached VM ownership and retained managed jobs if the pinned SDK passes integration. These moving docs do not prove every v0.7.13 method. |
| VERIFIED, synthetic model | Twelve idle/acquire restart cases, twelve deletion/start restart cases, four dispatch crash cuts, and nine edge checks passed. Two edge checks intentionally reject broken policies. [Fixture](../experiments/lifecycle/check.py), [results](../experiments/lifecycle/results.json). | The proposed ordering avoids the modeled races. This does not establish runtime or SQL correctness. |

## Persistent state

DESIGNED. Add lifecycle fields to existing bot, turn, and provider-session records. Add only guest-job and VM-lease records. Existing conversation events carry user-visible outcomes. Do not add a generic workflow graph, broker, Redis, or a second durable queue.

| Record | Minimum durable fields |
| --- | --- |
| Bot | `id`, `status = active \| deleting \| deleted`, `computer_desired = on_demand \| stopped`, `computer_generation`, unique never-reused `vm_name`, optional `runtime_id`, pinned image digest and runtime configuration, `vm_phase = stopped \| starting \| running \| stopping \| paused \| unknown`, `vm_generation`, `operation_deadline_at`, `idle_since`, last error. |
| Turn | Existing request ID, bot, conversation, provider-session reference, ordered message position. Add `phase = queued \| starting \| running \| cancelling \| finished`, outcome, `deadline_at`, `cancel_reason`, provider attempt ID, delivery acknowledgment, and optional native turn ID. |
| Provider session | Existing bot, conversation, provider, account, native session ID, and confirmed history coverage. Add active host process identity: host boot ID, process start time, and owned process-group or cgroup reference. Clear it only after observed exit. |
| Guest job | `id`, bot, originating turn, immutable request key and argument hash, admitted `computer_generation`, foreground or background, `phase = queued \| dispatching \| running \| checking \| cancelling \| finished`, outcome, absolute deadline, runtime job ID when known, bounded output reference and cursor, exit evidence. |
| VM lease | `id`, bot, holder kind and holder ID, request key and argument hash, admitted `computer_generation`, `state = waiting \| held \| released`, expiry, and last renewal. A job lease has exactly one job holder. |

Outcomes include `succeeded`, `failed`, `cancelled`, `timed_out`, `interrupted`, and `unknown`. Execution phase and effect certainty are separate. A confirmed dead process can have an unknown action outcome. Persist that uncertainty in the job or turn result instead of reporting a successful cancellation as proof that nothing happened.

Use unique constraints for scoped request keys and one job lease per job. Reject a repeated key with different arguments. A repeated release returns its previous result and never reacquires a lease. A new attempt needs a new request ID. PostgreSQL supports unique and partial unique indexes for these restrictions. [PostgreSQL 18 constraints](https://www.postgresql.org/docs/18/ddl-constraints.html#DDL-CONSTRAINTS-UNIQUE-CONSTRAINTS).

One active provider turn per bot is enough initially. Before starting another provider attempt, reap any previous process or reuse its verified idle transport. Order pending turns by message position within each conversation and creation order across that bot's conversations. This is stricter than conversation serialization but avoids two providers controlling one desktop. Other bots can run concurrently within host limits. Store the queue in turn rows. A provider session may persist on disk while its process does not.

Bot status and desired computer state have different meanings. An active bot accepts host turns and memory work even when its computer is explicitly stopped. `on_demand` permits guest use to start compute. `stopped` forbids guest tools from waking it. Idle VM stop leaves `computer_desired` unchanged. Only explicit resume restores `on_demand`, and it does not replay work admitted before the stop. No additional stop or resume UI is required for the first release.

Deletion immediately persists a tombstone with shutdown pending, represented here as `deleting`. It revokes access and cancels all work. `deleted` confirms shutdown completed. Both states deny new host turns and guest work. Chats, memory, VM disk, and native session files remain until a future explicit purge. Startup cannot reuse the identity or recreate its VM.

## Root tasks and provider attempts

Cross-track integration decision: a root owner task can remain working while no provider process is active. For asynchronous delegation, park the parent attempt after the child is accepted, reap its process, and release its host reservation. The scheduler then runs the child and queues a parent continuation with its recorded result. This works with one provider slot and adds no second scheduler. The [delegation contract](prompts-and-delegation.md#one-hop-delegation) defines the bounded one-hop behavior.

The single active turn rule below applies to an active provider attempt. A parked root task cannot keep the only provider slot reserved. The synthetic lifecycle fixture does not test real child scheduling. The prompt fixture separately checks pending-result handling and continuation.

## One scheduler owns external work

DESIGNED. Run one scheduler in the API process. Hold an exclusive local process lock beside the runtime storage for its full lifetime. A second API instance targeting that storage refuses to run. This is a single-host constraint, not distributed leader election.

All lifecycle mutations pass through this scheduler. API handlers write requests and wake it. The per-bot mutex from the auth design orders dispatch acceptance, stop, and deletion. Immediately before handing work to the trusted manager, recheck grants, task state, bot status, desired computer state, and the admitted computer generation under that mutex. Release it after manager acceptance, not after job completion.

A bounded timer scan reads unfinished turns, jobs, leases, and VM transitions. Start with a one-second scan and an in-process wake signal. PostgreSQL notifications are optional wake hints if another process later writes requests. Rows remain authoritative because notification delivery occurs after transaction commit. [PostgreSQL 18 NOTIFY](https://www.postgresql.org/docs/18/sql-notify.html).

Use short transactions with `SELECT ... FOR UPDATE` on the affected bot. Claim a transition and commit before calling a CLI or runtime. Row locks end at transaction completion, so no lock survives a slow external operation. Use a fixed bot-first row lock order. [PostgreSQL 18 row locks](https://www.postgresql.org/docs/18/explicit-locking.html#LOCKING-ROWS).

One in-flight VM lifecycle call per bot is allowed. Waiting acquisitions cannot use a VM marked `stopping`. `computer_generation` invalidates guest operations on owner stop, explicit resume, or deletion. `vm_generation` identifies an individual start or stop transition. A completion carries its persisted VM generation. A stale completion cannot grant a lease or overwrite newer intent. It still triggers runtime observation, since ignoring its result cannot undo a VM that actually started.

```text
submit_turn(bot, request_key, message):
	transaction:
		lock bot
		return existing turn if key and argument hash match
		require bot.status == active
		insert queued turn with deadline and ordered message
	wake scheduler

run_turn(turn):
	transaction:
		lock bot
		require active bot, no other active provider turn, available host budget
		reserve host capacity through turn.phase = starting
		persist provider attempt ID and delivery pending
	spawn restricted host provider process with isolated bot session paths
	persist process identity before allowing it to receive the owner message
	deliver message once, persist native acknowledgment when available
	stream bounded events and tool outcomes to PostgreSQL
	on confirmed final response: finish turn, then close and reap provider process
	release host capacity only after process exit, even if turn already finished
```

The spawn sequence needs a launch gate so the provider cannot act before the API records its identity. A small trusted launcher can wait on a pipe before executing the CLI. A crash before that gate opens closes the pipe and exits the launcher. The integration must also prove that descendants remain discoverable after API death.

Close idle provider processes after the turn and resume their native session on the next turn. If a provider requires a retained transport, count that idle process against the host budget and evict it first under pressure. Transport details remain inside the existing two provider adapters.

## Guest jobs and VM leases

DESIGNED. The API derives bot identity from scoped tool authorization. A model cannot choose another bot's VM, host path, or runtime credentials. Only a tool needing the computer acquires a lease. Reading database conversation state or observing a stopped computer does not start it.

```text
acquire_vm(holder, request_key):
	transaction:
		lock bot
		return existing receipt if key and arguments match
		require active bot, computer_desired == on_demand, holder authorized
		insert waiting lease with current computer_generation
		if running, reconciled, and no conflicting mutation: grant held lease
		if stopped and capacity available:
			set starting, increment generation, persist operation deadline
	commit before start or reconnect call
	on observed start completion:
		lock bot, verify identity and generation
		if active and on_demand: grant eligible leases with matching computer_generation
		otherwise: retain capacity and request stop, grant nothing

submit_guest_job(turn, request_key, arguments):
	transaction:
		lock bot, deduplicate key, insert job and lease with current computer_generation
	when lease held:
		persist dispatching before sending command
		recheck authorization and computer_generation at manager dispatch acceptance
		submit managed guest job once with guest cwd, deadline, and limits
		persist runtime job ID, then running
	observe status and bounded logs independently of provider turn
	on observed exit: persist outcome and release only this job's lease
```

Treat file transfer, browser actions, and desktop access as lease holders too. Start with one mutating guest job or tool operation per bot. It holds the mutation slot until confirmed completion. VNC viewing can coexist and owns its own activity lease. Manual desktop input uses the same mutation slot. Status and output reads do not need that slot. More parallel work within a bot can wait until a real use case requires it.

Foreground jobs normally finish before their tool call returns. A background command returns its durable job ID immediately, and the API renews its job lease while runtime observation confirms it is alive. It remains visible through job tools and conversation events. A provider switch retains accepted jobs and gives their IDs and status to the next provider. The new provider observes those jobs before submitting replacements.

The lease heartbeat belongs to the actual holder. The scheduler renews job leases from live work. The desktop proxy renews desktop leases while the authenticated connection remains alive. A client disconnect expires its desktop lease after a grace period. It does not cancel a turn or release a running job's lease. Short file operations release in cleanup paths after completion or confirmed failure.

Lease expiry requests reconciliation. A still-running job retains its lease. Unknown runtime activity retains the reservation and blocks idle stop. An expired desktop connection can be released once the proxy confirms it closed. Installed guest desktop services do not each own a lease. Arbitrary daemons launched by a task count as task work and cannot become untracked ways to keep computing after lease release.

Managed jobs have documented retained IDs and inspect, wait, log, and signal operations. Their kill operation still requires a later exit observation. These are candidates for the thin Rust adapter, not proven v0.7.13 integration behavior. [Managed job API](https://docs.microsandbox.dev/sdk/rust/execution#managed-jobs).

## Idle policy, budgets, and cancellation

DESIGNED. When the final holder releases, persist `idle_since`. Clear it when a new lease becomes held. After the idle timeout, lock the bot row and recheck all held leases, waiting acquisitions, unfinished jobs, and runtime uncertainty. Only then claim `stopping`. A new acquisition arriving after this claim waits for stopped observation and a fresh start. It cannot cancel a stop request already delivered.

Use stop for the first version. Optional brief pause can come later, when there are no holders and no background jobs. A paused VM keeps its complete admission reservation until stop releases RAM. Neither idle policy nor startup replaces a persisted environment. Current SDK documentation says `connect_or_create()` starts stopped environments, so desired-state checks must precede it. [SDK reuse and ownership](https://docs.microsandbox.dev/sdk/rust/sandbox).

Resource admission has separate pools and one combined host-memory check:

| Pool | Admission rule |
| --- | --- |
| Host providers | Cap live provider process groups separately. Reserve configurable CPU and RAM per process, including children. Keep the reservation while a finished turn's process still exits or its status is unknown. |
| Bot VMs | Reserve 1 vCPU and 2 GiB guest RAM per active desktop VM, plus a measured host-overhead allowance. Count starting, running, paused, stopping, and unknown workers. Enforce a separate maximum resident VM count. |
| Shared host | Reserve memory and CPU for Linux, the Rust API, PostgreSQL, and runtime overhead before admitting either pool. Admission requires both the pool limit and the combined host limit to fit. |

The guest allowance follows the accepted [prototype budget](../sandboxes.md#microsandbox-footprint-and-rust-deployment). The overhead allowance and host CLI limits need measurement. Worker RSS is an observation, not a safe maximum. An MVP with one provider slot and one resident VM slot is a valid deployment, while retaining multiple saved bots.

Track CPU separately for host CLI work and guest execution. A vCPU setting does not limit the host provider process. Linux cgroup v2 provides CPU, memory, process-count limits, and process-tree killing. Deployment needs delegated, owned groups or equivalent service-manager support. It must place provider descendants and VM workers in the intended groups without relying on a PID alone. No cgroup changes were attempted in this track. [Linux cgroup v2 controls](https://docs.kernel.org/admin-guide/cgroup-v2.html).

Queue eligible requests oldest first, skipping bots blocked by their own active turn or VM transition. Cancelled requests leave the queue. A queued request's deadline includes time waiting for resources. Reject new messages when a configurable small per-bot pending limit is full. Do not let repeated wake signals add duplicate work.

Proposed starting settings are a 60-second idle timeout, a 60-second lease with 15-second renewal, a 30-minute provider-turn deadline, and a 10-minute guest-job deadline. Background jobs can request a bounded extension. Bound VM start observation to 30 seconds and graceful stop to 10 seconds. These settings are DESIGNED tuning values, not measured performance claims.

Persist absolute UTC deadlines. Use monotonic timers while a process runs and database time when reconciling persisted deadlines. Heartbeat renewal cannot extend a job's execution deadline. Silent provider output alone is not an error before its deadline.

| Trigger | Required result |
| --- | --- |
| Owner cancels a turn | Persist cancellation. Revoke that turn's ability to start tools. Interrupt provider generation and cancel its foreground guest jobs. Explicit background jobs retain their independent leases. |
| Provider switch | Finish or interrupt the outgoing turn before dispatching the new provider. Preserve accepted guest jobs and their leases. Handoff records their current status. |
| Job deadline or job cancel | Reject new work for that job. Send graceful process-group termination, wait a bounded grace, then force-kill if supported. Confirm exit before releasing its lease. Preserve uncertain side effects. |
| Provider deadline | Interrupt through the adapter, then terminate its host process group if needed. Confirm exit and cancel foreground jobs. Background jobs follow their own deadlines. |
| Owner stops the computer | Persist `computer_desired = stopped` and increment computer generation. Invalidate queued guest operations, close desktop sessions, cancel guest jobs, and stop the VM. Host turns and memory work remain eligible. A paused VM may need controlled resume solely to finish shutdown. |
| Owner deletes a bot | Persist the tombstone and increment computer generation. Reject new turns and acquisitions, revoke task grants, close streams, and cancel all provider and guest work. Stop compute while retaining chats, memory, VM disk, and session files. |

An async timeout only ends the wait unless the underlying operation explicitly cancels work. Tokio's child `kill_on_drop` defaults to false, and even enabling it is not proof that descendants terminate. Use explicit termination, reaping, and process-tree containment. [Tokio process behavior](https://docs.rs/tokio/latest/tokio/process/struct.Command.html#method.kill_on_drop).

If per-job termination cannot be proved, mark the job `checking` and the computer unavailable for new mutations. Keep capacity charged. Whole-VM force-stop is allowed for owner computer stop or bot deletion, or for a sole remaining job holder after its deadline. Do not stop unrelated active work to implement a single job's cancellation. Stop timeout alone never releases capacity. A forced kill can lose unflushed guest writes.

## Idempotency and uncertain effects

DESIGNED. Database uniqueness prevents duplicate accepted requests. It does not make a remote command or provider message exactly once. Persist intent before dispatch and acknowledgment after dispatch. A crash between them leaves an uncertain attempt.

For VM start and stop, inspect actual identity and state before retrying toward the saved desired state. Reuse a bot's stable name, verify any known runtime ID, and never use `.replace()` during ordinary startup. For guest commands, save the native job ID as soon as possible. Recover by that ID when available. A native dispatch acknowledged before the API saves its ID is an unresolved correlation gap unless the pinned runtime supports a reliable request marker. Do not invent support for a client-selected idempotency key.

For an uncorrelated dispatch, list retained jobs with all pages if the pinned version supports it. If correlation is still uncertain, retain the lease, report unknown execution, and do not resubmit. Keep observing until the deadline, cancellation, or explicit owner resolution. A guest-side request journal can address this later if the gap proves frequent. It is unnecessary for the first conservative design.

A provider process lost after message delivery also has uncertain acceptance. Inspect native session history through the adapter when supported. Otherwise end that attempt as interrupted with uncertain delivery, preserve recorded tool outcomes, and rebuild a fresh native session before another explicit continuation. Never automatically replay its original action request. All tool request retries use the same recorded IDs.

Cancelled or interrupted actions can already have changed a website or sent data. Query an external action's supported status when available. Otherwise retain the unknown result and require an explicit decision before repeating it. The lifecycle cannot guarantee exactly-once browser actions, ordinary shell commands, or arbitrary network effects. This follows the accepted [uncertain handoff rule](../providers.md#simplified-direction-for-elsebot).

## Crash recovery, stop, and deletion

DESIGNED. Acquire the singleton process lock before reading or changing runtime state. Reconcile before admitting new work. A stale callback cannot replace this pass.

```text
startup():
	load bot intents, unfinished turns and jobs, leases, and process references
	inspect named runtime environments without starting them
	count all observed workers and unfinished transitions against capacity
	for each bot:
		find and terminate obsolete provider process trees before a new attempt
		recover jobs by retained runtime ID, or mark dispatch uncertainty
		reconcile leases with jobs and desktop connections
		if deleting: continue cancellation and shutdown, retain all saved data
		if deleted: preserve tombstone and saved data, refuse new work or recreation
		if active and computer_desired == stopped:
			enforce stopped computer, grant no guest lease, permit host turns
		if active and computer_desired == on_demand:
			observe in-flight start or stop before granting new guest work
	resume safe queued work only after this pass
```

Do not assume an orphan process is dead because its PID is absent or reused. Compare host boot ID and process start identity or an owned cgroup. A host reboot means old host processes are gone. It does not establish whether a previous external side effect completed. An unavailable database prevents new dispatch. Existing guest jobs remain subject to their runtime-enforced deadlines while the API recovers.

If a known job has exited, collect its retained result and release its lease. If its VM stopped or the host rebooted without a retained result, finish the job as interrupted or unknown without replay. If a VM is alive but unreachable, keep capacity charged and retry observation with bounded backoff. Report the bot unavailable. This condition can require owner intervention rather than guessing that resources are free.

Never start an owner-stopped computer to inspect jobs or configure VNC. Its active bot can still start host provider turns. For a computer never materialized, create its environment on permitted first use. For a previously recorded environment that is missing, preserve records and report a restore requirement instead of silently creating an empty replacement. Unexpected environments are charged while inspected but are not automatically removed. The ordinary startup path never prunes storage.

Computer stop is a durable intent, not a one-shot signal. It takes priority over a VM start already in flight. If that start completes late, grant no lease and stop the observed worker. Confirm computer shutdown only after guest work, desktop sessions, and the VM worker are gone. Host provider processes may remain active for conversation work. Explicit resume changes the desired computer state to `on_demand` without starting compute until new demand exists. The incremented computer generation rejects guest work queued before the stop.

Deletion first commits `deleting`, hides the bot from ordinary messaging, and revokes access. It then converges on zero provider processes, zero guest jobs, closed streams, released leases, and a stopped VM. Drain any in-flight start before deletion completes. Verify the exact runtime identity before shutdown. An identity mismatch is an error, never a reason to operate on a same-name replacement. A late result or callback can update internal shutdown evidence but cannot append to retained conversations or recreate compute.

Only after shutdown succeeds does the record become the `deleted` tombstone. A crash at any step resumes shutdown. Failed shutdown remains `deleting` with an actionable error. Retain chats, memory, the stopped VM's saved filesystem, and bot-specific native session files. Preserve shared provider-account authentication and other bots' files. Data removal belongs to a later explicit purge. Existing backups retain their normal retention policy. This supports first-release soft deletion without adding a purge UI or a general operations dashboard.

## Checks before implementation claims

VERIFIED in this track: `python3 docs/experiments/lifecycle/check.py` passes using only the Python standard library. It enumerates ordered events and restarts through JSON snapshots. It checks data retention through soft deletion, host-turn eligibility with an owner-stopped computer, and explicit resume rejecting old queued guest work. It detects both naive lease expiry that abandons live work and premature budget release after stop timeout. The model contains a perfect fake runtime and does not exercise PostgreSQL, real timers, processes, or provider protocols.

UNVERIFIED integration gates remain small and specific:

1. Pin the Rust SDK and runtime to v0.7.13. Prove detached create, reconnect without unwanted startup, retained job inspection and pagination, cancellation, and bounded logs across API death. Method signatures in current web docs need checking against that version.
2. Kill the API between job acceptance and native ID persistence. Prove reliable correlation or the conservative unknown-state behavior. A missing acknowledgment must not rerun a synthetic side effect.
3. Test actual PostgreSQL transactions with concurrent acquisition, idle stop, cancellation, and soft deletion. Repeat API startup with owner-stopped computers and deleting bots. Confirm retained data remains intact and stopped computers do not prevent host chat.
4. Prove host provider descendant containment, subscription session resume, and protocol interruption using the provider track's adapters. Measure host CPU and peak RAM separately from guest resources.
5. Show resource admission holding reservations through start, pause, failed stop, and unreachable-worker states. Verify shutdown leaves no provider descendants and does not destroy persistent VM files.

No real VM or provider was started for these checks. Only the new report and `docs/experiments/lifecycle/` belong to this track.
