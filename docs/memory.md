# Memory and storage

PostgreSQL is the database choice. The OptMem-inspired memory design still needs detailed design and validation. The research below was recorded on October 25, 2026.

## Memory design

Learning from the owner's usage over time is one of the main requirements. The assistant needs persistent memory, a way to improve that memory as it learns, and efficient search.

The memory system should take inspiration from [OptMem](https://github.com/VictorTaelin/OptMem), adapted to store memories in PostgreSQL. Every bot, including the main assistant, should have its own persistent memory storage within PostgreSQL.

OptMem keeps an append-only log of original memories and a rebuildable tree of summaries. Agents can read a compact overview, search the original memories, and expand summaries to recover details. These are the ideas to explore for else.bot's database-backed memory system.

Bots should access each other's memories through skills described in their prompts. Those skills should let a bot read another bot's memory in PostgreSQL while keeping each bot's memory storage separate.

The database comparison records the reasoning and the tradeoffs against SQLite.

Adapting OptMem's approach to PostgreSQL is part of the technical design work. Prefer deterministic operations wherever possible, with configuration settings that let us tune how the system produces its output. The LLM can connect the pieces when needed.

For memory, code should handle storing original entries, tracking the summary tree, selecting entries for retrieval, and assembling context according to configuration. Use the LLM where interpretation or summarization is needed. The detailed design should define these responsibilities and the settings that control their behavior.

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
