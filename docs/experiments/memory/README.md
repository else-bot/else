# Memory database experiment

This directory contains synthetic research fixtures, not application migrations. The report is [Per-bot memory](../../research/memory.md).

Run from the repository root with Python 3 and access to Docker:

```sh
python docs/experiments/memory/run.py
```

The runner pulls the official PostgreSQL image pinned by digest, creates a unique container and volume, and communicates with `psql` through `docker exec` stdin. The container has `--network none` and no port bindings. Local trust authentication applies only to this disposable database. The runner removes its own container and volume in `finally`. It leaves the pulled image in the local image cache. No existing container, volume, or database is selected.

`MEMORY_POSTGRES_IMAGE` can select another official PostgreSQL image for comparison. The runner records the actual image ID, digest, version, settings, and checks in [results.json](results.json). An interrupted Python process may need manual removal of the exact UUID-named resources recorded by the Docker labels. The script never prunes resources.

[schema.sql](schema.sql) defines four tables and a fixture append helper. The helper hard-codes synthetic source metadata and is not a complete production API. [verify.sql](verify.sql) checks constraints and constructs a complete tree of fabricated 512-byte node text. [run.py](run.py) checks concurrent database connections, persisted view recovery, publication gaps, synthetic rebuild catch-up, soft deletion, and view selection. It independently compares the last-based merge order with rollback push over 20,001 history lengths.

App-facing MVP delete is a soft tombstone. The fixture verifies that entries and jobs remain and new appends fail. A separate hard-delete cascade check models a future explicit purge only. It must not become the MVP delete implementation. VM files and host native session files are outside this fixture and must also remain after soft deletion.

No LLM runs. A SQL PASS says nothing about model recall or summary fidelity. Production grants, complete request fingerprints, node-completion validation, saved compaction-context construction, provider calls, and artifact storage remain application work. Until an enforceable no-tools summary profile is verified, retain originals and use exact joins and source traversal rather than starting an unrestricted summary worker.

The first runs found two fixture problems, both corrected before the recorded PASS. `pg_isready` could see the image's temporary initialization server, so readiness now also waits for the initialization-complete marker. PostgreSQL rejected a generated hash expression containing `convert_to`, so the fixture supplies the hash explicitly and checks it against the body. Failed-run containers and volumes were removed.

A strengthened overlap check also found that `FOR UPDATE` on the store row blocks behind foreign-key key-share locks held by an open node-copy transaction. The append helper now uses `FOR NO KEY UPDATE` because the bot key does not change. The final check observes the copy transaction sleeping both before and after ten appends, proving that the appends completed while that transaction remained open.

[source.json](source.json) pins the exact requested gist, its only file, raw URL, byte count, and SHA-256. The complete public source was fetched and inspected in scratch storage. It is linked rather than copied into the repository.
