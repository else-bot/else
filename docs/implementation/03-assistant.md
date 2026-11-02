# Memory, delegation, and bot computer tasks

These tasks extend the first usable checkpoint into the [assistant POC](../../TODO.md#assistant-poc). Keep the messaging UI unchanged apart from status and provider choices. Follow the [runbook](README.md).

## POC-013: Original memory log and retrieval

Depends on: POC-012.

Add each bot's immutable original memory log and store metadata in PostgreSQL. Save observable conversation and tool events transactionally with their sources. Expose bounded original-entry retrieval and lexical search through the scoped API. Do not save private reasoning or claim that a summary view exists before POC-014.

Acceptance:

- Real concurrent appends, request retries, and rollbacks preserve bot-local order and original content.
- Results retain bot, source, trust, and explicit correction information. Model or external text cannot create an authoritative owner correction.
- Cross-bot reads require configured target grants. Soft deletion retains memory and revokes ordinary access.
- An actual Codex session retrieves a saved synthetic fact through the API without starting a VM.

References: [Pinned gist adaptation](../research/memory.md), [memory design](../memory.md), [database cases](../experiments/memory/README.md).

## POC-014: Deterministic summary tree and saved views

Depends on: POC-013.

Add immutable binary nodes, summary jobs, generation metadata, and the persisted main and compaction views. Implement exact copies and joins, due-based sibling merges, configured byte budgets, contiguous publication, and stale-worker fencing. Add overview, zoom, and source-date tools.

Acceptance:

- Application tests reproduce the reference merge-order regression and verify complete source-range coverage.
- Concurrent completion, lease replacement, and a missing parent cannot publish stale work or silently drop history.
- Restart loads the exact saved view. An explicit rebuild catches concurrent appends and switches generation without rewriting originals.
- Unavailable model summaries leave jobs pending and originals traversable. Partial coverage and omitted ranges are explicit.
- Tests use the real application storage functions and PostgreSQL, rather than only rerunning the research fixture.

References: [Four-table design and view algorithm](../research/memory.md), [research SQL and comparison](../experiments/memory/README.md).

## POC-015: Model compaction and memory quality

Depends on: POC-014, POC-010, POC-011.

Connect summary jobs to a verified no-tools host provider profile using the saved model settings and disabled native sub-agents. Start with one low-priority summary request sharing the global provider budget. Preserve recipes, input hashes, sources, output sizes, and context boundaries. Retain deterministic fallback when summarization is unavailable.

Acceptance:

- Prove that summary attempts cannot execute native or API tools. A prompt instruction alone does not pass.
- A saved synthetic corpus covers family/work facts, corrections, long tool output, and injected instructions. Inspect actual summaries and zoom-based answers against originals.
- Oversized or failed output never installs truncated source text as a valid summary. Bounded retry and backlog status are observable.
- A newly resumed or replaced provider session receives bounded memory context. Existing sessions avoid repeated unchanged injection.
- Record model settings, observed failures, and quality limits. Do not claim deterministic prose, guaranteed recall, or verified provider cache savings.

References: [Memory budgets and compaction](../research/memory.md), [prompt trust](../prompts.md), [provider context](../providers.md).

## POC-016: Asynchronous bot delegation

Depends on: POC-013.

Compile versioned instructions from owner settings, roles, and the SQL bot registry. Implement main-to-leaf delegation with one hop and at most two children per root task. Return a pending child ID, park and reap the parent attempt, run the child through its own API-managed provider attempt, and queue a continuation with the recorded result. Never use the CLI's native sub-agent tools. Keep child work separate from the owner's direct chat.

Acceptance:

- An actual main bot chooses an appropriate registered bot by role, including a newly created role. Routing is prompt-driven.
- With one provider slot, a real registered bot runs and the parent resumes without polling or deadlock. Each API-managed attempt has a saved model profile, and no native CLI sub-agent is spawned. The root remains working between attempts.
- Retries do not create another child. Self/ancestor loops, depleted budgets, deleted targets, and widened permissions fail in code.
- Child results and recalled hostile text remain data. They cannot grant approval or rewrite instructions.
- Root cancellation and target deletion resolve pending child work and preserve recorded outcomes.

References: [Async delegation](../research/prompts-and-delegation.md#one-hop-delegation), [task grants](../research/authentication-and-permissions.md), [root task lifecycle](../research/execution-lifecycle.md#root-tasks-and-provider-attempts).

## POC-017: On-demand VM lifecycle

Depends on: POC-012, POC-010.

Add a thin Microsandbox adapter using the pinned, verified runtime and matching firmware. Save per-bot runtime identity, desired computer state, configuration, and transition generations. Create computers on permitted first use. Implement renewable leases and global admission with an initial budget of 1 vCPU and 2 GiB per active desktop VM, plus measured host overhead.

Acceptance:

- The application creates and reuses two named computers with different files. Restart and stop preserve their disks.
- Chat and memory work while computers are stopped. Explicit owner stop prevents implicit wake until resume.
- Idle stop releases the worker; pause retains its memory reservation. A lease expiry reconciles live work before releasing capacity.
- Startup inspects without waking, replacing, pruning, or silently recreating a missing recorded environment.
- Provider CLIs, subscription credentials, and API secrets remain on the host. No infrastructure vendor login is required for VM operation.

References: [Sandbox choice](../sandboxes.md), [runtime evidence](../experiments/bot-vm-verification.md), [lifecycle design](../research/execution-lifecycle.md).

## POC-018: Guest tools and desktop environment

Depends on: POC-017.

Implement scoped guest execution, job status/cancellation, file and artifact transfer, and browser/desktop operations through the API tool bridge. Build a repeatable guest image with Chromium, terminal, desktop, VNC, and noVNC. Use supervised guest services. An owner can inspect the guest through authenticated access without adding desktop tabs to GPUI.

Acceptance:

- A host Codex session runs a synthetic command in its assigned guest, retrieves an artifact, and operates a local browser page through tools.
- Job IDs, limits, working directories, and leases are explicit. Background work owns a lease after the provider turn ends.
- Host canaries, other bots' files and networks, runtime administration, private metadata endpoints, and provider credentials remain inaccessible.
- Desktop access requires owner authorization. VNC and browser-debugging endpoints have no unauthenticated public route.
- The image and service startup are reproducible. Stop/start preserves guest files and browser state.

References: [Host-to-VM contract](../providers.md#host-to-vm-tools-and-skills), [isolation](../security.md#isolation-and-remote-access), [verified desktop setup](../experiments/bot-vm-verification.md).

## POC-019: Execution recovery and resource admission

Depends on: POC-016, POC-018.

Exercise the scheduler, process ownership, native guest jobs, leases, generations, and budgets across real failures. Keep host provider capacity independent from VM capacity. Resolve job dispatch correlation conservatively when the pinned runtime cannot prove acceptance.

Acceptance:

- Kill the API around provider spawn, guest acceptance, job-ID persistence, lease renewal, and idle stop. Restart recovers the same work or records uncertainty without repeating its effect.
- Race acquisition against stop and deletion using actual PostgreSQL and runtime calls. A late start cannot revive a stopped or deleted bot.
- Cancelled waits, expired leases, unreachable workers, and stop timeouts retain reservations until live work is reconciled.
- One-slot delegation, background guest work, desktop disconnection, and bot-wide Stop work obey their distinct contracts.
- Measure host CLI and VM resources separately. Admission queues rather than exceeding configured limits. Cleanup leaves no owned orphan process or active test VM.

References: [Lifecycle failure rules](../research/execution-lifecycle.md), [synthetic cases to port](../experiments/lifecycle/check.py), [runtime snapshot limits](../experiments/bot-vm-verification.md#snapshot-and-recovery-constraints).

## POC-020: Claude adapter and provider switching

Depends on: POC-011, POC-018, POC-019.

Add the host Claude adapter behind the same conversation interface. Validate its login, restrictions, structured event correlation, interruption, and native resume. Implement the bounded outgoing checkpoint and inline handoff with deterministic history fallback. Track pending delivery and confirmed history coverage per native session.

Acceptance:

- Through the same desktop conversation, run Codex to Claude to Codex with synthetic task context and correct returning-session deltas.
- Preserve the bot's memory, computer, artifacts, and a running guest job. The receiving provider inspects existing job status rather than launching it again.
- An unavailable source, missing target session, interrupted turn, or uncertain send preserves the owner's follow-up and recorded outcomes without blind replay.
- Summaries have enforced no-tool execution. Use deterministic fallback when that cannot be established or a checkpoint fails.
- Configure an explicit saved Claude model ID before enabling it. Reject an unconfigured or unsupported profile rather than inheriting a floating CLI default.
- Prove Claude native agent spawning is denied, including on resume and handoff. Validate tool inventory, model requests, native agent lifecycle events, and process observations. Do not treat an empty custom-agent registry as disabling built-in sub-agents.
- Prove Claude host restrictions and per-bot session isolation. Existing Codex checks alone do not establish Claude safety.
- Update capabilities and the provider picker using server data. Add no provider-specific desktop workflow.

References: [Lightweight continuity algorithm](../research/conversation-continuity.md), [handoff prompts](../prompts.md#handoff-prompt-drafts), [source and fixture pins](../experiments/continuity/sources.json).
