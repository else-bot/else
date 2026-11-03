//! Owner-provided credentials for a bot's guest scripts (POC-026).
//!
//! A task secret is a named set of environment variables for one bot, for
//! example the SMTP settings a mailing script needs. Values are encrypted at
//! rest with an age identity derived from `SECRET_KEY` and a per-secret salt
//! (purpose `else.bot/task-secrets/v1`); only the variable names are stored
//! in the clear and shown to the bot. A value reaches the guest only as an
//! environment variable of the one command or job that names the secret,
//! sent over the runtime's stdin, never in a command line, a file, or saved
//! source. Output that would show a value is redacted, by length and
//! SHA-256, before it is returned or stored.

use std::collections::BTreeMap;
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::credentials::SecretKey;

pub const PURPOSE: &str = "else.bot/task-secrets/v1";
pub const MAX_VARS: usize = 16;
pub const MAX_VALUE: usize = 4096;
/// Secrets one command or job may name.
pub const MAX_PER_COMMAND: usize = 4;
/// Values shorter than this are not redacted (they would match ordinary
/// text); the owner is told so when storing one.
pub const MIN_REDACTED: usize = 4;

/// Variables a secret may not set: they change how the guest runs the
/// command rather than giving it data.
const RESERVED: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "PWD", "OLDPWD", "IFS", "ENV", "BASH_ENV",
    "DISPLAY", "TERM", "TMPDIR", "LANG", "PS4",
];

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("{0}")]
    Invalid(String),
    #[error("task secrets need the installation's SECRET_KEY")]
    NoKey,
    #[error("a stored secret could not be decrypted: {0}")]
    Crypto(String),
    #[error("no task secret named {0}")]
    Unknown(String),
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SecretInfo {
    pub name: String,
    pub env: Vec<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// A decrypted secret's variables; cleared when dropped.
#[derive(Default)]
pub struct Env {
    pub vars: Vec<(String, Zeroizing<String>)>,
}

impl Env {
    /// The redaction set for these values: (length, SHA-256), never the
    /// value itself, so it can be stored with a job.
    pub fn redaction(&self) -> Vec<Redact> {
        self.vars
            .iter()
            .filter(|(_, v)| v.len() >= MIN_REDACTED)
            .map(|(name, v)| {
                let mut base = [0u8; 8];
                getrandom::fill(&mut base).expect("the system random source");
                let base = u64::from_le_bytes(base) % (MODULUS - 2) + 2;
                Redact {
                    name: name.clone(),
                    len: v.len(),
                    sha256: hex::encode(Sha256::digest(v.as_bytes())),
                    base,
                    rolling: rolling(v.as_bytes(), base),
                }
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Redact {
    pub name: String,
    pub len: usize,
    pub sha256: String,
    /// A polynomial hash of the value with a random base, so a scan finds
    /// candidate positions in linear time; SHA-256 confirms each.
    pub base: u64,
    pub rolling: u64,
}

const MODULUS: u64 = (1 << 61) - 1;

fn mul(a: u64, b: u64) -> u64 {
    ((a as u128 * b as u128) % MODULUS as u128) as u64
}

fn rolling(bytes: &[u8], base: u64) -> u64 {
    bytes
        .iter()
        .fold(0, |h, &b| (mul(h, base) + b as u64 + 1) % MODULUS)
}

/// Replace every occurrence of a redacted value in `text` with
/// `[secret NAME]`. Matching is by exact bytes (length, then SHA-256), so
/// the value itself is never needed.
pub fn redact(text: &str, set: &[Redact]) -> String {
    let bytes = text.as_bytes();
    // Every confirmed match, then replaced left to right.
    let mut found: Vec<(usize, usize, &str)> = Vec::new();
    for r in set {
        if r.len == 0 || r.len > bytes.len() || r.base < 2 {
            continue;
        }
        // base^(len-1), to drop the leading byte of a window.
        let mut lead = 1u64;
        for _ in 1..r.len {
            lead = mul(lead, r.base);
        }
        let mut h = rolling(&bytes[..r.len], r.base);
        let mut i = 0;
        loop {
            if h == r.rolling && hex::encode(Sha256::digest(&bytes[i..i + r.len])) == r.sha256 {
                found.push((i, r.len, r.name.as_str()));
            }
            if i + r.len >= bytes.len() {
                break;
            }
            let out = (mul(bytes[i] as u64 + 1, lead)) % MODULUS;
            h = (h + MODULUS - out) % MODULUS;
            h = (mul(h, r.base) + bytes[i + r.len] as u64 + 1) % MODULUS;
            i += 1;
        }
    }
    if found.is_empty() {
        return text.to_owned();
    }
    found.sort_by_key(|(start, len, _)| (*start, std::cmp::Reverse(*len)));
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    for (start, len, name) in found {
        if start < at {
            continue;
        }
        out.extend_from_slice(&bytes[at..start]);
        out.extend_from_slice(format!("[secret {name}]").as_bytes());
        at = start + len;
    }
    out.extend_from_slice(&bytes[at..]);
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 40
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
}

fn valid_var(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('A'..='Z' | '_'))
        && name.len() <= 64
        && chars.all(|c| matches!(c, 'A'..='Z' | '0'..='9' | '_'))
        && !RESERVED.contains(&name)
        && !name.starts_with("LD_")
        && !name.starts_with("ELSE_")
}

/// Check a secret's variables before storing it.
pub fn validate(env: &BTreeMap<String, String>) -> Result<(), SecretError> {
    if env.is_empty() || env.len() > MAX_VARS {
        return Err(SecretError::Invalid(format!(
            "a secret has 1 to {MAX_VARS} variables"
        )));
    }
    for (name, value) in env {
        if !valid_var(name) {
            return Err(SecretError::Invalid(format!(
                "{name} is not an allowed variable name (A-Z, 0-9, _; not PATH, HOME, LD_*, ELSE_*, ...)"
            )));
        }
        if value.is_empty() || value.len() > MAX_VALUE || value.contains('\0') {
            return Err(SecretError::Invalid(format!(
                "{name}: a value has 1 to {MAX_VALUE} bytes and no NUL"
            )));
        }
    }
    Ok(())
}

fn identity(key: &SecretKey, salt: &[u8; 32]) -> Result<age::x25519::Identity, SecretError> {
    crate::backup::identity_for_purpose(key, salt, PURPOSE)
        .map_err(|e| SecretError::Crypto(e.to_string()))
}

fn seal(key: &SecretKey, salt: &[u8; 32], plain: &[u8]) -> Result<Vec<u8>, SecretError> {
    let identity = identity(key, salt)?;
    let recipient = identity.to_public();
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(|e| SecretError::Crypto(e.to_string()))?;
    let mut out = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut out)
        .map_err(|e| SecretError::Crypto(e.to_string()))?;
    writer
        .write_all(plain)
        .and_then(|()| writer.finish().map(|_| ()))
        .map_err(|e| SecretError::Crypto(e.to_string()))?;
    Ok(out)
}

fn open(
    key: &SecretKey,
    salt: &[u8; 32],
    cipher: &[u8],
) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    let identity = identity(key, salt)?;
    let decryptor = age::Decryptor::new(cipher).map_err(|e| SecretError::Crypto(e.to_string()))?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|e| SecretError::Crypto(e.to_string()))?;
    let mut out = Zeroizing::new(Vec::new());
    reader
        .read_to_end(&mut out)
        .map_err(|e| SecretError::Crypto(e.to_string()))?;
    Ok(out)
}

/// Store (create or replace) a secret. Returns whether it was new.
pub async fn put(
    pool: &PgPool,
    key: &SecretKey,
    bot_id: Uuid,
    name: &str,
    env: &BTreeMap<String, String>,
) -> Result<bool, SecretError> {
    if !valid_name(name) {
        return Err(SecretError::Invalid(
            "a secret name is 1 to 40 of a-z, 0-9, _ and -, starting with a letter".into(),
        ));
    }
    validate(env)?;
    let mut salt = [0u8; 32];
    getrandom::fill(&mut salt).map_err(|e| SecretError::Crypto(e.to_string()))?;
    let plain = Zeroizing::new(serde_json::to_vec(env).expect("a map serializes"));
    let cipher = seal(key, &salt, &plain)?;
    let names: Vec<String> = env.keys().cloned().collect();
    let inserted: bool = sqlx::query_scalar(
        "INSERT INTO task_secrets (bot_id, name, env_names, salt, ciphertext)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (bot_id, name) DO UPDATE SET env_names = $3, salt = $4, ciphertext = $5,
             updated_at = now()
         RETURNING (xmax = 0)",
    )
    .bind(bot_id)
    .bind(name)
    .bind(&names)
    .bind(&salt[..])
    .bind(&cipher)
    .fetch_one(pool)
    .await?;
    Ok(inserted)
}

pub async fn list(pool: &PgPool, bot_id: Uuid) -> Result<Vec<SecretInfo>, sqlx::Error> {
    let rows: Vec<(String, Vec<String>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT name, env_names, updated_at FROM task_secrets WHERE bot_id = $1 ORDER BY name",
    )
    .bind(bot_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, env, updated_at)| SecretInfo {
            name,
            env,
            updated_at,
        })
        .collect())
}

pub async fn delete(pool: &PgPool, bot_id: Uuid, name: &str) -> Result<bool, sqlx::Error> {
    Ok(
        sqlx::query("DELETE FROM task_secrets WHERE bot_id = $1 AND name = $2")
            .bind(bot_id)
            .bind(name)
            .execute(pool)
            .await?
            .rows_affected()
            == 1,
    )
}

/// Decrypt the named secrets of one bot into environment variables. A name
/// that is not this bot's is `Unknown`, the same as a missing one. Two
/// secrets setting the same variable are refused.
pub async fn resolve(
    pool: &PgPool,
    key: Option<&SecretKey>,
    bot_id: Uuid,
    names: &[String],
) -> Result<Env, SecretError> {
    let mut env = Env::default();
    if names.is_empty() {
        return Ok(env);
    }
    if names.len() > MAX_PER_COMMAND {
        return Err(SecretError::Invalid(format!(
            "at most {MAX_PER_COMMAND} secrets per command"
        )));
    }
    let key = key.ok_or(SecretError::NoKey)?;
    for name in names {
        let row: Option<(Vec<u8>, Vec<u8>)> = sqlx::query_as(
            "SELECT salt, ciphertext FROM task_secrets WHERE bot_id = $1 AND name = $2",
        )
        .bind(bot_id)
        .bind(name)
        .fetch_optional(pool)
        .await?;
        let Some((salt, cipher)) = row else {
            return Err(SecretError::Unknown(name.clone()));
        };
        let salt: [u8; 32] = salt
            .as_slice()
            .try_into()
            .map_err(|_| SecretError::Crypto("bad salt".into()))?;
        let plain = open(key, &salt, &cipher)?;
        let vars: BTreeMap<String, String> =
            serde_json::from_slice(&plain).map_err(|e| SecretError::Crypto(e.to_string()))?;
        for (var, value) in vars {
            let value = Zeroizing::new(value);
            if env.vars.iter().any(|(v, _)| *v == var) {
                return Err(SecretError::Invalid(format!(
                    "two secrets set {var}; name only one of them"
                )));
            }
            env.vars.push((var, value));
        }
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(values: &[(&str, &str)]) -> Vec<Redact> {
        Env {
            vars: values
                .iter()
                .map(|(n, v)| ((*n).to_owned(), Zeroizing::new((*v).to_owned())))
                .collect(),
        }
        .redaction()
    }

    #[test]
    fn values_are_redacted_by_hash_wherever_they_appear() {
        let set = set(&[("SMTP_PASSWORD", "hunter2-é-pass"), ("PIN", "123")]);
        assert_eq!(set.len(), 1, "too short to redact safely");
        let text = "login hunter2-é-pass ok; again:hunter2-é-pass.";
        assert_eq!(
            redact(text, &set),
            "login [secret SMTP_PASSWORD] ok; again:[secret SMTP_PASSWORD]."
        );
        assert_eq!(redact("nothing here", &set), "nothing here");
        assert!(!serde_json::to_string(&set).unwrap().contains("hunter2"));
        // A cut-off copy (a tail starting mid-value) is not a full match,
        // and a long output is scanned in linear time.
        assert_eq!(redact("2-é-pass", &set), "2-é-pass");
        let long = format!("{}hunter2-é-pass", "x".repeat(1_000_000));
        let started = std::time::Instant::now();
        assert!(redact(&long, &set).ends_with("[secret SMTP_PASSWORD]"));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn variable_names_cannot_change_how_commands_run() {
        let one = |k: &str| BTreeMap::from([(k.to_owned(), "value".to_owned())]);
        assert!(validate(&one("SMTP_PASSWORD")).is_ok());
        for bad in [
            "PATH",
            "LD_PRELOAD",
            "ELSE_TASK_GRANT",
            "lower",
            "1X",
            "HOME",
            "A-B",
        ] {
            assert!(validate(&one(bad)).is_err(), "{bad}");
        }
        assert!(validate(&BTreeMap::from([("X".to_owned(), "a\0b".to_owned())])).is_err());
        assert!(valid_name("smtp-test_1") && !valid_name("Smtp") && !valid_name("-x"));
    }
}
