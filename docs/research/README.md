# Research review for the first version

The starting design is one owner's messaging app with a bot list, one visible conversation per bot, and bot creation and deletion. Keep the Rust API, GPUI desktop, PostgreSQL, host-installed Codex and Claude CLIs, and on-demand per-bot Microsandbox computers. The [product requirements](../product.md#first-version) separate this first version from later features.

Research ran on November 2, 2026, through seven GPT 6.1 Sol workers with high reasoning. The agent interface exposed no separate Fast setting. The live Codex prompt checks explicitly requested and confirmed the priority/Fast configuration. Research fixtures are saved under `docs/experiments/`. No application implementation has started.

## Findings and evidence

A source inspection establishes what upstream does. A synthetic model checks proposed rules. A database or provider experiment exercises that component. None alone proves the complete application works.

| Track | Starting design | Evidence and limits |
| --- | --- | --- |
| [Conversation continuity](conversation-continuity.md) | PostgreSQL owns history. Resume separate native sessions. Deliver a bounded checkpoint and missed history inline on a switch. | Current T3 source pinned to `30654b3`; eight synthetic cases and installed CLI shape probes pass. Live cross-provider switching remains unverified. |
| [Memory](memory.md) | Four tables hold each bot's original log, immutable summary tree, saved view, and summary jobs. Use overview, zoom, and bounded text search. | Exact supplied gist revision pinned. Eleven checks pass on PostgreSQL 18.6, including concurrent writes and persisted views. Merge order matches the reference translation across 20,001 history lengths. Summary quality remains unverified. |
| [Prompts and delegation](prompts-and-delegation.md) | Versioned trusted instructions, a bot registry, scoped tools, and one-hop delegation. The main bot can request at most two children per owner task. | Thirteen v1 live synthetic Codex cases, three v2 asynchronous cases, and 17 guard-model checks pass. Tool results are scripted. Real child dispatch, host isolation, and Claude behavior remain unverified. |
| [Authentication and permissions](authentication-and-permissions.md) | One owner token, loopback API, SSH tunnel for a VPS, and transient task-scoped tool grants. Keep development-agent workspace authorization separate. | Thirty-nine policy-model checks and Rust private-file and environment probes pass. Actual API routes and host process containment remain unverified. |
| [Backups and recovery](backup-and-recovery.md) | Rust age encryption, PostgreSQL custom dumps, standalone VM archives, a versioned manifest, and completion published last. Restore beside existing state. | Seventeen encryption and publication checks plus six real PostgreSQL dump and restore checks pass. Coordinated capture, remote replication, and full-application restore remain unverified. |
| [Execution lifecycle](execution-lifecycle.md) | One scheduler and durable PostgreSQL records. Separate provider attempts, guest jobs, VM leases, and resource reservations. | Twenty-eight synthetic interleaving and crash cases plus nine edge checks pass. Existing VM tests provide runtime evidence. SQL races and pinned SDK job recovery remain unverified. |
| [Desktop contracts](desktop-and-feature-contracts.md) | Bot sidebar, plain-text chat, create and delete dialogs, HTTP commands, and one authenticated SSE stream for the selected bot. | Twelve synthetic contract cases pass, including reconnects, UTF-8 frames, and deletion retention. Live GPUI, HTTP, and PostgreSQL replay remain unverified. |

## Shared decisions

Use one API process and one scheduler initially. PostgreSQL provides durable state and queues. Keep provider adapters, memory compaction, VM management, and backup scheduling in that service. No Redis, broker, workflow engine, OAuth server, role editor, or separate agent framework is required.

Provider CLIs live on the host with subscription authentication and isolated bot session directories. Their tools reach the assigned guest through the Rust API. Chat, memory, and application operations do not require a running VM. Provider switch and memory rebuild never replace the bot's computer.

Keep conversation history independent of native provider files and summaries. Stable owner request IDs deduplicate message acceptance. Stable action and job IDs preserve known outcomes. An uncertain external effect blocks automatic replay of that action. A provider acknowledgment alone does not prove the effect completed.

The selected memory reference is the supplied UniiChat gist, not only the earlier OptMem repository. Save the live view across restarts. Rebuild summaries only through an explicit versioned generation. Code controls structure, budgets, publication order, and source information. Models supply summary prose. Keep originals available if model compaction is blocked. Start with one low-priority summary call using the same host-provider budget as interactive work.

The owner API token, provider credentials, task grants, webhook tokens, and `SECRET_KEY` have separate purposes. No runtime provider process receives the owner token, database credentials, or master key. Development agents may work within the owner's assigned repository authorization and disposable development services. That does not grant runtime bots access to the host or production data.

Soft deletion records the bot's tombstone before acknowledging. Revoke access, cancel all its work, and stop its computer. Retain chats, memory, files, and native session state for later explicit purge. Startup never recreates deleted bots. Explicit installation setup creates the initial main bot once. Deleting it clears the saved default without silently appointing a replacement.

Stopping a computer differs from deleting a bot. Host chat and memory still work while that computer is explicitly stopped. Guest tools require explicit resume. Idle shutdown preserves on-demand intent. Pause retains memory, so use stop to reclaim RAM.

Local backups establish the encryption and restore path. Automatic R2 or S3 replication remains required for protection against losing the host. Record local and remote completion separately. Restore fully verified artifacts into a fresh database and separate runtime storage. Keep the original master key recoverable outside the backup set. Recovery-management screens can wait.

## Delegation with one provider slot

A parent provider waiting for a child can occupy the only available provider slot. Avoid that deadlock by making delegation asynchronous from the start.

Accept and persist the child task, then return its pending ID. End the parent's current provider attempt and release its process reservation. Keep the owner's root task working. Run the child through the existing queue. Once the expected children finish, queue a parent continuation with their recorded results. The parent then answers in the original conversation. Guest jobs keep their independent leases throughout.

This needs parent and child task references, a bounded child count, and continuation records. It does not need a general workflow graph. The provider-attempt lifetime is shorter than the root task lifetime. The desktop projects the root task's activity rather than briefly showing idle between attempts. Three additional Codex cases test the asynchronous prompt rule with scripted pending and completed results. The [prompt research](prompts-and-delegation.md) keeps that evidence separate from the 13-case v1 suite. Actual single-slot scheduler behavior remains an implementation check.

## First implementation slice

The research is sufficient to start a small vertical slice. Carry the remaining checks into implementation rather than opening another broad architecture study.

1. Add the bot, conversation, request, and event records. Implement owner authentication, create, list, soft delete, snapshots, and replay with synthetic provider output.
2. Build the single GPUI messaging window against those contracts. Check typing, composition, selection, scrolling, reconnects, and deletion.
3. Connect the host Codex adapter with its tested instruction package and scoped tools. Verify real streaming, interruption, native resume, and denied host access before using sensitive owner data. Add Claude through the same adapter contract and run the actual provider-switch round trip.
4. Add the memory log and deterministic view mechanics. Inspect model summaries and zoom-based answers against a small labeled conversation set before enabling automatic compaction.
5. Add asynchronous delegation, guest job tracking, leases, cancellation, and resource admission through the same scheduler. Exercise crash boundaries against PostgreSQL and the pinned runtime.
6. Wire the verified encryption path into automatic backup capture and explicit restore. Add remote replication, interrupted-upload retry, retention, and a remote restore drill before claiming host-loss recovery.

These steps describe implementation order, not authorization to start coding in this research turn. Email, projects, notes, routines, mobile research, rich transcript rendering, and remote desktop views remain later work.

## Remaining integration checks

The important gaps are concrete: enforce host CLI restrictions while preserving subscription login, run a real Codex to Claude to Codex handoff, test asynchronous parent and child dispatch with one provider slot, inspect summary faithfulness, race actual lifecycle transactions, build the GPUI slice, and restore a coordinated backup from remote storage with the original runtime absent.

Each linked report includes its source pins, fixture commands, failures encountered, and evidence limits. Disposable research databases and volumes were removed. No remote messages, storage uploads, or production-data operations were performed.
