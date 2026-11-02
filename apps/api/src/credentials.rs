//! Installation credentials: the owner bearer token and the `SECRET_KEY`
//! encryption master key.
//!
//! The two are independent. The owner token authenticates the desktop and
//! trusted owner tooling. `SECRET_KEY` encrypts recoverable secrets and
//! backups; it authenticates nothing. Future runtime task grants are a third,
//! separate credential issued per provider task. Neither credential below is
//! ever passed to provider processes, written to logs, or printed.
//!
//! Explicit `setup` creates missing credential files once with
//! `create_new` and mode `0600`. Ordinary startup only reads and validates
//! them, and nothing here replaces an existing file.

use std::fmt;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

const KEY_BYTES: usize = 32;
/// Credential files are tiny; anything larger is not one of ours.
const MAX_FILE_BYTES: u64 = 4096;

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("{what} file {path} does not exist; run `else-api setup` to provision it")]
    Missing { what: &'static str, path: PathBuf },
    #[error("{what} file {path} is unsafe: {reason}")]
    Unsafe {
        what: &'static str,
        path: PathBuf,
        reason: String,
    },
    #[error("{what} in {source_name} is malformed: {reason}")]
    Malformed {
        what: &'static str,
        source_name: String,
        reason: &'static str,
    },
    #[error("{what} at {path}: {error}")]
    Io {
        what: &'static str,
        path: PathBuf,
        error: std::io::Error,
    },
    #[error("the system random source failed: {0}")]
    Random(String),
}

/// The SHA-256 digest of the provisioned owner token. The plaintext is
/// dropped after loading; requests are compared digest to digest.
#[derive(Clone)]
pub struct OwnerToken {
    digest: [u8; 32],
}

impl fmt::Debug for OwnerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OwnerToken(<redacted>)")
    }
}

impl OwnerToken {
    /// Parse the textual token: 32 random bytes as unpadded base64url.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
        if bytes.len() != KEY_BYTES {
            return None;
        }
        Some(Self {
            digest: Sha256::digest(text.as_bytes()).into(),
        })
    }

    /// Constant-time comparison of a presented token against this one.
    pub fn matches(&self, presented: &str) -> bool {
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        bool::from(self.digest.ct_eq(&digest))
    }
}

/// The installation master key. Backups derive their keys from it
/// (purpose-separated, never used raw); it is never logged or exported.
pub struct SecretKey {
    bytes: [u8; KEY_BYTES],
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

impl SecretKey {
    /// Input key material for purpose-separated derivations.
    pub(crate) fn bytes(&self) -> &[u8; KEY_BYTES] {
        &self.bytes
    }

    /// Parse standard base64 encoding 32 bytes.
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = STANDARD.decode(text).ok()?;
        Some(Self {
            bytes: bytes.try_into().ok()?,
        })
    }
}

/// Where the master key comes from. The `SECRET_KEY` environment variable
/// wins; otherwise a private file under the data directory is used.
#[derive(Clone)]
pub enum SecretKeySource {
    Env(String),
    File(PathBuf),
}

impl fmt::Debug for SecretKeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(_) => f.write_str("Env(<redacted>)"),
            Self::File(path) => f.debug_tuple("File").field(path).finish(),
        }
    }
}

fn random_key() -> Result<[u8; KEY_BYTES], CredentialError> {
    let mut bytes = [0u8; KEY_BYTES];
    getrandom::fill(&mut bytes).map_err(|err| CredentialError::Random(err.to_string()))?;
    Ok(bytes)
}

/// Result of provisioning one credential during setup.
#[derive(Debug, PartialEq, Eq)]
pub enum Provisioned {
    Created,
    Existing,
    FromEnvironment,
}

/// Create the private data directory if missing and verify that it is not
/// writable by other users.
pub fn ensure_private_dir(path: &Path) -> Result<(), CredentialError> {
    let io = |error| CredentialError::Io {
        what: "data directory",
        path: path.to_owned(),
        error,
    };
    if fs::symlink_metadata(path).is_err() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(io)?;
    }
    check_dir(path, "data directory")
}

fn check_dir(path: &Path, what: &'static str) -> Result<(), CredentialError> {
    let meta = fs::metadata(path).map_err(|error| CredentialError::Io {
        what,
        path: path.to_owned(),
        error,
    })?;
    let unsafe_dir = |reason: &str| CredentialError::Unsafe {
        what,
        path: path.to_owned(),
        reason: reason.to_owned(),
    };
    if !meta.is_dir() {
        return Err(unsafe_dir("not a directory"));
    }
    if meta.uid() != current_uid() {
        return Err(unsafe_dir("owned by another user"));
    }
    if meta.permissions().mode() & 0o022 != 0 {
        return Err(unsafe_dir("writable by group or other users"));
    }
    Ok(())
}

pub fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Provision the owner token file once. An existing file is validated and
/// preserved, never replaced.
pub fn provision_owner_token(path: &Path) -> Result<Provisioned, CredentialError> {
    const WHAT: &str = "owner token";
    if fs::symlink_metadata(path).is_ok() {
        load_owner_token(path)?;
        return Ok(Provisioned::Existing);
    }
    let token = URL_SAFE_NO_PAD.encode(random_key()?);
    create_private_file(path, WHAT, token.as_bytes())?;
    Ok(Provisioned::Created)
}

/// Provision the master key file once, unless `SECRET_KEY` is supplied by the
/// environment, in which case it is only validated.
pub fn provision_secret_key(source: &SecretKeySource) -> Result<Provisioned, CredentialError> {
    const WHAT: &str = "secret key";
    match source {
        SecretKeySource::Env(_) => {
            load_secret_key(source)?;
            Ok(Provisioned::FromEnvironment)
        }
        SecretKeySource::File(path) => {
            if fs::symlink_metadata(path).is_ok() {
                load_secret_key(source)?;
                return Ok(Provisioned::Existing);
            }
            let key = STANDARD.encode(random_key()?);
            create_private_file(path, WHAT, key.as_bytes())?;
            Ok(Provisioned::Created)
        }
    }
}

fn create_private_file(
    path: &Path,
    what: &'static str,
    contents: &[u8],
) -> Result<(), CredentialError> {
    let io = |error| CredentialError::Io {
        what,
        path: path.to_owned(),
        error,
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        check_dir(parent, "credential directory")?;
    }
    // `create_new` refuses any existing path, including a symlink, so a
    // concurrent setup cannot be tricked into overwriting another file.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io)?;
    // The umask can only remove bits, but make the intent explicit.
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(io)?;
    file.write_all(contents).map_err(io)?;
    file.write_all(b"\n").map_err(io)?;
    file.sync_all().map_err(io)?;
    Ok(())
}

fn read_private_file(path: &Path, what: &'static str) -> Result<String, CredentialError> {
    let unsafe_file = |reason: &str| CredentialError::Unsafe {
        what,
        path: path.to_owned(),
        reason: reason.to_owned(),
    };
    let io = |error| CredentialError::Io {
        what,
        path: path.to_owned(),
        error,
    };
    // A replaceable parent directory would let another user swap the file.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        check_dir(parent, "credential directory")?;
    }
    // Open once without following a final symlink, then validate the opened
    // descriptor itself, so the checked file is the file that is read.
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(CredentialError::Missing {
                what,
                path: path.to_owned(),
            });
        }
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {
            return Err(unsafe_file("is a symbolic link"));
        }
        Err(error) => return Err(io(error)),
    };
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err(unsafe_file("not a regular file"));
    }
    if meta.uid() != current_uid() {
        return Err(unsafe_file("owned by another user"));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(unsafe_file(
            "readable or writable by group or other users; expected mode 0600",
        ));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(unsafe_file("too large to be a credential file"));
    }
    let mut text = String::new();
    file.take(MAX_FILE_BYTES)
        .read_to_string(&mut text)
        .map_err(io)?;
    Ok(text)
}

/// A single trailing newline is tolerated; any other whitespace is malformed.
fn strip_newline(text: &str) -> &str {
    text.strip_suffix('\n').unwrap_or(text)
}

pub fn load_owner_token(path: &Path) -> Result<OwnerToken, CredentialError> {
    const WHAT: &str = "owner token";
    let text = read_private_file(path, WHAT)?;
    OwnerToken::parse(strip_newline(&text)).ok_or_else(|| CredentialError::Malformed {
        what: WHAT,
        source_name: path.display().to_string(),
        reason: "expected 32 bytes encoded as unpadded base64url",
    })
}

pub fn load_secret_key(source: &SecretKeySource) -> Result<SecretKey, CredentialError> {
    const WHAT: &str = "SECRET_KEY";
    let (text, source_name) = match source {
        SecretKeySource::Env(value) => (
            value.clone(),
            "the SECRET_KEY environment variable".to_owned(),
        ),
        SecretKeySource::File(path) => (
            read_private_file(path, "secret key")?,
            path.display().to_string(),
        ),
    };
    SecretKey::parse(strip_newline(&text)).ok_or(CredentialError::Malformed {
        what: WHAT,
        source_name,
        reason: "expected 32 bytes encoded as standard base64",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn provisions_once_and_preserves() {
        let dir = private_dir();
        let path = dir.path().join("owner-token");
        assert_eq!(provision_owner_token(&path).unwrap(), Provisioned::Created);
        let first = fs::read(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(provision_owner_token(&path).unwrap(), Provisioned::Existing);
        assert_eq!(fs::read(&path).unwrap(), first);
        let token = load_owner_token(&path).unwrap();
        assert!(token.matches(std::str::from_utf8(&first).unwrap().trim_end()));
        assert!(!token.matches("not-the-token"));
    }

    #[test]
    fn missing_file_points_to_setup() {
        let dir = private_dir();
        let err = load_owner_token(&dir.path().join("absent")).unwrap_err();
        assert!(err.to_string().contains("else-api setup"), "{err}");
    }

    #[test]
    fn malformed_existing_file_is_not_replaced() {
        let dir = private_dir();
        let path = dir.path().join("owner-token");
        create_private_file(&path, "owner token", b"short").unwrap();
        assert!(matches!(
            provision_owner_token(&path).unwrap_err(),
            CredentialError::Malformed { .. }
        ));
        assert_eq!(fs::read(&path).unwrap(), b"short\n");
    }

    #[test]
    fn rejects_shared_permissions_and_symlinks() {
        let dir = private_dir();
        let path = dir.path().join("owner-token");
        provision_owner_token(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            load_owner_token(&path).unwrap_err(),
            CredentialError::Unsafe { .. }
        ));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(matches!(
            load_owner_token(&link).unwrap_err(),
            CredentialError::Unsafe { .. }
        ));
        // A symlinked parent directory that others can write is refused.
        let shared = tempfile::tempdir().unwrap();
        fs::set_permissions(shared.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let moved = shared.path().join("owner-token");
        fs::copy(&path, &moved).unwrap();
        fs::set_permissions(&moved, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            load_owner_token(&moved).unwrap_err(),
            CredentialError::Unsafe { .. }
        ));
        // Setup refuses to write through a dangling symlink as well.
        let dangling = dir.path().join("dangling");
        std::os::unix::fs::symlink(dir.path().join("target"), &dangling).unwrap();
        assert!(provision_owner_token(&dangling).is_err());
        assert!(!dir.path().join("target").exists());
    }

    #[test]
    fn secret_key_validates_env_and_file() {
        let good = STANDARD.encode([7u8; 32]);
        assert!(load_secret_key(&SecretKeySource::Env(good)).is_ok());
        for bad in ["", "abc", &STANDARD.encode([7u8; 16])] {
            let err = load_secret_key(&SecretKeySource::Env(bad.to_owned())).unwrap_err();
            assert!(
                !err.to_string().contains(bad) || bad.is_empty(),
                "error echoed the value"
            );
        }
        let dir = private_dir();
        let source = SecretKeySource::File(dir.path().join("secret-key"));
        assert_eq!(provision_secret_key(&source).unwrap(), Provisioned::Created);
        assert_eq!(
            provision_secret_key(&source).unwrap(),
            Provisioned::Existing
        );
        load_secret_key(&source).unwrap();
    }

    #[test]
    fn owner_token_and_secret_key_are_independent() {
        let dir = private_dir();
        let token_path = dir.path().join("owner-token");
        let key_source = SecretKeySource::File(dir.path().join("secret-key"));
        provision_owner_token(&token_path).unwrap();
        provision_secret_key(&key_source).unwrap();
        let token_text = fs::read_to_string(&token_path).unwrap();
        let key_text = fs::read_to_string(dir.path().join("secret-key")).unwrap();
        assert_ne!(token_text, key_text);
        // The master key is not a valid owner credential.
        let token = load_owner_token(&token_path).unwrap();
        assert!(!token.matches(key_text.trim_end()));
    }
}
