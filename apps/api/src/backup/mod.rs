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
    /// Live work prevents a consistent capture now; retried later.
    #[error("deferred: {0}")]
    Deferred(String),
    /// A failed restore left something that may still run; the caller must
    /// not delete the new installation's state.
    #[error("{0}")]
    Leftover(String),
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
    /// A hint for retention only (the manifest is authoritative): the set
    /// recovers the whole assistant.
    #[serde(default)]
    pub whole_assistant: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// `postgres`, `vm` (one bot's standalone disk archive with image), or
    /// `file` (a host artifact a bot fetched, `file-<artifact id>.age`).
    pub kind: String,
    pub object: String,
    /// For a `vm` artifact, its bot.
    pub bot_id: Option<uuid::Uuid>,
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
    /// SHA-256 over the table's sorted row hashes.
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotEntry {
    pub id: uuid::Uuid,
    pub name: String,
    pub status: String,
    /// `included`, `none` (never used a computer), `missing` (gone from the
    /// runtime), `omitted-deleted-bot`, or `omitted` (a database-only set).
    pub vm: String,
    pub computer: Option<ComputerEntry>,
    /// Native provider sessions are not in this set; conversation context
    /// is rebuilt from PostgreSQL and providers need a fresh login check.
    pub provider_state: String,
    /// The native sessions the bot had (not included), for the record.
    pub provider_sessions: Vec<SessionEntry>,
}

/// A bot computer captured as a standalone disk archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComputerEntry {
    pub object: String,
    pub runtime_name: String,
    pub runtime_version: String,
    pub image: String,
    pub cpus: u32,
    pub memory_mib: u32,
    /// The owner's intent (`auto` or `stopped`), restored as recorded.
    pub desired_state: String,
    /// Network profile; restore applies its policy explicitly.
    pub network: String,
    pub desktop_port: Option<u16>,
    /// The desktop's VNC password (the guest holds the same in its disk).
    pub desktop_password: Option<String>,
    /// It was running before the backup stopped it for capture.
    pub was_running: bool,
    /// For a desktop: `ok` when its browser was closed cleanly before the
    /// disk was captured.
    pub browser_checkpoint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionEntry {
    pub provider: String,
    pub native_session_id: String,
    pub status: String,
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
/// encoded as a standard X25519 identity. Every intermediate copy of the
/// derived key is cleared.
pub fn identity(key: &SecretKey, salt: &[u8; 32]) -> Result<age::x25519::Identity, BackupError> {
    use zeroize::{Zeroize, Zeroizing};
    let mut seed = Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<sha2_010::Sha256>::new(Some(salt), key.bytes())
        .expand(PURPOSE.as_bytes(), seed.as_mut())
        .map_err(|_| BackupError::Crypto("HKDF output length rejected".into()))?;
    let mut groups = seed.to_base32();
    let mut encoded = Zeroizing::new(String::new());
    let written = bech32::encode_to_fmt(
        &mut *encoded,
        "age-secret-key-",
        &groups,
        bech32::Variant::Bech32,
    );
    let zero = bech32::u5::try_from_u8(0).expect("0 is a valid u5");
    for group in groups.iter_mut() {
        // SAFETY: a valid, aligned element of the vector.
        unsafe { std::ptr::write_volatile(group, zero) };
    }
    groups.clear();
    match written {
        Ok(Ok(())) => {}
        _ => return Err(BackupError::Crypto("encoding the identity failed".into())),
    }
    let mut upper = Zeroizing::new(encoded.to_uppercase());
    let parsed = upper.parse::<age::x25519::Identity>();
    upper.zeroize();
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
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ");
    let mut suffix = [0u8; 4];
    getrandom::fill(&mut suffix).expect("random source");
    format!("{now}-{}", hex::encode(suffix))
}

/// Created first in every set and restore staging directory: only
/// directories carrying it are ever removed.
pub const MARKER: &str = ".else-backup";

/// A real directory (not a symlink) under `dir` that this application made.
fn owned_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
        && fs::symlink_metadata(path.join(MARKER)).is_ok_and(|m| m.is_file())
}

pub fn mark_owned(dir: &Path) -> Result<(), BackupError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(MARKER))
        .map(|_| ())
        .map_err(|e| BackupError::io(dir.display(), e))
}

/// Completed sets under `dir`, oldest first by completion time (then name).
/// Only real, owned directories with a regular `complete.json` count.
pub fn completed_sets(dir: &Path) -> Result<Vec<String>, BackupError> {
    let mut sets = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(BackupError::io(dir.display(), err)),
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let path = dir.join(&name);
        if !valid_set_id(&name) || !owned_dir(&path) {
            continue;
        }
        if let Ok(meta) = fs::symlink_metadata(path.join(COMPLETE))
            && meta.is_file()
        {
            let at = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            sets.push((at, name));
        }
    }
    sets.sort();
    Ok(sets.into_iter().map(|(_, name)| name).collect())
}

/// Remove staging directories and sets that never completed (an earlier
/// run was interrupted). Only directories this application created are
/// touched, never completed sets or anything else in the directory. The
/// caller holds the backup lock.
pub fn clean_incomplete(dir: &Path) -> Result<usize, BackupError> {
    let mut removed = 0;
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(0);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if !owned_dir(&path) {
            continue;
        }
        let staging = name.starts_with(STAGING_PREFIX);
        let unfinished = valid_set_id(&name) && fs::symlink_metadata(path.join(COMPLETE)).is_err();
        if staging || unfinished {
            fs::remove_dir_all(&path).map_err(|e| BackupError::io(path.display(), e))?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Keep the newest `keep` completed sets (at least one), always `protect`
/// (the set just published), and always the newest whole-assistant set. The completion record goes first, so
/// a half-removed set is never taken for a backup. The caller holds the
/// backup lock.
pub fn apply_retention(dir: &Path, keep: usize, protect: &str) -> Result<Vec<String>, BackupError> {
    let mut sets = completed_sets(dir)?;
    // The newest whole-assistant set is never replaced by lower coverage.
    let newest_whole = sets
        .iter()
        .rev()
        .find(|name| {
            read_regular(&dir.join(name).join(COMPLETE), MAX_COMPLETE_BYTES)
                .ok()
                .and_then(|b| serde_json::from_slice::<Completion>(&b).ok())
                .is_some_and(|c| c.whole_assistant)
        })
        .cloned();
    sets.retain(|s| s != protect && Some(s) != newest_whole.as_ref());
    let keep = keep.max(1) - 1;
    let mut removed = Vec::new();
    if sets.len() <= keep {
        return Ok(removed);
    }
    let excess = sets.len() - keep;
    for name in &sets[..excess] {
        let path = dir.join(name);
        if !owned_dir(&path) {
            continue;
        }
        fs::remove_file(path.join(COMPLETE)).map_err(|e| BackupError::io(path.display(), e))?;
        sync_dir(&path)?;
        fs::remove_dir_all(&path).map_err(|e| BackupError::io(path.display(), e))?;
        removed.push(name.clone());
    }
    sync_dir(dir)?;
    Ok(removed)
}

/// An exclusive, non-blocking lock on `<dir>/.lock`, released when dropped
/// or when the process dies (so neither a crash nor a cancelled future can
/// leave it held). `None` when another holder has it.
pub struct DirLock(#[allow(dead_code)] File);

pub fn try_lock(dir: &Path) -> Result<Option<DirLock>, BackupError> {
    use std::os::fd::AsRawFd;
    ensure_private_dir(dir)?;
    let path = dir.join(".lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| BackupError::io(path.display(), e))?;
    // SAFETY: a valid descriptor owned by `file`.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(BackupError::io(path.display(), err))
        };
    }
    Ok(Some(DirLock(file)))
}

/// Run a tool with its output drained continuously and only bounded tails
/// kept, so large diagnostics cannot exhaust memory.
pub fn run_bounded(
    mut command: std::process::Command,
) -> Result<(std::process::ExitStatus, String, String), BackupError> {
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| BackupError::Producer(e.to_string()))?;
    let out = drain_tail(child.stdout.take().expect("piped"));
    let err = drain_tail(child.stderr.take().expect("piped"));
    let status = child
        .wait()
        .map_err(|e| BackupError::Producer(e.to_string()))?;
    Ok((
        status,
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    ))
}

/// Read a stream to its end on a thread, keeping the last 8 KiB.
pub fn drain_tail(mut stream: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut tail = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stream.read(&mut buf) {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > 16 * 1024 {
                tail.drain(..tail.len() - 8 * 1024);
            }
        }
        String::from_utf8_lossy(&tail).trim().to_owned()
    })
}

pub fn staging_path(dir: &Path, set_id: &str) -> PathBuf {
    dir.join(format!("{STAGING_PREFIX}{set_id}"))
}

/// Connection settings for the backup tools. The connection itself is
/// SQLx's own parse of `DATABASE_URL` (host, port, user, database, query
/// overrides, sockets, TLS mode and certificates), so backups reach exactly
/// the database the API uses; libpq's environment for `pg_dump` and
/// `pg_restore` is derived from it. The password goes only into that
/// environment, never onto a command line.
#[derive(Clone)]
pub struct PgParams {
    pub options: sqlx::postgres::PgConnectOptions,
    password: Option<String>,
    /// TLS files named in the URL (`sslrootcert`, `sslcert`, `sslkey`).
    tls_files: Vec<(&'static str, String)>,
}

impl std::fmt::Debug for PgParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PgParams({}@{}:{}/{})",
            self.user(),
            self.host(),
            self.port(),
            self.database()
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
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl PgParams {
    pub fn parse(url: &str) -> Result<Self, BackupError> {
        let options: sqlx::postgres::PgConnectOptions = url.parse().map_err(|_| {
            BackupError::Invalid("DATABASE_URL is not a valid PostgreSQL URL".into())
        })?;
        let rest = url
            .strip_prefix("postgresql://")
            .or_else(|| url.strip_prefix("postgres://"))
            .unwrap_or_default();
        let (main, query) = rest.split_once('?').unwrap_or((rest, ""));
        let authority = main.split('/').next().unwrap_or_default();
        let mut password = authority
            .rsplit_once('@')
            .and_then(|(userinfo, _)| userinfo.split_once(':'))
            .map(|(_, p)| percent_decode(p));
        let mut tls_files = Vec::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let value = percent_decode(v);
            match k {
                "password" => password = Some(value),
                "sslrootcert" => tls_files.push(("PGSSLROOTCERT", value)),
                "sslcert" => tls_files.push(("PGSSLCERT", value)),
                "sslkey" => tls_files.push(("PGSSLKEY", value)),
                _ => {}
            }
        }
        Ok(Self {
            options,
            password,
            tls_files,
        })
    }

    pub fn host(&self) -> String {
        match self.options.get_socket() {
            Some(socket) => socket.display().to_string(),
            // libpq takes an IPv6 address without the URL's brackets.
            None => {
                let host = self.options.get_host();
                host.strip_prefix('[')
                    .and_then(|h| h.strip_suffix(']'))
                    .unwrap_or(host)
                    .to_owned()
            }
        }
    }

    pub fn port(&self) -> u16 {
        self.options.get_port()
    }

    pub fn user(&self) -> &str {
        self.options.get_username()
    }

    pub fn database(&self) -> &str {
        self.options.get_database().unwrap_or_default()
    }

    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    pub fn with_database(&self, database: &str) -> Self {
        Self {
            options: self.options.clone().database(database),
            ..self.clone()
        }
    }

    fn sslmode(&self) -> &'static str {
        use sqlx::postgres::PgSslMode;
        match self.options.get_ssl_mode() {
            PgSslMode::Disable => "disable",
            PgSslMode::Allow => "allow",
            PgSslMode::Prefer => "prefer",
            PgSslMode::Require => "require",
            PgSslMode::VerifyCa => "verify-ca",
            PgSslMode::VerifyFull => "verify-full",
        }
    }

    /// Environment for libpq tools (the inherited environment is cleared).
    pub fn env(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![
            ("PGHOST", self.host()),
            ("PGPORT", self.port().to_string()),
            ("PGUSER", self.user().to_owned()),
            ("PGDATABASE", self.database().to_owned()),
            ("PGSSLMODE", self.sslmode().to_owned()),
            ("PGCONNECT_TIMEOUT", "10".into()),
        ];
        if let Some(password) = &self.password {
            env.push(("PGPASSWORD", password.clone()));
        }
        env.extend(self.tls_files.iter().cloned());
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
        let env = |url: &str| -> std::collections::HashMap<&'static str, String> {
            PgParams::parse(url).unwrap().env().into_iter().collect()
        };
        let e = env(
            "postgresql://else:p%40ss@127.0.0.1:5433/else_db?sslmode=verify-full&sslrootcert=/ca.pem",
        );
        assert_eq!(e["PGUSER"], "else");
        assert_eq!(e["PGPASSWORD"], "p@ss");
        assert_eq!(e["PGHOST"], "127.0.0.1");
        assert_eq!(e["PGPORT"], "5433");
        assert_eq!(e["PGDATABASE"], "else_db");
        assert_eq!(e["PGSSLMODE"], "verify-full");
        assert_eq!(e["PGSSLROOTCERT"], "/ca.pem");
        assert!(
            !format!("{:?}", PgParams::parse("postgres://u:secret@h/db").unwrap())
                .contains("secret")
        );
        let v6 = env("postgres://u@[::1]/db");
        assert_eq!(
            (v6["PGHOST"].as_str(), v6["PGPORT"].as_str()),
            ("::1", "5432")
        );
        assert!(!v6.contains_key("PGPASSWORD"));
        // Query overrides win, as they do for the API's own connection.
        let q = env("postgres://u@h/db?host=other&port=6000&user=v&dbname=x&password=pw");
        assert_eq!(
            (
                q["PGHOST"].as_str(),
                q["PGPORT"].as_str(),
                q["PGUSER"].as_str(),
                q["PGDATABASE"].as_str()
            ),
            ("other", "6000", "v", "x")
        );
        assert_eq!(q["PGPASSWORD"], "pw");
    }

    #[test]
    fn set_ids_are_plain_names() {
        assert!(valid_set_id(&new_set_id()));
        for bad in ["", ".staging-x", "../x", "a/b", "a b"] {
            assert!(!valid_set_id(bad), "{bad}");
        }
    }
}
