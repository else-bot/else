# Per-bot memory from the pinned UniiChat design

Research date: 2026-11-02. This is a proposed implementation design, with database evidence separated below. No application code or existing design documents changed.

Use one ordered PostgreSQL log per bot, immutable binary summary nodes, a saved view, and deterministic navigation through source ranges. Keep native Codex and Claude sessions on the host. Memory operations go through the Rust API and scoped MCP tools. They require neither a running bot VM nor database credentials inside a provider process.

## Evidence and scope

The labels in this report have specific meanings:

- VERIFIED means the linked source was fetched or the named disposable experiment passed.
- DESIGNED means a proposed rule for else.bot. It has not been implemented in the application.
- UNVERIFIED means evidence is still missing, including all claims about model recall and provider caching.

VERIFIED: The exact requested reference is [Victor Taelin's gist](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449), pinned to [revision `3c190e06f34aba0c69f49042c526093269604935`](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449/3c190e06f34aba0c69f49042c526093269604935). Its complete file list contains one file, `optchat.md`, titled *UniiChat: one chat that never ends*. The revision was committed on 2026-10-08. I fetched the revision API response and every raw file, checked for truncation, and matched the file's declared size and API content against its raw bytes. GitHub documents [revision reads and truncated-file handling](https://docs.github.com/en/rest/gists/gists#get-a-gist-revision).

The file is 19,092 bytes, with SHA-256 `12f300f760af82bc07bc5201051d1267824ded09c9def8186e4f8144368038d8`. The revision API returns a raw URL containing content revision `4c09901baa3685852bf252cdb70ace81d01d6be5`. This is recorded separately from the gist revision rather than silently treating them as the same identifier. [Fetch manifest](../experiments/memory/source.json).

Existing accepted constraints come from [memory and storage](../memory.md), [provider continuity](../providers.md), [architecture](../architecture.md), and [security](../security.md). PostgreSQL, the Rust API, GPUI, host subscription authentication, and native provider sessions remain accepted. The first UI remains messaging with multiple bots and bot creation and deletion. This design adds no memory editor, email, notes, kanban, routines, mobile work, embedding service, or orchestration framework.

## What the exact reference changes

VERIFIED source description: The pinned [UniiChat design](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449/3c190e06f34aba0c69f49042c526093269604935) uses these mechanisms:

| Mechanism | Reference behavior | else.bot decision, DESIGNED |
| --- | --- | --- |
| Original memory | Append messages, replies, tool calls, results, and reports in one chat log. Do not log private reasoning. | One stream per bot, including the main bot. Keep source conversation, turn, and tool references. |
| Summary tree | A leaf covers one message. Each parent covers two aligned adjacent children. Short inputs copy or join without a model. Target 512 UTF-8 bytes per node. | Preserve the binary tree and short-input fast path. Count bytes in code. |
| View | Ordered nodes cover the history. Save the actual view across restarts. | Persist nodes, covered sequence, merge state, and revision together. Rebuilding nodes does not authorize rebuilding the live view. |
| Merge order | Merge available sibling pairs by greatest age measured from their last message, divided by child span. Break ties by oldest range. | Use exact integer comparisons or rational arithmetic. Do not use the range's first message. |
| Merge timing | Above 128,000 bytes, merge toward 64,000 bytes. Continue later if required parents are unavailable. | Preserve the batched policy. Include rendered labels and separators in the budget. |
| Retrieval | Read the view, then zoom into children and finally originals. The reference forbids manual memory search. | Make zoom primary. Add bounded PostgreSQL text search because existing else.bot docs explicitly require it. |
| Turn continuity | Start every user turn in a fresh model session. | Keep accepted native sessions. Inject or retrieve memory at session start, recovery, handoff, and explicit refresh. |
| Trust | The prompt favors the latest mention. Compaction treats messages as data. | Newer text does not establish truth or grant permission. Keep source attribution and explicit correction links. |

The existing [memory document](../memory.md) names the older OptMem repository and describes an append-only log with a rebuildable tree. It does not specify the persisted view or this merge schedule. The tree is rebuildable. The live view is saved state, because recomputing it can select different nodes and change the prompt prefix. The gist explicitly distinguishes its approach from OptMem's earlier view refitting.

The gist also describes a second, smaller compaction view, ready queues, a maximum of eight simultaneous model calls, bounded retries, and provider-specific cache placement. else.bot can preserve the storage mechanisms without adopting its fresh-session policy or its cache claims. Subscription CLIs control prompt construction and caching themselves. UNVERIFIED: the gist's simulated cache hit rates, relative costs, chosen model effort, and stated wait times apply to else.bot. No direct paid model API is required by this design.

## Four tables are enough

DESIGNED: Use the existing bot ID as the ownership key. The [executable fixture schema](../experiments/memory/schema.sql) demonstrates the main constraints, but it is research SQL, not an application migration.

| Table | Required application fields |
| --- | --- |
| `memory_store` | `bot_id`, transactional `next_seq`, active `generation`, `published_seq`, `view_revision`, ordered `view_nodes`, and `compacting`. Add JSON for the versioned recipe and the saved compaction view with its watermark and merge state. The application checks the authoritative bot tombstone. The standalone fixture models it as `deleted_at` here. |
| `memory_entry` | Composite key `bot_id, seq`; unique bot-scoped request key; kind; original body; SHA-256; timestamp; source reference; authority; optional same-bot `supersedes_seq`. For split messages, retain original item ID, part index, part count, and original content hash. |
| `memory_node` | Composite key `bot_id, generation, start_seq, span`; summary body; byte count or computed `octet_length`; method; input hash; audit JSON containing recipe, prompt, provider and model identity, source IDs, context-view revision, and attempt information. |
| `memory_job` | Same node key; availability time; attempt count; opaque lease token and deadline. Retain bounded last-error information. Completed node audit records the accepted attempt. |

Represent a node as `start+span`, using zero-based entry sequences. Require a positive power-of-two span, aligned start, and a bounded integer range. `0+8` has children `0+4` and `4+4`. `start+1` resolves to its raw entry. Parent coverage is structural and does not depend on generated prose.

Composite foreign keys keep a correction inside its bot. Checks enforce alignment and byte limits. Cross-row rules, including both children existing in the same generation, belong in the API transaction. PostgreSQL warns against using `CHECK` expressions as a substitute for constraints over other rows. [PostgreSQL constraints](https://www.postgresql.org/docs/18/ddl-constraints.html).

Use a generated `tsvector` with the explicit `simple` configuration and a GIN index. Start with this predictable multilingual baseline and make language configuration a versioned choice. It provides lexical matching, not semantic recall. [PostgreSQL text search controls](https://www.postgresql.org/docs/18/textsearch-controls.html).

VERIFIED implementation detail: PostgreSQL rejected `sha256(convert_to(body, 'UTF8'))` as a generated-column expression because the expression is not immutable. The fixture supplies the hash on insert and checks it against the body. Do not copy the rejected generated-column form into a migration.

## Append, summarize, and publish independently

DESIGNED: Append in a short transaction. Lock the bot's `memory_store` row with `FOR NO KEY UPDATE`, check the request key and full request fingerprint, allocate `next_seq`, insert the entry and its initial job, and commit. A retry returns the existing sequence only if its payload and metadata match. A rollback consumes no sequence. This serializes a bot's log without serializing different bots or holding a lock during a provider call. PostgreSQL row locks last until transaction end. [Row locking](https://www.postgresql.org/docs/18/explicit-locking.html).

VERIFIED: The stronger `FOR UPDATE` lock blocked appends while an open rebuild transaction inserted nodes referencing the same store row. `FOR NO KEY UPDATE` preserves append serialization while allowing those foreign-key key-share locks. The bot key stays immutable. Use this lock for store state updates that do not change the key, including view publication.

Save normalized conversation events and their memory records in the same API transaction. Do not introduce a message bus or reread native session files to reconstruct memory. Stable provider event IDs become bot-scoped request keys. Log final assistant text and observable tool activity, not private reasoning.

The compactor is one loop inside the trusted Rust service:

1. Claim available jobs in fixed bot, start, and span order using `FOR UPDATE SKIP LOCKED`. Commit the lease before calling a provider. A queue claim can skip locked rows, so it does not establish publication order. [PostgreSQL queue locking](https://www.postgresql.org/docs/18/sql-select.html#SQL-FOR-UPDATE-SHARE).
2. If the labeled input fits, save it unchanged. Join two child lines with a newline if the complete result fits. Otherwise, request a summary only when a host provider adapter has a verified, enforceable no-tools profile. Source text is data. The summary cannot change permissions.
3. For interpretation, supply the saved smaller compaction view ending at the leaf or parent range's final entry. Never supply future entries. Stop at the first unavailable leaf. Record this exact context and its revision. Context may resolve references but must not introduce claims absent from the summarized source.
4. Validate the result, byte count, source recipe, current generation, input hash, and lease token. Insert the immutable node and retire the job in one transaction. A stale worker cannot publish after its lease has been replaced. Insert the parent job only after both children exist. A unique job key handles duplicate enqueue attempts.
5. Publish newly ready leaves only through the contiguous sequence after `published_seq`. Out-of-order completions wait. Append those leaf IDs to the saved view, then perform any due batch. Save the view and watermark atomically under the store-row lock.

Start with one in-flight summary per installation. The configurable upper bound is eight, matching the reference. At most seven earlier missing leaves may precede a newly started leaf job. This preserves the reference's bounded gap without starting a scheduler service. Persist readiness as jobs rather than rescanning all nodes on every append. Reconstruct missing ready jobs in bounded sequence pages only during explicit repair.

The compaction view has its own 16,000 to 32,000 byte sawtooth. Append new ready lines in order. Merge it when it crosses its upper threshold or the main view batches. All mutations use saved IDs and an explicit watermark. Building a summary never requires rewriting an original entry.

If the summary worker is unavailable or its no-tools profile cannot be enforced, retain all originals and leave model jobs blocked with an explicit reason. Continue exact copying, exact joins that fit, navigation through already built nodes, and paged traversal or lexical search of original entries. Return the covered prefix and pending ranges. Do not call an unrestricted provider or claim that a complete summary view exists. Tool-free model compaction has not been proven by this experiment.

## Deterministic view selection

DESIGNED: Let `T` be the number of entries in the view's covered prefix. For siblings with left start `s` and child span `n`, their final message is `s + 2*n - 1`. Rank candidates by:

```text
due = (T - (s + 2*n - 1)) / n
order: due descending, s ascending
```

Only adjacent siblings with an already built parent in the active generation qualify. Compare fractions by cross multiplication in a wide integer type. Append first. If serialized UTF-8 size exceeds the high threshold, enter compaction mode and merge until size is at most the low threshold. If no eligible parent exists, retain coverage, keep compaction mode set, and resume when nodes arrive. A normal append cannot split old nodes.

VERIFIED: At `T=10`, view `0+4,4+4,8+1,9+1` merges the newest pair `8+1,9+1`. The fixture's last-based ranking also matches an independent translation of the reference's rollback push for all 20,001 tested history lengths. This is an algorithm check, not evidence that summaries retain facts. [Experiment runner and results](../experiments/memory/run.py).

## Retrieval, sources, and corrections

DESIGNED: Expose four small tool operations through the scoped Rust API:

| Operation | Result |
| --- | --- |
| `memory.view` | Bot, generation, revision, covered and pending ranges, and the ordered bounded view. |
| `memory.zoom(start, span)` | Both children, or a paged original for span one, with source IDs, timestamps on request, authority, correction links, and truncation markers. |
| `memory.search(query, cursor)` | Bounded original-entry hits under a fixed text configuration. Order by rank descending, sequence descending, then source ID. Return a watermark-bound cursor. |
| `memory.date(seq)` | Source timestamp and original event reference. |

Bot identity comes from the scoped tool session. A requested target bot requires an explicit cross-bot read grant. The initial owner configuration can grant those reads without accounts, organizations, or an enterprise authorization system. Cross-bot retrieval returns the originating bot and entry IDs. It does not copy authority, grant write access, or expose SQL credentials. Provider sessions and VMs never receive direct PostgreSQL access.

Before relying on an old summary for a fact or consequential action, zoom to the original. Search is a recovery option when the view omits the relevant term. Summaries cannot establish that an action completed or that an instruction came from the owner. The API returns provenance outside generated text. A mixed-source parent is derived data even if its text claims otherwise. Preserve the original source kind at leaves and derive range provenance from originals rather than asking a model to classify trust.

Keep superseded originals. An explicit accepted correction links to its earlier owner assertion through `supersedes_seq`. Current-assertion retrieval excludes an entry only when such a valid link exists, and returns the correction chain when zoomed. External and model-authored entries cannot create authoritative owner corrections. Ordinary natural-language corrections remain original owner messages; a model may suggest a relationship, but that suggestion is derived until tied to an explicit owner correction. Do not claim deterministic fact extraction from arbitrary prose.

No separate fact table or automatic conflict-resolution model is needed for the first version. An unrelated newer web result does not supersede an owner's preference. Where no explicit link exists, return competing statements and their sources. These rules preserve the accepted [memory trust requirements](../security.md#prompt-injection-protections).

MVP bot deletion sets a soft tombstone on the bot record. Exclude the bot from active messaging and reject new turns, appends, and job claims. Fence already running jobs against that tombstone and stop its execution through the normal lifecycle path. Retain every original, summary, queued-job record, VM disk, artifact, and host native session file. Existing memory-read grants stop returning the inactive bot by default. Physical deletion belongs to a later explicit purge operation. The fixture's cascade test demonstrates isolation for that future purge only, never app-facing delete. Filesystem retention is DESIGNED and is not tested by this SQL fixture.

## Budgets and provider-session behavior

DESIGNED starting values are configurable per installation, with the effective recipe recorded per generation:

| Setting | Initial value and rule |
| --- | --- |
| Node target | 512 UTF-8 bytes, including kind labels in node text. |
| Main view | 64,000 byte low threshold and 128,000 byte high threshold, including range labels, separators, and wrapper. |
| Compaction view | 16,000 and 32,000 bytes, with the same serialization rules. |
| Summary repair | At most five total output attempts per claim. Reuse the same input and show the measured overage. Persist bounded backoff. |
| Invalid or unavailable summary | Keep originals and blocked jobs. Use deterministic copies, joins, and traversal. Never install half a source message as a summary. No silent string truncation. |
| Concurrency | One summary in flight initially, configurable up to eight. |
| Retrieval | Twenty lexical hits, at most 16,000 serialized bytes per response, and four automatic zoom calls per provider turn. Further retrieval stays explicit and bounded. |
| Original text | Split long non-tool text at UTF-8 boundaries into parts of at most 64,000 bytes, retaining grouping metadata. Never silently cut the owner's message. |
| Tool output | Capture a labeled head and tail of up to 30,000 characters, as in the reference. Preserve a full artifact when available within the artifact limit, initially 10 MiB. Record exact omissions when it is unavailable or too large. |
| Turn readiness | Wait at most two seconds for pending summaries. Then return an explicitly partial covered range and bounded raw-tail retrieval references. Do not claim full-history coverage. |
| Provider capacity | Adapter-specific input limit with reserved room for the current message, tool output, and response. Reduce selected memory before exceeding it. A byte budget does not establish a token budget. |

The strict node limit is an explicit adaptation. The reference keeps the shortest attempt and tolerates small overages. else.bot's proposed default keeps oversized output pending instead. This improves predictable budgeting at the cost of possible summary backlog. The referenced 128,000-byte view limit is also a soft storage target while required parents are missing. The provider input limit remains hard. Serve a smaller saved coarse view when possible, otherwise a partial view with retrieval references. Never drop coverage silently.

Native provider sessions retain ongoing task context. At a fresh session or handoff, load the bot instructions, selected saved memory view, and accepted provider handoff context. During an existing session, use explicit memory retrieval and bounded refreshes. Avoid repeatedly injecting unchanged history. This intentionally differs from the reference's fresh-call-per-message policy. Memory does not replace the conversation log, native session, or provider handoff record. Provider caching, input accounting, and summary-only execution controls need adapter verification.

## Restart, repair, and rebuild

DESIGNED: On startup, load the exact saved view and verify that its nodes exist, its ranges form an aligned contiguous prefix, and its recorded generation is available. Acknowledge appends only after database commit. An expired job is reclaimable with a new token. Pending leaves catch up from `published_seq + 1`. Normal startup never re-summarizes accepted nodes or rebuilds the live view.

Distinguish two repairs. A missing node can be restored byte-for-byte from backup with its audit record. Regenerating a lost model summary produces a new result and belongs to an explicit rebuild. If a saved view is corrupt, preserve the bad record, restore from backup when possible, or perform a recorded repair that changes its revision. Do not silently pretend the prompt prefix stayed unchanged.

For a recipe change, snapshot an entry watermark and build a separate generation without updating old nodes. Reuse identical outputs when their input hashes and recipe match. Catch up entries appended after the watermark. Under a short store lock, verify that the new generation has every needed leaf through the current watermark, validate its new view, and atomically switch generation, view, and publication state. If appends moved ahead, release the lock and catch up again. Keep the old generation until no in-flight retrieval references it. This is a small maintenance path inside the API, not a distributed workflow engine.

A rebuilt generation may reproduce stored outputs exactly. Fresh LLM calls need not reproduce their text. Record both the old and new recipe and results. Database snapshots can isolate a consistent read, while the final locked watermark check handles later commits. [PostgreSQL transaction isolation](https://www.postgresql.org/docs/18/transaction-iso.html).

## Actual verification and remaining gaps

VERIFIED: [Rerunnable scripts and SQL](../experiments/memory/README.md) ran against a dedicated official PostgreSQL container with no network and no exposed ports. [Actual results](../experiments/memory/results.json) record server version, image digest, checks, and cleanup. The scripts create a UUID-named container and volume and remove only those resources. No existing containers or database data were used.

The checks cover concurrent appends with a shared retry key, rollback, immutable entry updates, invalid correction authority, same-bot foreign keys, byte and alignment constraints, lexical retrieval, independently claimed jobs, lease replacement, persisted views across a database restart, complete synthetic tree coverage, rebuild catch-up during an observed open copy transaction, missing-leaf publication order, and soft tombstone retention. The hard-delete isolation check is a separate future-purge fixture. The view check exercises the reference's merge order against rollback push. Exact counts and outputs are in the result file.

UNVERIFIED: Recall quality, summary faithfulness, conflicting-fact interpretation, prompt injection resistance, multilingual retrieval quality, subscription rate limits, provider tool disabling, native-session memory refresh, and cache behavior. The synthetic node text deliberately has no semantic meaning. These checks also do not establish crash consistency under host power failure, realistic latency or storage growth, or production role grants.

The next application work can implement the four tables and API operations without another architecture study. Before enabling model compaction, run a small labeled conversation set through each host adapter and inspect zoom-based answers against originals, especially corrections and hostile tool text. Keep those results separate from this database PASS.
