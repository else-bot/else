//! The Vault: owner-provided credentials every bot on this server can use.
//!
//! A task secret (a Vault entry) is a named set of environment variables that
//! belongs to the installation, so every bot on this server can name it, for
//! example the SMTP settings a mailing script needs or a website login. It
//! also has a description and the website it signs in to, so a bot can find
//! it, and a record of which bot used it last. Values are encrypted at
//! rest with an age identity derived from `SECRET_KEY` and a per-secret salt
//! (purpose `else.bot/task-secrets/v1`); only the variable names are stored
//! in the clear and shown to the bot. A value reaches the guest only as an
//! environment variable of the one command or job that names the secret,
//! sent over the runtime's stdin, never in a command line, a file, or saved
//! source. Output that would show a value is filtered inside the guest and
//! redacted again before it is returned or stored; nothing derived from a
//! value is stored.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;
use sqlx::{SqliteConnection, SqlitePool};
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
pub const MAX_DESCRIPTION: usize = 200;
/// Uses kept per secret; older ones are pruned as new ones are recorded.
const USES_KEPT: i64 = 100;

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
    #[error("a task secret named {0} already exists")]
    Exists(String),
}

/// The host a Vault entry signs in to, lowercase with no leading `www.`.
/// A browser fill of the entry is refused on any page outside it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Website(String);

impl Website {
    /// Accepts a host (`x.com`) or a URL (`https://www.x.com/login`).
    pub fn parse(input: &str) -> Result<Self, SecretError> {
        let invalid = || SecretError::Invalid("website is a host such as x.com or a URL".into());
        let input = input.trim();
        if input.is_empty() || input.len() > 2000 {
            return Err(invalid());
        }
        let url = if input.contains("://") {
            reqwest::Url::parse(input)
        } else {
            reqwest::Url::parse(&format!("https://{input}"))
        }
        .map_err(|_| invalid())?;
        let host = url
            .host_str()
            .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
            .ok_or_else(invalid)?;
        let host = host.strip_prefix("www.").unwrap_or(&host).to_owned();
        let valid = !host.is_empty()
            && host.len() <= 253
            && host.contains('.')
            && host
                .chars()
                .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '.' | '-'));
        if !valid {
            return Err(invalid());
        }
        Ok(Self(host))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether a page on `host` belongs to this website (itself or a subdomain).
    pub fn covers(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        host == self.0
            || host
                .strip_suffix(self.0.as_str())
                .is_some_and(|rest| rest.ends_with('.'))
    }

    fn stored(text: Option<String>) -> Option<Self> {
        text.map(Self)
    }
}

/// A description as stored: trimmed, at most 200 bytes, one line.
pub fn description(text: &str) -> Result<String, SecretError> {
    let text = text.trim();
    if text.len() > MAX_DESCRIPTION || text.chars().any(char::is_control) {
        return Err(SecretError::Invalid(format!(
            "a description is one line of at most {MAX_DESCRIPTION} bytes"
        )));
    }
    Ok(text.to_owned())
}

/// How a bot used a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Use {
    Command,
    Job,
    Browser,
}

impl Use {
    fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Job => "job",
            Self::Browser => "browser",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "command" => Some(Self::Command),
            "job" => Some(Self::Job),
            "browser" => Some(Self::Browser),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LastUse {
    pub bot_id: Uuid,
    pub bot_name: Option<String>,
    #[serde(rename = "use")]
    pub kind: Use,
    pub at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SecretInfo {
    pub name: String,
    pub description: String,
    pub website: Option<Website>,
    pub env: Vec<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub last_used: Option<LastUse>,
}

impl SecretInfo {
    /// Whether this entry answers a bot's search: words of the query found
    /// in its name, description, website, or variable names, or a URL or
    /// host the entry's website covers.
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        if let (Some(website), Ok(asked)) = (&self.website, Website::parse(&query))
            && (website.covers(asked.as_str()) || asked.covers(website.as_str()))
        {
            return true;
        }
        let haystack = format!(
            "{} {} {} {}",
            self.name,
            self.description,
            self.website.as_ref().map_or("", Website::as_str),
            self.env.join(" ")
        )
        .to_lowercase();
        query.split_whitespace().all(|word| haystack.contains(word))
    }
}

/// What the owner says about an entry besides its values. `None` keeps
/// what is stored.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub description: Option<String>,
    pub website: Option<Option<Website>>,
}

/// A decrypted secret's variables; cleared when dropped.
#[derive(Default)]
pub struct Env {
    pub vars: Vec<(String, Zeroizing<String>)>,
}

impl Env {
    /// The values to redact from output (4 bytes or longer), held in
    /// memory for one call and cleared when dropped. Nothing derived from a
    /// value is stored: a stored fingerprint of a short value could be
    /// brute-forced without `SECRET_KEY`.
    pub fn redaction(&self) -> Vec<Redact> {
        let mut set: Vec<Redact> = self
            .vars
            .iter()
            .filter(|(_, v)| v.len() >= MIN_REDACTED)
            .map(|(name, v)| Redact {
                name: name.clone(),
                value: v.clone(),
            })
            .collect();
        set.sort_by_key(|r| std::cmp::Reverse(r.value.len()));
        set
    }
}

#[derive(Clone)]
pub struct Redact {
    pub name: String,
    pub value: Zeroizing<String>,
}

/// Replace every occurrence of a redacted value in `text` with
/// `[secret NAME]` (linear-time substring search per value).
pub fn redact(text: &str, set: &[Redact]) -> String {
    let mut out = text.to_owned();
    for r in set {
        if out.contains(r.value.as_str()) {
            out = out.replace(r.value.as_str(), &format!("[secret {}]", r.name));
        }
    }
    out
}

/// `redact` applied to every string, number, and key of a JSON value.
pub fn redact_json(value: Value, set: &[Redact]) -> Value {
    match value {
        Value::String(text) => Value::String(redact(&text, set)),
        Value::Number(number) => {
            let text = number.to_string();
            let redacted = redact(&text, set);
            if redacted == text {
                Value::Number(number)
            } else {
                Value::String(redacted)
            }
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|v| redact_json(v, set)).collect())
        }
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(k, v)| (redact(&k, set), redact_json(v, set)))
                .collect(),
        ),
        other => other,
    }
}

pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('a'..='z'))
        && name.len() <= 40
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_' | '-'))
}

pub fn valid_var(name: &str) -> bool {
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

/// A validated secret, encrypted and ready to write; holds no plaintext.
pub struct Sealed {
    names: Vec<String>,
    salt: [u8; 32],
    cipher: Vec<u8>,
}

/// Validate and encrypt a secret's variables outside any transaction.
pub fn seal_env(
    key: &SecretKey,
    name: &str,
    env: &BTreeMap<String, String>,
) -> Result<Sealed, SecretError> {
    if !valid_name(name) {
        return Err(SecretError::Invalid(
            "a secret name is 1 to 40 of a-z, 0-9, _ and -, starting with a letter".into(),
        ));
    }
    validate(env)?;
    let plain = Zeroizing::new(serde_json::to_vec(env).expect("a map serializes"));
    let sealed = crate::sealed::seal(key, PURPOSE, &plain)
        .map_err(|e| SecretError::Crypto(e.to_string()))?;
    Ok(Sealed {
        names: env.keys().cloned().collect(),
        salt: sealed.salt,
        cipher: sealed.cipher,
    })
}

/// Write a sealed secret (create or replace) in the caller's transaction.
/// Returns whether it was new.
pub async fn write_in(
    conn: &mut SqliteConnection,
    name: &str,
    sealed: &Sealed,
    meta: &Meta,
) -> Result<bool, sqlx::Error> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM task_secrets WHERE name = ?1)")
            .bind(name)
            .fetch_one(&mut *conn)
            .await?;
    sqlx::query(
        "INSERT INTO task_secrets (name, env_names, salt, ciphertext, description, website)
         VALUES (?1, ?2, ?3, ?4, coalesce(?5, ''), CASE WHEN ?6 THEN ?7 END)
         ON CONFLICT (name) DO UPDATE SET env_names = ?2, salt = ?3, ciphertext = ?4,
             description = coalesce(?5, description),
             website = CASE WHEN ?6 THEN ?7 ELSE website END,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')",
    )
    .bind(name)
    .bind(sqlx::types::Json(&sealed.names))
    .bind(&sealed.salt[..])
    .bind(&sealed.cipher)
    .bind(meta.description.as_deref())
    .bind(meta.website.is_some())
    .bind(
        meta.website
            .as_ref()
            .and_then(|w| w.as_ref().map(Website::as_str)),
    )
    .execute(&mut *conn)
    .await?;
    Ok(!exists)
}

/// Whether a write may replace an existing secret of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    CreateOrReplace,
    CreateOnly,
}

/// Store a secret. Returns whether it was new; `CreateOnly` refuses a
/// name already taken with `Exists` and changes nothing.
pub async fn put(
    pool: &SqlitePool,
    key: &SecretKey,
    name: &str,
    env: &BTreeMap<String, String>,
    meta: &Meta,
    mode: Mode,
) -> Result<bool, SecretError> {
    let sealed = seal_env(key, name, env)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if mode == Mode::CreateOnly && get(&mut tx, name).await?.is_some() {
        return Err(SecretError::Exists(name.to_owned()));
    }
    let created = write_in(&mut tx, name, &sealed, meta).await?;
    tx.commit().await?;
    Ok(created)
}

type InfoRow = (
    String,
    sqlx::types::Json<Vec<String>>,
    chrono::DateTime<chrono::Utc>,
    String,
    Option<String>,
    Option<Uuid>,
    Option<String>,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
);

const INFO: &str = "SELECT s.name, s.env_names, s.updated_at, s.description, s.website,
        u.bot_id, b.name, u.use, u.used_at
    FROM task_secrets s
    LEFT JOIN secret_uses u ON u.id = (SELECT max(id) FROM secret_uses WHERE secret = s.name)
    LEFT JOIN bots b ON b.id = u.bot_id";

fn info(
    (name, env, updated_at, description, website, bot_id, bot_name, kind, at): InfoRow,
) -> SecretInfo {
    let last_used = match (bot_id, kind.as_deref().and_then(Use::parse), at) {
        (Some(bot_id), Some(kind), Some(at)) => Some(LastUse {
            bot_id,
            bot_name,
            kind,
            at,
        }),
        _ => None,
    };
    SecretInfo {
        name,
        description,
        website: Website::stored(website),
        env: env.0,
        updated_at,
        last_used,
    }
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<SecretInfo>, sqlx::Error> {
    let rows: Vec<InfoRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!("{INFO} ORDER BY s.name")))
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(info).collect())
}

pub async fn get(
    conn: &mut SqliteConnection,
    name: &str,
) -> Result<Option<SecretInfo>, sqlx::Error> {
    let row: Option<InfoRow> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!("{INFO} WHERE s.name = ?1")))
            .bind(name)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(row.map(info))
}

/// An owner's edit of an entry: metadata, values to set, and variables to
/// remove. Unchanged values are re-encrypted with a fresh salt.
#[derive(Default)]
pub struct Patch {
    pub meta: Meta,
    pub set: BTreeMap<String, String>,
    pub remove: BTreeSet<String>,
}

/// Apply an owner's edit atomically and return the entry as stored.
pub async fn update(
    pool: &SqlitePool,
    key: Option<&SecretKey>,
    name: &str,
    patch: &Patch,
) -> Result<SecretInfo, SecretError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT salt, ciphertext FROM task_secrets WHERE name = ?1")
            .bind(name)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((salt, cipher)) = row else {
        return Err(SecretError::Unknown(name.to_owned()));
    };
    if patch.set.is_empty() && patch.remove.is_empty() {
        sqlx::query(
            "UPDATE task_secrets SET description = coalesce(?2, description),
                 website = CASE WHEN ?3 THEN ?4 ELSE website END,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE name = ?1",
        )
        .bind(name)
        .bind(patch.meta.description.as_deref())
        .bind(patch.meta.website.is_some())
        .bind(
            patch
                .meta
                .website
                .as_ref()
                .and_then(|w| w.as_ref().map(Website::as_str)),
        )
        .execute(&mut *tx)
        .await?;
    } else {
        let key = key.ok_or(SecretError::NoKey)?;
        let mut env = decrypt(key, &salt, &cipher)?;
        for var in &patch.remove {
            if env.remove(var).is_none() {
                return Err(SecretError::Invalid(format!(
                    "{name} has no variable {var}"
                )));
            }
        }
        for (var, value) in &patch.set {
            env.insert(var.clone(), Zeroizing::new(value.clone()));
        }
        let mut plain: BTreeMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().to_owned()))
            .collect();
        let sealed = seal_env(key, name, &plain);
        plain.values_mut().for_each(zeroize::Zeroize::zeroize);
        write_in(&mut tx, name, &sealed?, &patch.meta).await?;
    }
    let found = get(&mut tx, name)
        .await?
        .ok_or_else(|| SecretError::Unknown(name.to_owned()))?;
    tx.commit().await?;
    Ok(found)
}

/// Record that a bot used these secrets, keeping the latest uses of each.
/// A failure is logged: an audit gap never fails the bot's command.
pub async fn record_use(
    pool: &SqlitePool,
    names: &[String],
    bot_id: Uuid,
    turn_id: Option<Uuid>,
    kind: Use,
) {
    for name in names {
        let recorded = sqlx::query(
            "INSERT INTO secret_uses (secret, bot_id, turn_id, use)
             SELECT name, ?2, ?3, ?4 FROM task_secrets WHERE name = ?1",
        )
        .bind(name)
        .bind(bot_id)
        .bind(turn_id)
        .bind(kind.as_str())
        .execute(pool)
        .await;
        let pruned = match recorded {
            Ok(_) => {
                sqlx::query(
                    "DELETE FROM secret_uses WHERE secret = ?1 AND id < (
                         SELECT min(id) FROM (SELECT id FROM secret_uses WHERE secret = ?1
                                              ORDER BY id DESC LIMIT ?2))",
                )
                .bind(name)
                .bind(USES_KEPT)
                .execute(pool)
                .await
            }
            Err(err) => Err(err),
        };
        if let Err(err) = pruned {
            tracing::warn!(secret = %name, error = %err, "a secret use was not recorded");
        }
    }
}

pub async fn delete(pool: &SqlitePool, name: &str) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query("DELETE FROM task_secrets WHERE name = ?1")
        .bind(name)
        .execute(pool)
        .await?
        .rows_affected()
        == 1)
}

/// Whether an entry is reserved for a provider's own login. Cursor's SDK
/// adapter kept its API key in `provider-cursor`; installations that added
/// one keep it hidden from bots' commands, jobs, logins and listing.
pub fn provider_only(name: &str) -> bool {
    name.starts_with("provider-")
}

/// Decrypt the named secrets into environment variables. A missing or
/// provider-only name is `Unknown`. Two secrets setting the same variable
/// are refused.
pub async fn resolve(
    pool: &SqlitePool,
    key: Option<&SecretKey>,
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
        if provider_only(name) {
            return Err(SecretError::Unknown(name.clone()));
        }
        let row: Option<(Vec<u8>, Vec<u8>)> =
            sqlx::query_as("SELECT salt, ciphertext FROM task_secrets WHERE name = ?1")
                .bind(name)
                .fetch_optional(pool)
                .await?;
        let Some((salt, cipher)) = row else {
            return Err(SecretError::Unknown(name.clone()));
        };
        for (var, value) in decrypt(key, &salt, &cipher)? {
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

/// An entry's website and values, read together so the website checked is
/// the one stored with these values. A provider-only entry is `Unknown`.
pub async fn login(
    pool: &SqlitePool,
    key: Option<&SecretKey>,
    name: &str,
) -> Result<(Option<Website>, Env), SecretError> {
    if provider_only(name) {
        return Err(SecretError::Unknown(name.to_owned()));
    }
    let row: Option<(Option<String>, Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT website, salt, ciphertext FROM task_secrets WHERE name = ?1")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    let (website, salt, cipher) = row.ok_or_else(|| SecretError::Unknown(name.to_owned()))?;
    let vars = decrypt(key.ok_or(SecretError::NoKey)?, &salt, &cipher)?;
    Ok((
        Website::stored(website),
        Env {
            vars: vars.into_iter().collect(),
        },
    ))
}

/// What to redact from a bot's browser results: the values of every entry
/// whose website covers `host` and of every entry this bot has typed into
/// its browser, since a page keeps a value after the call that filled it
/// and a result can carry text from an earlier page. Each value is also
/// matched in the forms a snapshot shows it in. Best effort: the bot's own
/// computer can always read its pages another way.
pub async fn browser_redaction(
    pool: &SqlitePool,
    key: Option<&SecretKey>,
    bot_id: Uuid,
    host: Option<&str>,
) -> Result<Vec<Redact>, SecretError> {
    type Row = (Option<String>, bool, Vec<u8>, Vec<u8>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT s.website, EXISTS (SELECT 1 FROM secret_uses u WHERE u.secret = s.name
                                   AND u.bot_id = ?1 AND u.use = 'browser'),
                s.salt, s.ciphertext
         FROM task_secrets s WHERE s.website IS NOT NULL",
    )
    .bind(bot_id)
    .fetch_all(pool)
    .await?;
    let mut env = Env::default();
    for (website, filled, salt, cipher) in rows {
        let on_page = host
            .is_some_and(|host| Website::stored(website.clone()).is_some_and(|w| w.covers(host)));
        if filled || on_page {
            env.vars
                .extend(decrypt(key.ok_or(SecretError::NoKey)?, &salt, &cipher)?);
        }
    }
    let mut set = Vec::new();
    for redact in env.redaction() {
        for form in shown_forms(&redact.value) {
            if form.len() >= MIN_REDACTED && form != *redact.value {
                set.push(Redact {
                    name: redact.name.clone(),
                    value: Zeroizing::new(form),
                });
            }
        }
        set.push(redact);
    }
    set.sort_by_key(|r| std::cmp::Reverse(r.value.len()));
    Ok(set)
}

/// How a browser snapshot can print a value: whitespace collapsed, cut to
/// 100 characters, and JSON-escaped with non-ASCII as `\uXXXX`.
fn shown_forms(value: &str) -> Vec<String> {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = collapsed.chars().take(100).collect();
    let mut forms = Vec::new();
    for text in [collapsed, cut] {
        let mut escaped = String::new();
        for c in text.chars() {
            match c {
                '"' => escaped.push_str("\\\""),
                '\\' => escaped.push_str("\\\\"),
                ' '..='~' => escaped.push(c),
                _ => {
                    let mut units = [0u16; 2];
                    for unit in c.encode_utf16(&mut units) {
                        escaped.push_str(&format!("\\u{unit:04x}"));
                    }
                }
            }
        }
        forms.push(escaped);
        forms.push(text);
    }
    forms.dedup();
    forms
}

/// One entry's values, for the owner only.
pub async fn reveal(
    pool: &SqlitePool,
    key: Option<&SecretKey>,
    name: &str,
) -> Result<BTreeMap<String, Zeroizing<String>>, SecretError> {
    let row: Option<(Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT salt, ciphertext FROM task_secrets WHERE name = ?1")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    let (salt, cipher) = row.ok_or_else(|| SecretError::Unknown(name.to_owned()))?;
    decrypt(key.ok_or(SecretError::NoKey)?, &salt, &cipher)
}

fn decrypt(
    key: &SecretKey,
    salt: &[u8],
    cipher: &[u8],
) -> Result<BTreeMap<String, Zeroizing<String>>, SecretError> {
    let plain = crate::sealed::open(key, PURPOSE, salt, cipher)
        .map_err(|e| SecretError::Crypto(e.to_string()))?;
    let vars: BTreeMap<String, String> =
        serde_json::from_slice(&plain).map_err(|e| SecretError::Crypto(e.to_string()))?;
    Ok(vars
        .into_iter()
        .map(|(k, v)| (k, Zeroizing::new(v)))
        .collect())
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

    #[test]
    fn a_website_covers_itself_and_its_subdomains_only() {
        let x = Website::parse("https://WWW.x.com/i/flow/login?x=1").unwrap();
        assert_eq!(x.as_str(), "x.com");
        assert_eq!(Website::parse("x.com.").unwrap(), x);
        assert!(x.covers("x.com") && x.covers("mobile.x.com") && x.covers("X.COM."));
        for other in ["evilx.com", "x.com.evil.net", "com", ""] {
            assert!(!x.covers(other), "{other}");
        }
        for bad in [
            "",
            "localhost",
            "not a site",
            "file:///etc/passwd",
            "https://",
        ] {
            assert!(Website::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_search_matches_words_or_the_site_of_a_url() {
        let entry = SecretInfo {
            name: "x-login".into(),
            description: "X account @example".into(),
            website: Some(Website::parse("x.com").unwrap()),
            env: vec!["PASSWORD".into(), "USERNAME".into()],
            updated_at: chrono::Utc::now(),
            last_used: None,
        };
        for hit in [
            "",
            "x account",
            "EXAMPLE",
            "https://mobile.x.com/login",
            "password",
        ] {
            assert!(entry.matches(hit), "{hit}");
        }
        for miss in ["gmail.com", "account gmail", "evilx.com"] {
            assert!(!entry.matches(miss), "{miss}");
        }
    }

    #[test]
    fn a_value_is_matched_as_a_snapshot_prints_it() {
        let forms = shown_forms("élise  @example.com");
        assert!(forms.contains(&"élise @example.com".to_owned()));
        assert!(forms.contains(&"\\u00e9lise @example.com".to_owned()));
        let long = "x".repeat(120);
        assert!(forms_contain(&long, &"x".repeat(100)));
        assert!(shown_forms("say \"hi\"").contains(&"say \\\"hi\\\"".to_owned()));
    }

    fn forms_contain(value: &str, form: &str) -> bool {
        shown_forms(value).iter().any(|f| f == form)
    }
}
