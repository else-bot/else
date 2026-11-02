# Memory and storage

PostgreSQL is the database choice. The [November 2 memory research](research/memory.md) defines the per-bot design from the exact requested gist and passes 11 checks on PostgreSQL 18.6. The original log, lexical search, explicit owner corrections, cross-bot read grants, and Codex retrieval over MCP are implemented (POC-013); the deterministic summary tree, saved view, rebuilds, and overview/zoom/date tools are implemented too (POC-014); model summaries follow in POC-015. Model summary quality remains unverified. The database comparison below preserves the October 25 research.

## Memory design

Learning from the owner's usage over time is one of the main requirements. The assistant needs persistent memory, a way to improve that memory as it learns, and efficient search.

Use [Victor Taelin's UniiChat design](https://gist.github.com/VictorTaelin/91837951a5ce5b38f341ec1ba1df6449/3c190e06f34aba0c69f49042c526093269604935), adapted to PostgreSQL, for every bot including the main assistant. This is the exact gist the owner selected, pinned to its inspected revision. It refines the earlier [OptMem](https://github.com/VictorTaelin/OptMem) inspiration with a saved memory view and deterministic merge schedule.

Each bot has one ordered log of original messages and observable tool results, immutable binary summary nodes, a saved view over source ranges, and persisted summary jobs. Four logical tables are enough. The summary tree can be rebuilt explicitly. The live view is saved state and must survive startup unchanged.

Code controls append order, parent eligibility, merge priority, byte budgets, job fencing, provenance, and retrieval. A model only summarizes inputs that cannot be copied or joined within the configured node limit. Use the same host-provider budget as interactive work, with compaction at lower priority. Start with 512-byte nodes, a main view that compacts from above 128,000 bytes toward 64,000 bytes, and one summary request at a time. These are configurable defaults. Byte limits do not establish provider token limits.

Use overview and zoom tools to navigate from summaries to originals. Retain bounded PostgreSQL text search as an explicit else.bot adaptation. The gist omits that manual-search path. Keep accepted native provider sessions rather than adopting the gist's fresh session for every turn. Provider caching claims from the reference have not been verified for subscription CLIs.

Skills access memory through the Rust API with bot and task authorization. Cross-bot reads require configured target grants. Every result retains bot, range, source, and trust information. Preserve original corrections and superseded assertions. Summaries cannot grant authority or prove an external action completed.

If a safe summary-only provider profile is unavailable, keep all originals, exact copies, and joins. Report pending coverage and allow bounded traversal or search. Do not run an unrestricted provider or silently truncate source history to satisfy a budget.

The [detailed report](research/memory.md) includes schema, append and compaction rules, merge ordering, retrieval operations, rebuilds, and test limits. Its [database fixture](experiments/memory/README.md) checks concurrent appends, persisted views, stale-job fencing, rebuild catch-up, and deletion retention in an isolated container. The merge algorithm also matched its independent reference translation across 20,001 history lengths. These results validate mechanics, not recall quality.

## Database research

Research date, 2026-10-25.

The decision is to use PostgreSQL as the primary database for bot memory and the rest of else.bot's data. The planned system already has a central Rust API, and bots, routines, email processing, and memory maintenance can produce overlapping writes. PostgreSQL fits that direction and provides an extension for vector search if we need it.

SQLite is a credible alternative for an installation that prioritizes fewer services. It can run behind an application server, so having desktop and mobile clients does not rule it out. Those clients would talk to the API, which owns the local database file. SQLite explicitly documents this deployment pattern. [SQLite deployment guidance](https://sqlite.org/whentouse.html).

### What matters for else.bot

| Need | PostgreSQL | SQLite |
| --- | --- | --- |
| Installation | A separate database service to install, configure, and maintain. Docker is one deployment option. [Server architecture](https://www.postgresql.org/docs/current/tutorial-arch.html). | An embedded database with no separate database service. This is the simpler option to bundle. [Deployment guidance](https://sqlite.org/whentouse.html). |
| Overlapping bot and background writes | Transactions can write different rows concurrently. Conflicting writes still need coordination. [Locking behavior](https://www.postgresql.org/docs/current/explicit-locking.html). | One writer per database file. WAL mode allows readers alongside the writer, but writes still take turns. [WAL behavior](https://sqlite.org/wal.html). |
| Text search across memories, notes, and email | Built-in full-text search with indexing and ranking. [Text search](https://www.postgresql.org/docs/current/textsearch-intro.html). | FTS5 provides indexed full-text search and ranking. [FTS5](https://sqlite.org/fts5.html). |
| Optional vector search | pgvector supports exact search and approximate indexes such as HNSW and IVFFlat. [pgvector](https://github.com/pgvector/pgvector). | Extensions such as sqlite-vec provide vector search. Its maintainers currently describe it as pre-v1 and warn of breaking changes. [sqlite-vec](https://github.com/asg017/sqlite-vec). |
| Live backups | `pg_dump` creates consistent logical exports during normal use. This fits the chosen backup approach. [pg_dump](https://www.postgresql.org/docs/current/app-pgdump.html). | The online backup API and `VACUUM INTO` can create a consistent snapshot of a running database. Copying the main file alone during WAL activity is not a backup strategy. [Backup API](https://sqlite.org/backup.html), [WAL files](https://sqlite.org/wal.html). |

Multiple bots do not prove that SQLite would be too slow. Short transactions and queued writes may be enough for a personal assistant. The PostgreSQL choice comes from the planned independent background work, optional vector search, and the owner's familiarity with PostgreSQL. We have no workload measurements yet.

The cost is an extra service, resource use, and database upgrades to manage. If a single-process installation becomes the priority and measurements show little write contention, SQLite deserves another look. Supporting both databases from the start would add work without a current requirement.

### Applying the choice to memory

The proposed layout is one else.bot database with bot ownership recorded on each memory entry and summary. Each bot gets its own logical memory store, including the main assistant. Separate memory does not require a separate database server for each bot.

Skills should read and search memory through the Rust API. The API scopes each operation to the requested bot, including reads of another bot's memory. This also lets the same service manage chats, bot definitions, projects, notes, email metadata, and routines.

The OptMem-inspired log and summary tree can use ordinary relational tables in either database. Embeddings are an optional search addition. Start with indexed entries, summary traversal, and text search. If semantic retrieval becomes useful, add pgvector with exact search first. Its approximate indexes trade recall for speed, so enable those only after measuring the need. [pgvector search behavior](https://github.com/pgvector/pgvector#indexing).

Configuration should control retrieval limits, context budgets, summary sizes, and rebuild triggers. Code should apply a fixed ordering and tie-break rules when assembling context. LLM-generated summaries remain variable even when the surrounding storage and retrieval operations are deterministic.

[Prompt and memory security](security.md#prompt-injection-protections) covers source and trust information. [Backup design](backups.md) covers preservation of the database and bot computers.
