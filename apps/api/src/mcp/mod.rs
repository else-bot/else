//! Shared MCP packages. Each package actor owns its files and live connection.

pub mod events;
mod library;
pub mod oauth;
mod transport;
pub use library::{Config, Library};
pub(crate) use transport::METHODS;

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use else_wire::mcp::{InstalledMcp, McpConnectionStatus};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::db::task_secrets;

const MAX_TEXT: usize = 64 * 1024;
/// Sent on every MCP and OAuth request, because some firewalls block
/// clients that send none.
const USER_AGENT: &str = concat!("else.bot/", env!("ELSE_VERSION"));

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("That MCP is not installed.")]
    NotFound,
    #[error(
        "An MCP with that id is already installed with different files. Call mcp_install again with replace true to ask the owner to approve replacing it, or use a new id to add another account."
    )]
    Conflict,
    #[error("The shared MCP library could not be read or saved.")]
    Io(#[from] std::io::Error),
    #[error(
        "The MCP connection failed. Check its setup; do not repeat a mutation whose outcome is unknown."
    )]
    Connection,
    #[error(
        "The MCP server answered HTTP {0}. Check its URL and headers; do not repeat a mutation whose outcome is unknown."
    )]
    Http(u16),
    #[error("The owner needs to sign in to this MCP.")]
    AuthRequired(Box<McpConnectionStatus>),
    #[error("The MCP requires OAuth authorization.")]
    AuthChallenge(String),
    #[error("The MCP request timed out and may have completed. Check its outcome before retrying.")]
    Timeout,
    #[error("The MCP library is paused for a backup or update. Try again afterward.")]
    Paused,
    #[error("{0}")]
    Secret(String),
}

impl Error {
    /// A stable code for bots that branch on a failure without parsing its
    /// message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_request",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::Io(_) => "library_unavailable",
            Self::Connection => "connection_failed",
            Self::Http(_) => "http_status",
            Self::AuthRequired(_) | Self::AuthChallenge(_) => "auth_required",
            Self::Timeout => "timeout",
            Self::Paused => "paused",
            Self::Secret(_) => "secret_unavailable",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PackageId(String);

impl TryFrom<String> for PackageId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty()
            || value.len() > 64
            || !value.as_bytes()[0].is_ascii_lowercase()
            || !value
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            return Err(
                "Use an MCP id starting with a lowercase letter, followed by lowercase letters, digits or hyphens, at most 64 characters.",
            );
        }
        Ok(Self(value))
    }
}

impl From<PackageId> for String {
    fn from(id: PackageId) -> String {
        id.0
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Transport {
    Http {
        url: String,
        /// Values may hold `${VAR}` placeholders, filled from `secrets`
        /// each time a connection opens, so no value is saved here.
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        secrets: Vec<String>,
    },
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
}

impl Transport {
    /// The HTTP server's origin, for the owner's settings to find its icon.
    /// The path and query can hold credentials, so only the scheme, host and
    /// port leave.
    fn website(&self) -> Option<String> {
        let Self::Http { url, .. } = self else {
            return None;
        };
        let parsed = reqwest::Url::parse(url).ok()?;
        matches!(parsed.scheme(), "http" | "https")
            .then(|| parsed.origin().ascii_serialization())
            .filter(|origin| origin != "null")
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Http { .. } => "http",
            Self::Stdio { .. } => "stdio",
        }
    }

    /// What the owner approves when a replacement changes the connection.
    /// Header and environment values, URL queries and arguments can hold
    /// credentials, so only their names or counts appear.
    fn describe(&self) -> String {
        let names = |keys: Vec<&String>| {
            keys.into_iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self {
            Self::Http {
                url,
                headers,
                secrets,
            } => {
                let mut text = match reqwest::Url::parse(url) {
                    Ok(mut parsed) => {
                        let query = parsed.query().is_some();
                        parsed.set_query(None);
                        format!("HTTP {parsed}{}", if query { "?…" } else { "" })
                    }
                    Err(_) => "HTTP".to_owned(),
                };
                if !headers.is_empty() {
                    text.push_str(&format!(
                        " with headers {}",
                        names(headers.keys().collect())
                    ));
                }
                if !secrets.is_empty() {
                    text.push_str(&format!(
                        " from task secrets {}",
                        names(secrets.iter().collect())
                    ));
                }
                text
            }
            Self::Stdio { command, args, env } => {
                let mut text = format!("stdio {command}");
                if !args.is_empty() {
                    text.push_str(&format!(" with {} arguments", args.len()));
                }
                if !env.is_empty() {
                    text.push_str(&format!(" and environment {}", names(env.keys().collect())));
                }
                text
            }
        }
    }

    fn validate(&self) -> Result<(), Error> {
        match self {
            Self::Http {
                url,
                headers,
                secrets,
            } => {
                let url = reqwest::Url::parse(url)
                    .map_err(|_| Error::Invalid("Use a complete HTTP MCP URL."))?;
                if !matches!(url.scheme(), "http" | "https")
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                {
                    return Err(Error::Invalid(
                        "Use an http or https MCP URL without user info or a fragment.",
                    ));
                }
                if headers.len() > 32 {
                    return Err(Error::Invalid("Use at most 32 HTTP headers."));
                }
                if secrets.len() > MAX_SECRETS
                    || secrets.iter().any(|name| !task_secrets::valid_name(name))
                    || (1..secrets.len()).any(|i| secrets[..i].contains(&secrets[i]))
                {
                    return Err(Error::Invalid(
                        "Name at most 4 different task secrets, each in lowercase as stored.",
                    ));
                }
                for (name, value) in headers {
                    fill(value, |var| {
                        task_secrets::valid_var(var).then_some("").ok_or(Error::Invalid(
                            "Write each header placeholder as ${VAR}, naming an uppercase task secret variable.",
                        ))
                    })?;
                    if value.contains("${") && secrets.is_empty() {
                        return Err(Error::Invalid(
                            "Name the task secrets that set the header placeholders in secrets.",
                        ));
                    }
                    let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                        .map_err(|_| Error::Invalid("Invalid MCP HTTP header name."))?;
                    reqwest::header::HeaderValue::from_str(value)
                        .map_err(|_| Error::Invalid("Invalid MCP HTTP header value."))?;
                    if matches!(
                        name.as_str(),
                        "host"
                            | "content-length"
                            | "connection"
                            | "transfer-encoding"
                            | "content-type"
                            | "accept"
                            | "mcp-session-id"
                            | "mcp-protocol-version"
                    ) {
                        return Err(Error::Invalid(
                            "That HTTP header is managed by the MCP transport.",
                        ));
                    }
                }
            }
            Self::Stdio { command, args, env } => {
                if command.trim().is_empty()
                    || command.len() > 4096
                    || command.contains('\0')
                    || args.len() > 128
                    || env.len() > 64
                    || args
                        .iter()
                        .any(|arg| arg.len() > MAX_TEXT || arg.contains('\0'))
                {
                    return Err(Error::Invalid(
                        "Invalid MCP command, arguments or environment.",
                    ));
                }
                for (key, value) in env {
                    if key.is_empty()
                        || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                        || key.as_bytes()[0].is_ascii_digit()
                        || value.len() > MAX_TEXT
                        || value.contains('\0')
                        || key.starts_with("ELSE_")
                        || matches!(
                            key.as_str(),
                            "OWNER_TOKEN"
                                | "OWNER_TOKEN_FILE"
                                | "SECRET_KEY"
                                | "SECRET_KEY_FILE"
                                | "DATABASE_URL"
                        )
                    {
                        return Err(Error::Invalid(
                            "Invalid or reserved MCP environment variable.",
                        ));
                    }
                }
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| Error::Invalid("Invalid MCP config."))?
            .len()
            > MAX_TEXT
        {
            return Err(Error::Invalid("Keep the MCP config to 64 KB."));
        }
        Ok(())
    }
}

const MAX_SECRETS: usize = task_secrets::MAX_PER_COMMAND;

fn fill<'a>(
    template: &str,
    mut value: impl FnMut(&str) -> Result<&'a str, Error>,
) -> Result<Zeroizing<String>, Error> {
    let mut out = Zeroizing::new(String::with_capacity(template.len()));
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or(Error::Invalid(
            "Close each ${VAR} placeholder in the MCP headers.",
        ))?;
        out.push_str(value(&after[..end])?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[derive(Clone)]
pub struct TaskSecrets {
    pool: sqlx::SqlitePool,
    key: Option<Arc<crate::credentials::SecretKey>>,
}

impl TaskSecrets {
    pub fn new(pool: sqlx::SqlitePool, key: Option<Arc<crate::credentials::SecretKey>>) -> Self {
        Self { pool, key }
    }

    /// The config to connect with: header placeholders filled from the
    /// named task secrets, read now so a secret the owner replaced applies
    /// to the next connection. Also the values to redact from what the
    /// server returns, as for guest commands.
    pub async fn fill(
        this: Option<&Self>,
        config: &Transport,
    ) -> Result<(Transport, Vec<task_secrets::Redact>), Error> {
        let Transport::Http {
            url,
            headers,
            secrets,
        } = config
        else {
            return Ok((config.clone(), Vec::new()));
        };
        if secrets.is_empty() {
            return Ok((config.clone(), Vec::new()));
        }
        let this = this.ok_or_else(|| {
            Error::Secret("Task secrets are not available on this server.".into())
        })?;
        let env = task_secrets::resolve(&this.pool, this.key.as_deref(), secrets)
            .await
            .map_err(|error| match error {
                task_secrets::SecretError::Unknown(name) => Error::Secret(format!(
                    "No task secret named {name}. Ask the owner to store it, then try again."
                )),
                task_secrets::SecretError::NoKey => {
                    Error::Secret("Task secrets need the installation's SECRET_KEY.".into())
                }
                task_secrets::SecretError::Invalid(message) => Error::Secret(message),
                error => {
                    tracing::warn!(%error, "MCP task secrets could not be read");
                    Error::Secret("The MCP's task secrets could not be read.".into())
                }
            })?;
        let mut filled = BTreeMap::new();
        for (name, template) in headers {
            let value = fill(template, |var| {
                env.vars
                    .iter()
                    .find(|(name, _)| name == var)
                    .map(|(_, value)| value.as_str())
                    .ok_or_else(|| {
                        Error::Secret(format!(
                            "The task secrets {} do not set {var}. Ask the owner to add it.",
                            secrets.join(", ")
                        ))
                    })
            })?;
            filled.insert(name.clone(), value.to_string());
        }
        let filled = Transport::Http {
            url: url.clone(),
            headers: filled,
            secrets: Vec::new(),
        };
        Ok((filled, env.redaction()))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installation {
    pub id: PackageId,
    pub name: String,
    pub description: String,
    pub skill: String,
    pub documentation: String,
    pub config: Transport,
}

impl Installation {
    pub fn validate(&self) -> Result<(), Error> {
        if self.name.trim().is_empty()
            || self.name.len() > 120
            || self.name.chars().any(char::is_control)
            || self.description.len() > 1000
            || self.description.contains('\0')
            || self.skill.trim().is_empty()
            || self.documentation.trim().is_empty()
            || self.skill.len() > MAX_TEXT
            || self.documentation.len() > MAX_TEXT
            || self.skill.contains('\0')
            || self.documentation.contains('\0')
        {
            return Err(Error::Invalid(
                "Supply a name, description, skill and documentation. Keep each document to 64 KB.",
            ));
        }
        self.config.validate()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    pub id: PackageId,
    pub name: String,
    pub description: String,
    pub installed_by: String,
    pub installed_at: String,
    pub transport: String,
}

impl From<&Summary> for InstalledMcp {
    fn from(summary: &Summary) -> Self {
        Self {
            id: summary.id.0.clone(),
            name: summary.name.clone(),
            description: summary.description.clone(),
            installed_by: summary.installed_by.clone(),
            installed_at: summary.installed_at.clone(),
            transport: Some(summary.transport.clone()),
            website: None,
        }
    }
}

#[derive(Serialize)]
pub struct Package {
    pub manifest: Summary,
    pub skill: String,
    pub documentation: String,
    #[serde(skip)]
    config: Transport,
}

impl Package {
    pub fn matches(&self, input: &Installation) -> bool {
        self.manifest.name == input.name
            && self.manifest.description == input.description
            && self.skill == input.skill
            && self.documentation == input.documentation
            && self.same_connection(input)
    }

    /// Saved sign-ins and tool permissions belong to this connection.
    pub fn same_connection(&self, input: &Installation) -> bool {
        self.config == input.config
    }

    /// The approval card text for replacing this package with `input`.
    pub fn replacement(&self, input: &Installation) -> String {
        let mut changed = Vec::new();
        if self.manifest.name != input.name || self.manifest.description != input.description {
            changed.push("name or description");
        }
        if self.skill != input.skill {
            changed.push("skill");
        }
        if self.documentation != input.documentation {
            changed.push("documentation");
        }
        if !self.same_connection(input) {
            changed.push("connection");
        }
        let mut text = format!(
            "MCP: {}\nCurrent: {}, {}\nNew: {}, {}\nChanges: {}",
            self.manifest.id.0,
            self.manifest.name,
            self.config.describe(),
            input.name,
            input.config.describe(),
            changed.join(", "),
        );
        if self.same_connection(input) {
            text.push_str("\nSaved sign-ins and MCP tool permissions are kept.");
        } else {
            text.push_str(
                "\nEvery bot's saved sign-in and MCP tool permissions for it are removed. \
                 Bots sign in again on their next use.",
            );
        }
        text
    }
}

/// The package a replacement published, and whether its connection changed,
/// which removed every bot's saved sign-in.
#[derive(Debug)]
pub struct Replaced {
    pub manifest: Summary,
    pub reconnect: bool,
}

/// Remove unpublished and retired packages after the API's process sweep, before accepting work.
pub fn cleanup(root: &Path) -> Result<(), Error> {
    match std::fs::symlink_metadata(root) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(_) => require_dir(root)?,
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let uuid = name
            .strip_prefix(".install-")
            .or_else(|| name.strip_prefix(".removed-"));
        if uuid.is_some_and(|uuid| Uuid::parse_str(uuid).is_ok()) {
            if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(entry.path())?;
            } else {
                std::fs::remove_file(entry.path())?;
            }
        }
    }
    std::fs::File::open(root)?.sync_all()?;
    Ok(())
}

fn missing(error: Error) -> Error {
    match error {
        Error::Io(err) if err.kind() == ErrorKind::NotFound => Error::NotFound,
        other => other,
    }
}

fn require_dir(path: &Path) -> Result<(), Error> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(Error::Invalid(
            "MCP package directories cannot be symlinks.",
        ));
    }
    Ok(())
}

fn read_file(path: &Path) -> Result<String, Error> {
    use std::io::Read;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > MAX_TEXT as u64 {
        return Err(Error::Invalid("Invalid MCP package file."));
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_TEXT as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_TEXT {
        return Err(Error::Invalid("Invalid MCP package file."));
    }
    String::from_utf8(bytes).map_err(|_| Error::Invalid("MCP package files must be UTF-8."))
}

fn read_manifest(path: &Path) -> Result<Summary, Error> {
    require_dir(path.parent().expect("package parent"))?;
    require_dir(path)?;
    serde_json::from_str(&read_file(&path.join("manifest.json"))?)
        .map_err(|_| Error::Invalid("Invalid MCP manifest."))
}

fn read_package(path: &Path) -> Result<Package, Error> {
    let manifest = read_manifest(path)?;
    let config: Transport = serde_json::from_str(&read_file(&path.join("config.json"))?)
        .map_err(|_| Error::Invalid("Invalid MCP config."))?;
    let skill = read_file(&path.join("SKILL.md"))?;
    let documentation = read_file(&path.join("README.md"))?;
    Installation {
        id: manifest.id.clone(),
        name: manifest.name.clone(),
        description: manifest.description.clone(),
        skill: skill.clone(),
        documentation: documentation.clone(),
        config: config.clone(),
    }
    .validate()?;
    Ok(Package {
        manifest,
        config,
        skill,
        documentation,
    })
}

fn install(
    path: &Path,
    input: &Installation,
    installed_by: String,
    replace: bool,
) -> Result<Replaced, Error> {
    let parent = path.parent().expect("package parent");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    require_dir(parent)?;
    let existing = match read_package(path) {
        Ok(existing) if existing.matches(input) => {
            return Ok(Replaced {
                manifest: existing.manifest,
                reconnect: false,
            });
        }
        Ok(_) if !replace => return Err(Error::Conflict),
        Ok(existing) => Some(existing),
        Err(Error::Io(err)) if err.kind() == ErrorKind::NotFound => {
            if std::fs::symlink_metadata(path).is_ok() {
                return Err(Error::Invalid("The existing MCP package is incomplete."));
            }
            None
        }
        Err(err) => return Err(err),
    };
    let manifest = Summary {
        id: input.id.clone(),
        name: input.name.clone(),
        description: input.description.clone(),
        installed_by,
        installed_at: Utc::now().to_rfc3339(),
        transport: input.config.label().into(),
    };
    let staging = parent.join(format!(".install-{}", Uuid::new_v4()));
    std::fs::DirBuilder::new().mode(0o700).create(&staging)?;
    let result = (|| {
        for (name, bytes) in [
            (
                "manifest.json",
                serde_json::to_vec_pretty(&manifest).expect("MCP manifest JSON"),
            ),
            (
                "config.json",
                serde_json::to_vec_pretty(&input.config).expect("MCP config JSON"),
            ),
            ("SKILL.md", input.skill.as_bytes().to_vec()),
            ("README.md", input.documentation.as_bytes().to_vec()),
        ] {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(staging.join(name))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        let keep_sign_ins = existing
            .as_ref()
            .is_some_and(|existing| existing.same_connection(input));
        if keep_sign_ins {
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("oauth-")
                    && name.ends_with(".json")
                    && entry.file_type()?.is_file()
                {
                    std::fs::hard_link(entry.path(), staging.join(entry.file_name()))?;
                }
            }
        }
        std::fs::File::open(&staging)?.sync_all()?;
        if existing.is_some() {
            exchange(&staging, path)?;
        } else {
            std::fs::rename(&staging, path)?;
        }
        std::fs::File::open(parent)?.sync_all()?;
        Ok::<_, Error>(Replaced {
            manifest,
            reconnect: existing.is_some() && !keep_sign_ins,
        })
    })();
    let _ = std::fs::remove_dir_all(&staging);
    if result.is_ok() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    result
}

/// Atomically swap two directories, so a crash leaves either package whole.
/// The old one is left at `a`, under a staging name that startup cleanup removes.
fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    use rustix::fs::{CWD, RenameFlags};
    Ok(rustix::fs::renameat_with(
        CWD,
        a,
        CWD,
        b,
        RenameFlags::EXCHANGE,
    )?)
}

/// The packages installed under `root`, sorted by name. Reads only the
/// manifests, so a turn can list them without the library's connections.
pub async fn installed(root: PathBuf) -> Result<Vec<Summary>, Error> {
    tokio::task::spawn_blocking(move || {
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        require_dir(&root)?;
        let mut summaries = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(id) = PackageId::try_from(name) else {
                tracing::warn!("Ignoring an invalid directory in the shared MCP library");
                continue;
            };
            match read_manifest(&root.join(&id.0)) {
                Ok(manifest) if manifest.id == id => summaries.push(manifest),
                Ok(_) => tracing::warn!("Ignoring a mismatched MCP manifest id"),
                Err(Error::Io(err)) if err.kind() == ErrorKind::NotFound => continue,
                Err(_) => tracing::warn!("Ignoring an invalid MCP manifest"),
            }
        }
        summaries.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
        Ok(summaries)
    })
    .await
    .map_err(|_| Error::Connection)?
}

#[cfg(test)]
mod tests;
