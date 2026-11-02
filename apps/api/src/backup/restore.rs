//! Restore a verified set into a new database.
//!
//! Nothing existing is changed: the target database must not exist, every
//! object is checked against the completion record and the decrypted
//! manifest (lengths and SHA-256 of ciphertext and plaintext, read through
//! the authenticated final chunk) before any SQL runs, and the dump is
//! decrypted into a private staging directory that is removed afterwards.
//! `pg_restore` runs in one transaction; after it, every table's row count
//! and digest must equal the manifest's, migrations are applied, and the
//! restored provider profiles must still pass the adapters' rules (so a
//! restored setting cannot enable native sub-agents). On any failure the
//! database this restore created is dropped. A missing or wrong key is an
//! error, never a reason to regenerate anything.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use sqlx::Connection;
use sqlx::postgres::{PgConnectOptions, PgConnection};

use super::{BackupError, COMPLETE, Completion, DATABASE, MANIFEST, Manifest, PgParams};
use crate::credentials::SecretKey;

#[derive(Debug)]
pub struct RestoreRequest<'a> {
    pub set_dir: &'a Path,
    pub key: &'a SecretKey,
    /// Credentials for the server; the target database name replaces its
    /// database.
    pub server: &'a PgParams,
    pub target_database: &'a str,
    pub pg_restore: &'a Path,
    /// A private directory for decrypted staging files.
    pub staging_parent: &'a Path,
    /// Largest decrypted dump accepted (bounds staging space).
    pub max_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct RestoreReport {
    pub set_id: String,
    pub manifest: Manifest,
    /// The database's newest migration after restore.
    pub migration_version: i64,
}

fn valid_database_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
}

fn options(params: &PgParams) -> PgConnectOptions {
    // SQLx's own parse of the URL: TLS mode, certificates, and overrides
    // apply to every connection the restore makes.
    params.options.clone()
}

/// Remove decrypted staging left by a restore that was killed (the caller
/// holds the staging lock, so no live restore's files are touched).
fn clean_stale(parent: &Path) -> Result<(), BackupError> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ours = entry.file_name().to_string_lossy().starts_with(".restore-")
            && std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir())
            && std::fs::symlink_metadata(path.join(super::MARKER)).is_ok_and(|m| m.is_file());
        if ours {
            std::fs::remove_dir_all(&path).map_err(|e| BackupError::io(path.display(), e))?;
        }
    }
    Ok(())
}

/// Read and validate the bootstrap and the encrypted manifest.
pub fn open_set(
    set_dir: &Path,
    key: &SecretKey,
) -> Result<(Completion, Manifest, age::x25519::Identity), BackupError> {
    let set_id = set_dir
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| super::valid_set_id(n))
        .ok_or_else(|| BackupError::Invalid("the set directory name is not a set ID".into()))?
        .to_owned();
    let bootstrap = super::read_regular(&set_dir.join(COMPLETE), super::MAX_COMPLETE_BYTES)
        .map_err(|err| match err {
            BackupError::Io(_) => BackupError::Invalid("the set has no completion record".into()),
            other => other,
        })?;
    let completion: Completion = serde_json::from_slice(&bootstrap)
        .map_err(|e| BackupError::Invalid(format!("completion record: {e}")))?;
    if completion.schema_version != super::SCHEMA_VERSION {
        return Err(BackupError::Invalid(format!(
            "unsupported schema version {}",
            completion.schema_version
        )));
    }
    if completion.set_id != set_id {
        return Err(BackupError::Invalid(
            "the completion record names another set".into(),
        ));
    }
    if completion.manifest.object != MANIFEST {
        return Err(BackupError::Invalid("unexpected manifest object".into()));
    }
    // A wrong key is recognized before anything is decrypted.
    let identity = super::identity_for(key, &completion.encryption)?;
    let ciphertext = super::read_regular(
        &set_dir.join(MANIFEST),
        super::MAX_MANIFEST_BYTES + 64 * 1024,
    )?;
    if ciphertext.len() as u64 != completion.manifest.ciphertext_bytes
        || super::sha256_hex(&ciphertext) != completion.manifest.ciphertext_sha256
    {
        return Err(BackupError::Invalid(
            "the manifest does not match its completion record".into(),
        ));
    }
    let plain = super::open_small(&ciphertext, &identity, super::MAX_MANIFEST_BYTES)?;
    let manifest: Manifest = serde_json::from_slice(&plain)
        .map_err(|e| BackupError::Invalid(format!("manifest: {e}")))?;
    if manifest.schema_version != completion.schema_version
        || manifest.set_id != completion.set_id
        || manifest.encryption != completion.encryption
    {
        return Err(BackupError::Invalid(
            "the manifest does not match its completion record".into(),
        ));
    }
    match manifest.artifacts.as_slice() {
        [artifact] if artifact.kind == "postgres" && artifact.object == DATABASE => {}
        _ => return Err(BackupError::Invalid("unexpected artifacts".into())),
    }
    Ok((completion, manifest, identity))
}

/// Check the encrypted dump and decrypt it into `target` (created new),
/// verifying length and hash of both forms.
fn verify_and_decrypt(
    set_dir: &Path,
    manifest: &Manifest,
    identity: &age::x25519::Identity,
    target: &Path,
) -> Result<(), BackupError> {
    let artifact = &manifest.artifacts[0];
    let source = set_dir.join(&artifact.object);
    let meta =
        std::fs::symlink_metadata(&source).map_err(|e| BackupError::io(source.display(), e))?;
    if !meta.is_file() || meta.len() != artifact.ciphertext_bytes {
        return Err(BackupError::Invalid(
            "the dump's size does not match the manifest".into(),
        ));
    }
    // Ciphertext first, so a corrupted file is refused before decrypting.
    let mut hasher = super::Tee::new(std::io::sink());
    let mut file =
        std::fs::File::open(&source).map_err(|e| BackupError::io(source.display(), e))?;
    std::io::copy(&mut file, &mut hasher).map_err(|e| BackupError::io(source.display(), e))?;
    if hasher.digest() != artifact.ciphertext_sha256 {
        return Err(BackupError::Invalid(
            "the dump's ciphertext hash does not match".into(),
        ));
    }
    let file = std::fs::File::open(&source).map_err(|e| BackupError::io(source.display(), e))?;
    let decryptor = age::Decryptor::new(std::io::BufReader::new(file))
        .map_err(|e| BackupError::Crypto(e.to_string()))?;
    let mut reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .map_err(|e| BackupError::Crypto(e.to_string()))?;
    let out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(target)
        .map_err(|e| BackupError::io(target.display(), e))?;
    let mut plain = super::Tee::new(out);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| BackupError::Crypto(format!("the dump does not decrypt: {e}")))?;
        if n == 0 {
            break;
        }
        if plain.bytes + n as u64 > artifact.plaintext_bytes {
            return Err(BackupError::Invalid(
                "the dump is longer than recorded".into(),
            ));
        }
        plain
            .write_all(&buf[..n])
            .map_err(|e| BackupError::io(target.display(), e))?;
    }
    plain
        .inner
        .sync_all()
        .map_err(|e| BackupError::io(target.display(), e))?;
    if plain.bytes != artifact.plaintext_bytes || plain.digest() != artifact.plaintext_sha256 {
        return Err(BackupError::Invalid(
            "the decrypted dump does not match the manifest".into(),
        ));
    }
    Ok(())
}

/// Removes the staging directory however the restore ends.
struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub async fn restore(request: RestoreRequest<'_>) -> Result<RestoreReport, BackupError> {
    if !valid_database_name(request.target_database) {
        return Err(BackupError::Refused(
            "the target database name must be lowercase letters, digits, and underscores".into(),
        ));
    }
    let (_, manifest, identity) = open_set(request.set_dir, request.key)?;
    if manifest.artifacts[0].plaintext_bytes > request.max_bytes {
        return Err(BackupError::TooLarge(request.max_bytes));
    }
    // One restore at a time per staging directory; stale staging from a
    // killed restore is removed under the same lock.
    let _lock = super::try_lock(request.staging_parent)?
        .ok_or_else(|| BackupError::Refused("another restore is running".into()))?;
    clean_stale(request.staging_parent)?;
    let staging = Staging(
        request
            .staging_parent
            .join(format!(".restore-{}", uuid::Uuid::new_v4())),
    );
    super::private_dir(&staging.0)?;
    super::mark_owned(&staging.0)?;
    let dump = staging.0.join("database.dump");
    {
        let set_dir = request.set_dir.to_owned();
        let manifest = manifest.clone();
        let identity = identity.clone();
        let dump = dump.clone();
        tokio::task::spawn_blocking(move || {
            verify_and_decrypt(&set_dir, &manifest, &identity, &dump)
        })
        .await
        .map_err(|e| BackupError::Io(e.to_string()))??;
    }

    // The target must be new; an existing database is never touched.
    let admin = request.server.with_database("postgres");
    let mut conn = PgConnection::connect_with(&options(&admin)).await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(request.target_database)
            .fetch_one(&mut conn)
            .await?;
    if exists {
        return Err(BackupError::Refused(format!(
            "database {} already exists; nothing was changed",
            request.target_database
        )));
    }
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE DATABASE \"{}\" TEMPLATE template0 ENCODING 'UTF8'",
        request.target_database
    )))
    .execute(&mut conn)
    .await?;
    let restored = load_and_check(&request, &manifest, &dump).await;
    if restored.is_err() {
        // Only the database created above is dropped.
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            request.target_database
        )))
        .execute(&mut conn)
        .await;
    }
    let migration_version = restored?;
    Ok(RestoreReport {
        set_id: manifest.set_id.clone(),
        manifest,
        migration_version,
    })
}

async fn load_and_check(
    request: &RestoreRequest<'_>,
    manifest: &Manifest,
    dump: &Path,
) -> Result<i64, BackupError> {
    let target = request.server.with_database(request.target_database);
    let pg_restore = request.pg_restore.to_owned();
    let env = target.env();
    let dump = dump.to_owned();
    let database = request.target_database.to_owned();
    let (status, _, stderr) = tokio::task::spawn_blocking(move || {
        let mut command = Command::new(&pg_restore);
        command
            .args([
                "--single-transaction",
                "--exit-on-error",
                "--no-owner",
                "--no-acl",
            ])
            // A database name only; the password stays in the environment.
            .arg(format!("--dbname={database}"))
            .arg(&dump)
            .env_clear()
            .envs(env);
        super::run_bounded(command)
    })
    .await
    .map_err(|e| BackupError::Io(e.to_string()))??;
    if !status.success() {
        return Err(BackupError::Producer(format!(
            "pg_restore exited with {status}: {}",
            stderr.lines().last().unwrap_or_default()
        )));
    }
    let mut conn = PgConnection::connect_with(&options(&target)).await?;
    // Exactly the captured rows, before migrations may change any.
    let digests = super::capture::table_digests(&mut conn).await?;
    if digests != manifest.tables {
        let differing: Vec<String> = manifest
            .tables
            .iter()
            .filter(|t| !digests.contains(t))
            .map(|t| t.table.clone())
            .collect();
        return Err(BackupError::Invalid(format!(
            "restored tables differ from the manifest: {differing:?}"
        )));
    }
    let recorded: i64 =
        sqlx::query_scalar("SELECT coalesce(max(version), 0) FROM _sqlx_migrations WHERE success")
            .fetch_one(&mut conn)
            .await?;
    if recorded != manifest.application.migration_version {
        return Err(BackupError::Invalid(
            "the restored migration state differs".into(),
        ));
    }
    crate::db::MIGRATOR
        .run(&mut conn)
        .await
        .map_err(|e| BackupError::Invalid(format!("migrations: {e}")))?;
    // Restored settings must still satisfy the adapters' rules: no profile
    // can enable native sub-agents or select an unsupported model.
    let profiles: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT provider_id, model, reasoning_effort, service_tier FROM provider_profiles",
    )
    .fetch_all(&mut conn)
    .await?;
    for (provider, model, reasoning_effort, service_tier) in profiles {
        let profile = crate::app::providers::Profile {
            model,
            reasoning_effort,
            service_tier,
        };
        let checked = match provider.as_str() {
            crate::providers::codex::ID => crate::providers::codex::validate(&profile),
            crate::providers::claude::ID => crate::providers::claude::validate(&profile),
            _ => Ok(()),
        };
        if let Err(reason) = checked {
            return Err(BackupError::Invalid(format!(
                "restored {provider} profile is not allowed: {reason}"
            )));
        }
    }
    let version: i64 =
        sqlx::query_scalar("SELECT coalesce(max(version), 0) FROM _sqlx_migrations WHERE success")
            .fetch_one(&mut conn)
            .await?;
    conn.close().await?;
    Ok(version)
}
