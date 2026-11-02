# Reproduce the synthetic backup checks

These fixtures verify local archive encryption, restore staging, and a disposable PostgreSQL roundtrip. They do not implement else.bot or connect to application data, provider accounts, VMs, or storage buckets. Keys are generated for each run. Temporary plaintext and keys are removed when the Python process exits normally.

Requirements are Rust 1.98.0 or a compatible compiler, Python 3.11 or newer, and Linux with libc `renameat2`. Public crates.io access is needed for the first build. The checked-in lockfile pins the tested dependency graph. Build with two jobs to bound competition with other research workers.

From the repository root, run:

```sh
CARGO_HOME=/tmp/elsebot-research-backups/cargo-home \
	CARGO_TARGET_DIR=/tmp/elsebot-research-backups/target \
	CARGO_BUILD_JOBS=2 \
	cargo build --locked --manifest-path docs/experiments/backups/Cargo.toml
python3 docs/experiments/backups/verify.py \
	--binary /tmp/elsebot-research-backups/target/debug/elsebot-backup-research \
	--results docs/experiments/backups/results.json
```

Expect a JSON `PASS` response. Read [results.json](results.json) for the individual assertions. The run also regenerates [manifest.example.json](manifest.example.json), which uses synthetic database bytes and a salt placeholder. Encrypted outputs vary on each run because age generates new file keys.

The [Rust fixture](src/main.rs) derives a backup identity from a synthetic 32-byte master and salt with RustCrypto HKDF-SHA-256. It uses standard Bech32 key encoding and the age library for all encryption. It streams with `io::copy`, finalizes encryption, syncs output files, creates outputs without replacement, and removes partial plaintext on failure.

The [Python fixture](verify.py) validates a completion record and encrypted manifest before restoring artifacts into a separate temporary directory. It publishes with Linux `RENAME_NOREPLACE` and checks existing file hashes after rejected restores. Publication uses a synced temporary completion record and a same-filesystem hard link without replacement.

## Run the disposable PostgreSQL check

The [PostgreSQL fixture](verify_postgres.py) requires Docker socket access and the cached image `postgres@sha256:5c4783a8cc3f0074912164f94dfa1be4e043438f0f707cf968d7af9a0d31a2c8`. It creates one uniquely named container with no network or published port, a 384 MiB memory limit, and one CPU. Database files use tmpfs. No host directory or Docker volume is mounted. A trust login is confined to this synthetic container.

After building the Rust fixture, run:

```sh
python3 docs/experiments/backups/verify_postgres.py \
	--binary /tmp/elsebot-research-backups/target/debug/elsebot-backup-research \
	--results docs/experiments/backups/postgres-results.json
```

Expect six assertions to pass and container cleanup to report `verified`. The fixture saves a PostgreSQL custom dump, encrypts and decrypts it through the same Rust age binary, compares its hash, and restores it into a new database created from `template0`. It compares rows, JSON, Unicode, and sequence state. A foreign-key failure checks restored constraints. A second restore over existing tables must fail and preserve all original rows and sequences. [postgres-results.json](postgres-results.json) records the image, versions, hashes, and cleanup result.

Cleanup removes only the generated container by its exact name, including any container-owned anonymous volumes, and verifies its absence. No volumes are expected because tmpfs covers the image's data volume. If the Python process is killed before cleanup, the recorded or Docker-visible name starts with `elsebot-research-backups-`; remove only that run's exact container name.

These checks do not establish actual power-loss durability, a hostile filesystem race boundary, parser resource limits, age CLI interoperability, streaming producer cancellation, VM and database consistency, or remote upload behavior. The production design and remaining acceptance checks are in [the research report](../../research/backup-and-recovery.md).
