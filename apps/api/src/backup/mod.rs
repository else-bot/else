//! Encrypted database backups and non-destructive restore (POC-021).
//!
//! A backup set is a directory `<backup dir>/<set id>/` holding separately
//! age-encrypted objects (`database.dump.age`, a PostgreSQL custom dump, and
//! `manifest.json.age`) and, published last, the unencrypted bootstrap
//! `complete.json`. A set without `complete.json` is not a backup. Every set
//! has a fresh public salt; its age identity is derived from `SECRET_KEY`
//! with HKDF-SHA-256 and the purpose `else.bot/backups/v1`, so neither the
//! raw master key nor the database-secret key encrypts anything here. The
//! master key never enters an artifact, a log, or a command line.
//!
//! This task covers the database only, and its sets say so: `coverage`
//! marks bot computers and provider state as omitted and the set as not a
//! whole-assistant backup.

pub mod capture;
pub mod restore;
pub mod worker;

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use bech32::ToBase32;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::credentials::SecretKey;

pub const SCHEMA_VERSION: u32 = 1;
pub const FORMAT: &str = "age-encryption.org/v1";
pub const KDF: &str = "HKDF-SHA-256";
pub const PURPOSE: &str = "else.bot/backups/v1";
/// Derives the opaque, non-secret identifier recorded with each set.
const KEY_ID_PURPOSE: &[u8] = b"else.bot/key-id/v1";
pub const COMPLETE: &str = "complete.json";
pub const MANIFEST: &str = "manifest.json.age";
pub const DATABASE: &str = "database.dump.age";
const STAGING_PREFIX: &str = ".staging-";
/// Bootstrap and manifest size caps: anything larger is not ours.
pub const MAX_COMPLETE_BYTES: u64 = 16 * 1024;
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("{0}")]
    Io(String),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("encryption: {0}")]
    Crypto(String),
    #[error("pg_dump failed: {0}")]
    Producer(String),
    #[error("the dump exceeded the {0}-byte limit")]
    TooLarge(u64),
    #[error("this set was made with key {expected}; the key provided is {provided}")]
    WrongKey { expected: String, provided: String },
    #[error("invalid backup set: {0}")]
    Invalid(String),
    #[error("{0}")]
    Refused(String),
    #[error("interrupted")]
    Interrupted,
}

impl BackupError {
    pub fn io(what: impl std::fmt::Display, err: std::io::Error) -> Self {
        Self::Io(format!("{what}: {err}"))
    }
}

/// Recorded with each set so recovery can select the key; holds no key
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Encryption {
    pub format: String,
    pub key_id: String,
    pub kdf: String,
    pub info: String,
    pub salt_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectRef {
    pub object: String,
    pub ciphertext_bytes: u64,
    pub ciphertext_sha256: String,
}

/// The unencrypted bootstrap, published last. Untrusted until the manifest
/// it names decrypts and matches it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    pub schema_version: u32,
    pub set_id: String,
    pub encryption: Encryption,
    pub manifest: ObjectRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub kind: String,
    pub object: String,
    pub ciphertext_bytes: u64,
    pub ciphertext_sha256: String,
    pub plaintext_bytes: u64,
    pub plaintext_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Application {
    pub version: String,
    /// The newest applied migration.
    pub migration_version: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Postgres {
    pub server_version: String,
    pub dump_version: String,
    pub format: String,
    /// Extensions the restore target needs (name and version).
    pub extensions: Vec<(String, String)>,
}

/// Row count and content digest of one table, taken in the dump's snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableDigest {
    pub table: String,
    pub rows: i64,
    pub md5: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotEntry {
    pub id: uuid::Uuid,
    pub name: String,
    pub status: String,
    /// The bot's computer is not in this set.
    pub vm: String,
    /// Native provider sessions are not in this set; conversation context
    /// is rebuilt from PostgreSQL and providers may need a fresh login.
    pub provider_state: String,
}

/// What a set contains, stated explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub database: String,
    pub vm: String,
    pub provider_state: String,
    /// True only when every state the assistant needs is included.
    pub whole_assistant: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub set_id: String,
    pub created_at: String,
    pub capture_started_at: String,
    pub capture_finished_at: String,
    pub encryption: Encryption,
    pub application: Application,
    pub postgres: Postgres,
    pub artifacts: Vec<Artifact>,
    pub tables: Vec<TableDigest>,
    pub bots: Vec<BotEntry>,
    pub coverage: Coverage,
}

/// The coverage of a database-only set.
pub fn database_only_coverage() -> Coverage {
    Coverage {
        database: "included".into(),
        vm: "omitted".into(),
        provider_state: "omitted-reauth-required".into(),
        whole_assistant: false,
        note: "Database only: bot computers (files, browser profiles, running jobs) and native \
               provider sessions are not included. Restoring this set does not recover the \
               whole assistant."
            .into(),
    }
}

/// The opaque identifier of a master key: a one-way derivation, so it
/// reveals nothing about the key but tells keys apart.
pub fn key_id(key: &SecretKey) -> String {
    let mut out = [0u8; 8];
    hkdf::Hkdf::<sha2_010::Sha256>::new(None, key.bytes())
        .expand(KEY_ID_PURPOSE, &mut out)
        .expect("8 bytes is a valid HKDF length");
    format!("sk1-{}", hex::encode(out))
}

/// The age identity of one set: HKDF-SHA-256(SECRET_KEY, salt, purpose)
/// encoded as a standard X25519 identity.
pub fn identity(key: &SecretKey, salt: &[u8; 32]) -> Result<age::x25519::Identity, BackupError> {
    let mut seed = [0u8; 32];
    hkdf::Hkdf::<sha2_010::Sha256>::new(Some(salt), key.bytes())
        .expand(PURPOSE.as_bytes(), &mut seed)
        .map_err(|_| BackupError::Crypto("HKDF output length rejected".into()))?;
    let encoded = bech32::encode("age-secret-key-", seed.to_base32(), bech32::Variant::Bech32);
    seed.fill(0);
    let mut encoded = encoded.map_err(|e| BackupError::Crypto(e.to_string()))?;
    let mut upper = encoded.to_uppercase();
    let parsed = upper.parse::<age::x25519::Identity>();
    // The encodings are the key; clear them (NUL bytes keep them valid UTF-8).
    for text in [&mut encoded, &mut upper] {
        // SAFETY: overwriting with ASCII NUL keeps the string valid UTF-8.
        unsafe { text.as_bytes_mut().fill(0) };
    }
    parsed.map_err(|e| BackupError::Crypto(e.to_string()))
}

pub fn encryption_for(key: &SecretKey, salt: &[u8; 32]) -> Encryption {
    Encryption {
        format: FORMAT.into(),
        key_id: key_id(key),
        kdf: KDF.into(),
        info: PURPOSE.into(),
        salt_hex: hex::encode(salt),
    }
}

/// Validate recorded encryption fields and derive the set's identity.
pub fn identity_for(
    key: &SecretKey,
    encryption: &Encryption,
) -> Result<age::x25519::Identity, BackupError> {
    if encryption.format != FORMAT || encryption.kdf != KDF || encryption.info != PURPOSE {
        return Err(BackupError::Invalid(format!(
            "unsupported encryption {}/{}/{}",
            encryption.format, encryption.kdf, encryption.info
        )));
    }
    let provided = key_id(key);
    if encryption.key_id != provided {
        return Err(BackupError::WrongKey {
            expected: encryption.key_id.clone(),
            provided,
        });
    }
    let salt: [u8; 32] = hex::decode(&encryption.salt_hex)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| BackupError::Invalid("the salt is not 32 bytes of hex".into()))?;
    identity(key, &salt)
}

/// Counts and hashes bytes written through it.
pub struct Tee<W> {
    pub inner: W,
    hasher: Sha256,
    pub bytes: u64,
}

impl<W> Tee<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    pub fn digest(&self) -> String {
        hex::encode(self.hasher.clone().finalize())
    }
}

impl<W: Write> Write for Tee<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Sizes and hashes of one encrypted object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub plaintext_bytes: u64,
    pub plaintext_sha256: String,
    pub ciphertext_bytes: u64,
    pub ciphertext_sha256: String,
}

/// Encrypt `input` into a new private file (never replacing one), with at
/// most `limit` plaintext bytes, and sync it. The partial file is removed on
/// any failure.
pub fn seal(
    path: &Path,
    identity: &age::x25519::Identity,
    input: &mut dyn Read,
    limit: u64,
) -> Result<Sealed, BackupError> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| BackupError::io(path.display(), e))?;
    let result = (|| {
        let recipient = identity.to_public();
        let encryptor =
            age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
                .map_err(|e| BackupError::Crypto(e.to_string()))?;
        let mut writer = encryptor
            .wrap_output(Tee::new(file))
            .map_err(|e| BackupError::Crypto(e.to_string()))?;
        let mut plain = Tee::new(std::io::sink());
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = input
                .read(&mut buf)
                .map_err(|e| BackupError::Producer(e.to_string()))?;
            if n == 0 {
                break;
            }
            if plain.bytes + n as u64 > limit {
                return Err(BackupError::TooLarge(limit));
            }
            plain.write_all(&buf[..n]).expect("sink");
            writer
                .write_all(&buf[..n])
                .map_err(|e| BackupError::io(path.display(), e))?;
        }
        // The authenticated final chunk: without it the file is invalid.
        let cipher = writer
            .finish()
            .map_err(|e| BackupError::io(path.display(), e))?;
        cipher
            .inner
            .sync_all()
            .map_err(|e| BackupError::io(path.display(), e))?;
        Ok(Sealed {
            plaintext_bytes: plain.bytes,
            plaintext_sha256: plain.digest(),
            ciphertext_bytes: cipher.bytes,
            ciphertext_sha256: cipher.digest(),
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

/// A regular file (not a symlink) no larger than `max`, read whole.
pub fn read_regular(path: &Path, max: u64) -> Result<Vec<u8>, BackupError> {
    let meta = fs::symlink_metadata(path).map_err(|e| BackupError::io(path.display(), e))?;
    if !meta.is_file() {
        return Err(BackupError::Invalid(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > max {
        return Err(BackupError::Invalid(format!(
            "{} is too large",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|f| f.take(max + 1).read_to_end(&mut bytes))
        .map_err(|e| BackupError::io(path.display(), e))?;
    if bytes.len() as u64 > max {
        return Err(BackupError::Invalid(format!(
            "{} is too large",
            path.display()
        )));
    }
    Ok(bytes)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Decrypt a small object whole (the manifest), through its final chunk.
pub fn open_small(
    ciphertext: &[u8],
    identity: &age::x25519::Identity,
    max: u64,
) -> Result<Vec<u8>, BackupError> {
    let decryptor =
        age::Decryptor::new(ciphertext).map_err(|e| BackupError::Crypto(e.to_string()))?;
    let reader = decryptor
        .decrypt(std::iter::once(identity as &dyn age::Identity))
        .map_err(|e| BackupError::Crypto(e.to_string()))?;
    let mut out = Vec::new();
    reader
        .take(max + 1)
        .read_to_end(&mut out)
        .map_err(|e| BackupError::Crypto(e.to_string()))?;
    if out.len() as u64 > max {
        return Err(BackupError::Invalid("the manifest is too large".into()));
    }
    Ok(out)
}

pub fn private_dir(path: &Path) -> Result<(), BackupError> {
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|e| BackupError::io(path.display(), e))
}

pub fn ensure_private_dir(path: &Path) -> Result<(), BackupError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| BackupError::io(path.display(), e))?;
    let meta = fs::symlink_metadata(path).map_err(|e| BackupError::io(path.display(), e))?;
    if !meta.is_dir() {
        return Err(BackupError::Refused(format!(
            "{} is not a directory",
            path.display()
        )));
    }
    Ok(())
}

pub fn sync_dir(path: &Path) -> Result<(), BackupError> {
    File::open(path)
        .and_then(|d| d.sync_all())
        .map_err(|e| BackupError::io(path.display(), e))
}

/// Rename without ever replacing an existing destination.
pub fn rename_noreplace(from: &Path, to: &Path) -> Result<(), BackupError> {
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).expect("no NUL");
    let (a, b) = (c(from), c(to));
    // SAFETY: two valid NUL-terminated paths relative to the cwd.
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc != 0 {
        return Err(BackupError::io(
            to.display(),
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

/// Set IDs are UTC timestamps with a random suffix; anything else under the
/// backup directory is not a set.
pub fn valid_set_id(id: &str) -> bool {
    id.len() <= 64
        && !id.is_empty()
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn new_set_id() -> String {
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let mut suffix = [0u8; 4];
    getrandom::fill(&mut suffix).expect("random source");
    format!("{now}-{}", hex::encode(suffix))
}

/// Completed sets under `dir`, oldest first (set IDs sort by time).
pub fn completed_sets(dir: &Path) -> Result<Vec<String>, BackupError> {
    let mut sets = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(sets),
        Err(err) => return Err(BackupError::io(dir.display(), err)),
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !valid_set_id(&name) {
            continue;
        }
        let complete = dir.join(&name).join(COMPLETE);
        if fs::symlink_metadata(&complete).is_ok_and(|m| m.is_file()) {
            sets.push(name);
        }
    }
    sets.sort();
    Ok(sets)
}

/// Remove staging directories and sets that never completed (an earlier
/// run was interrupted). Completed sets are never touched.
pub fn clean_incomplete(dir: &Path) -> Result<usize, BackupError> {
    let mut removed = 0;
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(0);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let is_dir = fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir());
        let staging = name.starts_with(STAGING_PREFIX);
        let unfinished = valid_set_id(&name) && !path.join(COMPLETE).exists();
        if is_dir && (staging || unfinished) {
            fs::remove_dir_all(&path).map_err(|e| BackupError::io(path.display(), e))?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Keep the newest `keep` completed sets (at least one). The completion
/// record goes first, so a half-removed set is never taken for a backup.
pub fn apply_retention(dir: &Path, keep: usize) -> Result<Vec<String>, BackupError> {
    let sets = completed_sets(dir)?;
    let keep = keep.max(1);
    let mut removed = Vec::new();
    if sets.len() <= keep {
        return Ok(removed);
    }
    for name in &sets[..sets.len() - keep] {
        let path = dir.join(name);
        fs::remove_file(path.join(COMPLETE)).map_err(|e| BackupError::io(path.display(), e))?;
        sync_dir(&path)?;
        fs::remove_dir_all(&path).map_err(|e| BackupError::io(path.display(), e))?;
        removed.push(name.clone());
    }
    sync_dir(dir)?;
    Ok(removed)
}

pub fn staging_path(dir: &Path, set_id: &str) -> PathBuf {
    dir.join(format!("{STAGING_PREFIX}{set_id}"))
}

/// libpq connection settings for `pg_dump` and `pg_restore`, passed in the
/// environment so the password is never on a command line.
#[derive(Clone, PartialEq, Eq)]
pub struct PgParams {
    pub host: String,
    pub port: String,
    pub user: String,
    pub password: Option<String>,
    pub database: String,
    pub sslmode: Option<String>,
}

impl std::fmt::Debug for PgParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PgParams({}@{}:{}/{})",
            self.user, self.host, self.port, self.database
        )
    }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl PgParams {
    /// Parse a `postgres://` or `postgresql://` URL.
    pub fn parse(url: &str) -> Result<Self, BackupError> {
        let rest = url
            .strip_prefix("postgresql://")
            .or_else(|| url.strip_prefix("postgres://"))
            .ok_or_else(|| BackupError::Invalid("DATABASE_URL is not a postgres URL".into()))?;
        let (main, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (authority, database) = main.split_once('/').unwrap_or((main, ""));
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        let (user, password) = match userinfo {
            Some(u) => match u.split_once(':') {
                Some((u, p)) => (percent_decode(u), Some(percent_decode(p))),
                None => (percent_decode(u), None),
            },
            None => (String::new(), None),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (h.to_owned(), p.to_owned()),
            _ => (hostport.to_owned(), "5432".to_owned()),
        };
        let sslmode = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("sslmode=").map(str::to_owned));
        Ok(Self {
            host: host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned(),
            port,
            user,
            password,
            database: percent_decode(database),
            sslmode,
        })
    }

    pub fn with_database(&self, database: &str) -> Self {
        Self {
            database: database.to_owned(),
            ..self.clone()
        }
    }

    /// Environment for libpq tools (the inherited environment is cleared).
    pub fn env(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![
            ("PGHOST", self.host.clone()),
            ("PGPORT", self.port.clone()),
            ("PGUSER", self.user.clone()),
            ("PGDATABASE", self.database.clone()),
            ("PGCONNECT_TIMEOUT", "10".into()),
        ];
        if let Some(password) = &self.password {
            env.push(("PGPASSWORD", password.clone()));
        }
        if let Some(mode) = &self.sslmode {
            env.push(("PGSSLMODE", mode.clone()));
        }
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> SecretKey {
        use base64::Engine;
        SecretKey::parse(&base64::engine::general_purpose::STANDARD.encode([byte; 32])).unwrap()
    }

    #[test]
    fn identities_separate_purpose_salt_and_key() {
        let salt = [7u8; 32];
        let a = identity(&key(1), &salt).unwrap().to_public().to_string();
        assert_eq!(a, identity(&key(1), &salt).unwrap().to_public().to_string());
        assert_ne!(a, identity(&key(2), &salt).unwrap().to_public().to_string());
        assert_ne!(
            a,
            identity(&key(1), &[8u8; 32])
                .unwrap()
                .to_public()
                .to_string()
        );
        assert_ne!(key_id(&key(1)), key_id(&key(2)));
        assert!(key_id(&key(1)).starts_with("sk1-"));
    }

    #[test]
    fn sealed_objects_open_only_with_their_identity_and_whole() {
        let dir = tempfile::tempdir().unwrap();
        let id = identity(&key(1), &[1; 32]).unwrap();
        let path = dir.path().join("x.age");
        let data = vec![42u8; 200_000];
        let sealed = seal(&path, &id, &mut data.as_slice(), 1 << 20).unwrap();
        assert_eq!(sealed.plaintext_bytes, 200_000);
        let bytes = fs::read(&path).unwrap();
        assert_eq!(sealed.ciphertext_sha256, sha256_hex(&bytes));
        assert_eq!(open_small(&bytes, &id, 1 << 20).unwrap(), data);
        let other = identity(&key(2), &[1; 32]).unwrap();
        assert!(open_small(&bytes, &other, 1 << 20).is_err());
        assert!(
            open_small(&bytes[..bytes.len() - 10], &id, 1 << 20).is_err(),
            "truncated"
        );
        let mut flipped = bytes.clone();
        let last = flipped.len() - 100;
        flipped[last] ^= 1;
        assert!(open_small(&flipped, &id, 1 << 20).is_err(), "corrupted");
        // Never replaces, and a too-large input leaves nothing behind.
        assert!(seal(&path, &id, &mut data.as_slice(), 1 << 20).is_err());
        let big = dir.path().join("big.age");
        assert!(matches!(
            seal(&big, &id, &mut data.as_slice(), 1000),
            Err(BackupError::TooLarge(1000))
        ));
        assert!(!big.exists());
    }

    #[test]
    fn urls_become_libpq_settings_without_the_password_on_argv() {
        let p = PgParams::parse("postgresql://else:p%40ss@127.0.0.1:5433/else_db?sslmode=disable")
            .unwrap();
        assert_eq!(
            (
                p.user.as_str(),
                p.password.as_deref(),
                p.host.as_str(),
                p.port.as_str(),
                p.database.as_str()
            ),
            ("else", Some("p@ss"), "127.0.0.1", "5433", "else_db")
        );
        assert_eq!(p.sslmode.as_deref(), Some("disable"));
        assert!(!format!("{p:?}").contains("p@ss"));
        let q = PgParams::parse("postgres://u@localhost/db").unwrap();
        assert_eq!((q.port.as_str(), q.password.as_deref()), ("5432", None));
    }

    #[test]
    fn set_ids_are_plain_names() {
        assert!(valid_set_id(&new_set_id()));
        for bad in ["", ".staging-x", "../x", "a/b", "a b"] {
            assert!(!valid_set_id(bad), "{bad}");
        }
    }
}
