# Backup and recovery research

Research date: 2026-11-02. Recommendation: use Rust `age` streaming encryption, standalone PostgreSQL exports, and one backup worker in the Rust API. Establish scheduled local backups first. Add automatic S3 or R2 replication to that same worker before claiming protection against host loss.

This proposal preserves [backups.md](../backups.md), [restore.md](../restore.md), and the [master key requirements](../security.md#master-key-and-encrypted-secrets). It adds no product screens. The first GPUI scope remains bot messaging and bot creation and deletion. Backup scheduling and restore are operator functions.

## Evidence and limits

| Status | Finding |
| --- | --- |
| VERIFIED | The disposable [Rust encryption fixture](../experiments/backups/src/main.rs) compiled with `age = 0.12.1` and Rust 1.98.0. |
| VERIFIED | A 2,097,172-byte synthetic payload encrypted and recovered with the identical SHA-256 hash. Ciphertext size varies with the age header and is recorded per run. |
| VERIFIED | The 17 [local checks](../experiments/backups/results.json) passed key separation, corruption and truncation rejection, completion publication, and restoration beside existing files. |
| VERIFIED | Six [PostgreSQL integration checks](../experiments/backups/postgres-results.json) passed a real custom dump, age encryption and decryption, and restore into a fresh database. Two bots, 37 messages, one setting, Unicode, JSON, and sequence state matched exactly. |
| VERIFIED | A restore over an existing PostgreSQL schema failed without changing its rows or sequences. The isolated Docker container was removed and its absence verified. |
| VERIFIED, existing evidence | Microsandbox v0.7.13 disk, full, and desktop-profile recovery already passed the [VM verification](../experiments/bot-vm-verification.md). Those tests were not repeated. |
| DESIGNED | Coordinated capture, scheduled local backups, remote replication, retention, and application restore described below. |
| UNVERIFIED | Remote uploads, provider-state portability, complete application recovery, startup preservation, power-loss behavior, and hostile-input parser limits. |

The fixtures use generated synthetic keys and files under `/tmp/elsebot-research-backups`. They read no application database, provider login, master key, or storage credentials. They make no storage requests. [Reproduction instructions](../experiments/backups/README.md) and a dependency lockfile accompany them.

The PostgreSQL check uses `postgres@sha256:5c4783a8cc3f0074912164f94dfa1be4e043438f0f707cf968d7af9a0d31a2c8`, which reports PostgreSQL, `pg_dump`, and `pg_restore` 18.6. Its uniquely named container has network mode `none`, no published ports, no bind mounts or volumes, and temporary PostgreSQL data in tmpfs. It uses a 384 MiB memory limit and one CPU. The custom dump is staged in a private temporary directory before the existing Rust fixture encrypts it. This verifies format interoperability and database restore, not streaming producer cancellation or coordinated VM capture.

## Encryption and key separation

DESIGNED: choose binary `age-encryption.org/v1` files through the Rust `age` crate, initially pinned to the tested `0.12.1`. The format encrypts 64 KiB chunks with ChaCha20-Poly1305 and requires an authenticated final chunk. Use the crate's recipient API and streaming reader and writer. Call `StreamWriter::finish()` and propagate its error. Read decrypted artifacts through EOF before accepting them. [Age format specification](https://c2sp.org/age@v1.1.0), [Rust streaming writer](https://docs.rs/age/0.12.1/age/stream/struct.StreamWriter.html).

The crate documents interoperability with the reference age implementation and `rage`. It also labels releases before 1.0 as beta. Pin dependencies and keep the compatibility fixture. CLI interoperability was not tested here. [Rust age library](https://docs.rs/age/0.12.1/age/).

Retain the accepted installation master key. Decode `SECRET_KEY` as exactly 32 random bytes. For each backup set, generate a public 32-byte random salt and derive 32 bytes with HKDF-SHA-256:

```text
IKM  = decoded SECRET_KEY
salt = new per-set random bytes
info = UTF-8 "else.bot/backups/v1"
L    = 32 bytes
```

Encode those bytes as a standard age X25519 identity and let the age library handle key agreement, random file keys, header authentication, nonces, and payload encryption. The fixture uses the standard Bech32 identity encoding and the library's parser. It implements no cipher or age payload format. Every artifact, including the manifest, is a separate age encryption operation with a fresh library-generated file key. [Age X25519 identity format](https://c2sp.org/age@v1.1.0#the-x25519-recipient-type).

Database secret encryption keeps the distinct `else.bot/secrets/v1` purpose. The backup key is neither the raw `SECRET_KEY` nor the database secret key. HKDF's context field separates uses of the same input key. The fixture verifies repeatability and different public identities for different purposes and salts. [HKDF section 3.2](https://www.rfc-editor.org/rfc/rfc5869#section-3.2), [RustCrypto HKDF](https://docs.rs/hkdf/0.12.4/hkdf/).

Only the trusted API and its worker receive this key material. Host provider CLIs, bot VMs, tools, and MCP endpoints do not. Production code must clear temporary key buffers and avoid key values in logs, command arguments, or crash reports. The synthetic fixture is not a secret-memory-hardening test.

The outer completion record includes an opaque master-key version identifier, the KDF version and salt, and the age format. It contains no key bytes. Recovery must match those fields against the decrypted manifest. Reject unknown formats and purposes instead of guessing. A damaged salt or wrong key identifier must cause an error, never key regeneration or state reset.

Keep a recovery copy of `SECRET_KEY`, labeled with its version, outside the server and backup bucket, such as in the owner's existing password manager. Preserve older versions while backups or database secrets still need them. Storage access credentials remain separate and can be reissued. Recovering an age identity alone cannot decrypt database secrets derived from the master. Rotation tooling and recovery-management screens remain deferred.

Age supplies ciphertext integrity, not sender signatures or freshness. Someone who knows a recipient's public key can produce a new valid file for it. Keep bucket writes restricted to the trusted worker. A storage attacker can also delete sets or replay an older complete set. This proposal does not claim to prevent those actions.

## A small versioned backup set

DESIGNED: use a unique set identifier, such as a UTC timestamp plus random suffix. Each set contains separate encrypted objects and a small encrypted JSON manifest. Avoid a custom packed archive. A database dump and `.msb` archive already have their own formats and compression.

The [synthetic manifest example](../experiments/backups/manifest.example.json) establishes the JSON shape. Its database bytes are synthetic, its salt is a placeholder, and its empty bot list is deliberate. It is not a usable PostgreSQL backup. Production version 1 requires these fields:

| Field | Contents |
| --- | --- |
| `schema_version`, `set_id` | Schema version `1` and immutable set identity. |
| `created_at`, `capture_started_at`, `capture_finished_at` | UTC times. Capture time is distinct from eventual upload completion. |
| `encryption` | Age format, opaque master-key `key_id`, `HKDF-SHA-256`, exact purpose label, and 32-byte public salt. |
| `application` | Recoverable application version and database migration version. |
| `postgres` | Server version, `pg_dump` version, custom dump format, and required role and extension names and versions. Do not record connection passwords. |
| `artifacts` | Kind, relative object key, byte counts, and SHA-256 of ciphertext and plaintext for every included artifact. |
| `bots` | Bot ID, recorded owner running or stopped state, disk snapshot and artifact IDs, image digest and required image artifacts, runtime and guest-kernel versions, and resource settings. |
| `bots[].provider_sessions` | Host provider and CLI version, bot-to-native-session mappings, artifact references where supported, and whether fresh login or context reconstruction is required. |
| `coverage` | Explicit inclusion or omission of database, VM, external-volume, and provider state. Required missing artifacts prevent a complete backup. |

For version 1, VM exports are standalone disk archives. Include their OCI images for recovery without the old host. Avoid incremental bases and shared image retention logic initially. If later exports depend on another image or base artifact, list every dependency and preserve it while any retained set needs it. Reuse the existing VM integrity checks, browser shutdown checkpoint, and host-mount inventory in [backups.md](../backups.md#microsandbox-snapshot-findings).

Provider-state artifacts are optional only when the manifest explicitly records the fallback. They contain allowlisted supported files from isolated bot locations, never the owner's home directory or `SECRET_KEY`. Until provider research establishes portability, mark authentication as requiring login and native-session recovery as unverified. PostgreSQL remains the source for conversation history and rebuilding session context. A bot's required VM archive is not optional merely because provider state is optional.

Keep `complete.json` as the last published object. It is an unencrypted bootstrap containing the schema version, set ID, encryption fields, relative encrypted-manifest key, and its ciphertext SHA-256. The recovery manifest itself remains encrypted. Treat all bootstrap fields as untrusted hints until decryption succeeds and the matching inner fields validate. The fixture uses exactly this two-file arrangement.

Production parsing must cap bootstrap and manifest size, validate enum values and counts, and accept only expected relative keys below the selected set prefix. Reject absolute paths, traversal, duplicate destinations, symlinks, unexpected archive entries, and arbitrary fetch URLs. The fixture checks schema rejection and relative-path traversal. It is not a complete hostile-archive validator.

## Capture and automatic local publication

DESIGNED: use one background task inside the Rust API and one backup at a time. No separate queue service or workflow engine is needed. A configurable interval, local directory, retention count, and optional bucket configuration are enough. Start with daily backups and seven completed sets as defaults, then adjust from measured capture size and acceptable data loss. On startup, run a missed backup once rather than replaying every missed interval. Record local and remote success separately.

Capture must establish a common application checkpoint. `pg_dump` has its own consistent database view, but that does not coordinate the database with VM files or host CLI session state. It also omits cluster-wide roles. [PostgreSQL pg_dump](https://www.postgresql.org/docs/18/app-pgdump.html).

The smallest initial capture protocol is:

1. Acquire the existing maintenance access used for VM lifecycle operations. Stop accepting new turns, bot creation and deletion, and other application mutations. Wait for current provider turns and scoped tool calls to finish. On timeout, defer the backup and record the failure. Do not freeze an arbitrary active host CLI process.
2. Commit buffered application state. Quiesce supported provider session writers. Checkpoint guest applications, including closing Chromium as already verified. Preserve every owner's stopped VM state. Do not start a stopped VM to back it up.
3. While application writes remain gated, produce the PostgreSQL export, fixed VM checkpoints, and supported host provider-state copies. Hold the gate through the dump initially. This costs pause time but avoids a premature distributed snapshot mechanism.
4. Resume work after those source copies are fixed. Export immutable VM checkpoints outside the gate only if the pinned SDK guarantees they cannot change. Otherwise finish the export before releasing maintenance access.
5. Finish encryption, check all producer exit statuses, validate artifact inventories and VM integrity, then publish the local set. A failed dump must not become complete even if its partial output forms a valid age file.

Use `pg_dump --format=custom --no-owner --no-acl` for the single application database. Stream stdout into the age writer, compute the plaintext hash as it passes, and drain stderr with a bounded log. Wait for a successful dump exit and successful encryption finalization. On cancellation or encryption failure, terminate and reap the dump process, then release maintenance access. Provision the known application role and extensions separately during restore. Record any departure from the single-owner role model in the manifest. Custom format supports `pg_restore`; dumps do not include global roles. [PostgreSQL dump formats and options](https://www.postgresql.org/docs/18/app-pgdump.html).

Write into a private directory with mode `0700` and files with mode `0600`. Use bounded buffers and one VM export at a time. Reserve disk space for checkpoints, archives, and the encrypted local set. If the SDK requires a plaintext `.msb` export, keep it in that private directory and remove it after successful encryption. Unlinking does not promise forensic erasure on SSDs. Disk encryption is separate from archive encryption.

Before `complete.json` appears, sync artifact files, the encrypted manifest, and their containing directory. Write the completion record to a temporary file, sync it, and publish it without replacing an existing record. Sync the directory again. The fixture uses a same-filesystem hard link to publish a completed record without replacement. These operations demonstrate visibility rules, not survival of an actual power cut. Directory entries require their own `fsync`. [Linux fsync](https://man7.org/linux/man-pages/man2/fsync.2.html).

Readers ignore sets without a completion record. No mutable `latest` pointer is required. List completed sets and validate the selected manifest. Interrupted staging directories can be cleaned on the next run without touching completed sets. Restore always rechecks the artifacts, even when the set was previously marked complete.

## Automatic remote replication remains required

DESIGNED: the next worker step uploads each completed local set to the owner's S3 or R2 bucket under immutable keys. Keep it in the same Rust process. Cloudflare documents the AWS Rust SDK against its S3 endpoint. R2 implements a subset of S3, so pin and test the operations actually used. [Cloudflare Rust example](https://developers.cloudflare.com/r2/examples/aws/aws-sdk-rust/), [R2 API compatibility](https://developers.cloudflare.com/r2/api/s3/api/).

Upload encrypted data artifacts first, then the encrypted manifest. Verify each remote object before uploading the completion record last. For the first implementation, a streamed readback and ciphertext SHA-256 comparison gives a storage-independent check. A later optimization may use explicitly verified provider checksum support. A `HEAD` length or user-supplied hash metadata alone does not prove remote payload bytes. Multipart ETags are not whole-object hashes. [S3 integrity documentation](https://docs.aws.amazon.com/AmazonS3/latest/userguide/checking-object-integrity-upload.html).

Completion is atomic at one object, not across the whole bucket. S3 documents atomic single-key updates and no transaction across keys. R2 documents strong read-after-write consistency. Publish the remote completion record only after all referenced objects pass verification, then read it back. Store only private objects and avoid cached public delivery routes during recovery. [S3 consistency model](https://docs.aws.amazon.com/AmazonS3/latest/userguide/Welcome.html#ConsistencyModel), [R2 consistency model](https://developers.cloudflare.com/r2/reference/consistency/).

On restart, retry local sets without a verified remote completion record. Compare existing object hashes before skipping an upload. Abort abandoned multipart uploads. Retry with bounded delay, and report the last remote completion time and error through operational logs or status. A local success must not clear a remote failure. Until automatic remote backup and a remote restore drill pass, report host-loss recovery as UNVERIFIED.

Retain the last seven complete remote sets initially. Never delete the last complete remote set, or the only local copy still awaiting remote replication. Remove a selected old completion record before deleting its now-unreferenced artifacts. Use the same single worker for capture, replication, and retention to avoid concurrent deletion. Preserve referenced image or base objects if dependency sharing is introduced. A short connection failure must not turn retention into data loss.

## Non-destructive recovery

DESIGNED: recovery is an explicit operator action into a new installation directory, new database, and separate Microsandbox runtime home. Ordinary startup does not trigger a restore or import defaults over saved state.

Recover the matching master-key version first. Fetch a completion record, validate its allowed fields, decrypt the manifest, and compare its inner identity and encryption parameters. Fetch only referenced objects. Check ciphertext hashes and lengths, decrypt each artifact to private staging, consume it through EOF, and check plaintext hashes and lengths. Do not pipe a partly verified plaintext stream directly into the destination database or runtime. Delete failed staging outputs.

Create the destination database from `template0` with the required role and extensions. Restore with `pg_restore --single-transaction --exit-on-error --no-owner --no-acl --dbname=<new-database> <verified-dump>`. Do not use `--clean` or restore over the active database. A single transaction rolls back a failed restore. Dumps execute SQL supplied by the source, so accept only the owner's trusted backups. [PostgreSQL restore behavior](https://www.postgresql.org/docs/18/app-pgrestore.html).

Apply the recorded versioned migrations to that new database. Import verified VM artifacts into a separate runtime home. Restore supported provider-state files only to absent isolated bot locations. Keep bots stopped while testing recovered configuration, conversation rows, resource settings, and secret decryption. Never resume an interrupted external action automatically, since the backup cannot prove whether the external service already applied it.

Publish a staged directory with a no-replace operation. The fixture uses Linux `renameat2` with `RENAME_NOREPLACE` and rejects an existing destination before any writes. A normal rename can replace an existing target. [Linux rename semantics](https://man7.org/linux/man-pages/man2/rename.2.html). Database and runtime cutover cannot be one filesystem transaction, so keep the API stopped until all restored resources validate and explicitly select the new installation paths.

Then start else.bot twice. Both starts must preserve bot IDs, saved values, configuration, and conversation rows without duplicates or resets. Restore owner running or stopped intent only after validation. Reauthenticate providers as needed and rebuild native context from PostgreSQL when session files cannot resume. This whole-application check remains UNVERIFIED until application implementation exists.

## Implementation acceptance

The next implementation can stay small: one capture function, one age streaming helper, one manifest version, one serialized worker, and one explicit restore command. Avoid provider plugins, incremental archive machinery, a recovery database, key-rotation screens, and a separate orchestration service.

The disposable PostgreSQL roundtrip now passes. Completion still requires producer-failure injection, crash-boundary tests, and a two-bot capture with the existing VM checkpoint rules. Follow with automatic bucket replication, an interrupted-upload retry, retention checks, and restoration from remote storage with the old runtime absent. Finish with two application starts against restored data. Local encryption and PostgreSQL restore evidence are ready. The remaining integration checks separate this design from a whole-assistant recovery guarantee.
