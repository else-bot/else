//! Shared MCP packages. Each package actor owns its files and live connection.

pub mod oauth;
mod transport;
pub(crate) use transport::METHODS;

use std::collections::{BTreeMap, HashMap};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::actor::Actor;

const MAX_TEXT: usize = 64 * 1024;

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
    #[error("The owner needs to sign in to this MCP.")]
    AuthRequired(Box<oauth::ConnectionStatus>),
    #[error("The MCP requires OAuth authorization.")]
    AuthChallenge(String),
    #[error("The MCP authorization expired or was revoked. Sign in again.")]
    OAuthRevoked,
    #[error("The MCP request timed out and may have completed. Check its outcome before retrying.")]
    Timeout,
    #[error("The MCP library is paused for a backup or update. Try again afterward.")]
    Paused,
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
        #[serde(default)]
        headers: BTreeMap<String, String>,
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
            Self::Http { url, headers } => {
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
            Self::Http { url, headers } => {
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
                for (name, value) in headers {
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

/// A package as the owner's settings list it.
#[derive(Serialize)]
pub struct Listing {
    #[serde(flatten)]
    pub manifest: Summary,
    pub website: Option<String>,
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

struct Slot {
    path: PathBuf,
    /// One connection and one saved sign-in, shared by every bot.
    session: Option<oauth::Session>,
    oauth_key: Option<Arc<crate::credentials::SecretKey>>,
    oauth_client: Option<Arc<oauth::RegisteredClient>>,
    retired: bool,
    gate: Arc<crate::maintenance::Gate>,
}

impl Slot {
    async fn package(&self) -> Result<Package, Error> {
        if self.retired {
            return Err(Error::NotFound);
        }
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || read_package(&path))
            .await
            .map_err(|_| Error::Connection)?
    }

    async fn session(&mut self) -> Result<&mut oauth::Session, Error> {
        if self.session.is_none() {
            let store = oauth::Store::new(self.path.clone(), self.oauth_key.clone());
            self.session = Some(oauth::Session::open(store, self.oauth_client.clone()).await?);
        }
        Ok(self.session.as_mut().expect("opened MCP session"))
    }

    async fn close(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.close().await;
        }
    }
}

#[derive(Clone)]
struct Entry {
    epoch: Uuid,
    actor: Actor<Slot>,
}

#[derive(Clone)]
pub struct Library {
    root: PathBuf,
    gate: Arc<crate::maintenance::Gate>,
    slots: Actor<HashMap<String, Entry>>,
    oauth_key: Option<Arc<crate::credentials::SecretKey>>,
    oauth_client: Option<Arc<oauth::RegisteredClient>>,
}

impl Library {
    pub fn new(root: PathBuf, gate: Arc<crate::maintenance::Gate>) -> Self {
        Self {
            root,
            gate,
            slots: Actor::default(),
            oauth_key: None,
            oauth_client: None,
        }
    }

    pub fn with_oauth_key(mut self, key: Option<Arc<crate::credentials::SecretKey>>) -> Self {
        self.oauth_key = key;
        self
    }

    pub fn with_oauth_client(mut self, client: Option<Arc<oauth::RegisteredClient>>) -> Self {
        self.oauth_client = client;
        self
    }

    async fn slot(&self, id: &PackageId) -> Result<Entry, Error> {
        let id = id.0.clone();
        let path = self.root.join(&id);
        let gate = self.gate.clone();
        let oauth_key = self.oauth_key.clone();
        let oauth_client = self.oauth_client.clone();
        self.slots
            .call(move |slots| {
                if !slots.contains_key(&id) && slots.len() >= 1024 {
                    return Err(Error::Invalid(
                        "Too many MCP ids in this process. Restart before using another id.",
                    ));
                }
                Ok(slots
                    .entry(id)
                    .or_insert_with(|| Entry {
                        epoch: Uuid::new_v4(),
                        actor: Actor::new(Slot {
                            path,
                            session: None,
                            oauth_key,
                            oauth_client,
                            retired: false,
                            gate,
                        }),
                    })
                    .clone())
            })
            .await
    }

    async fn installed_slot(&self, id: &PackageId) -> Result<Entry, Error> {
        let metadata = tokio::fs::symlink_metadata(self.root.join(&id.0))
            .await
            .map_err(Error::from)
            .map_err(missing)?;
        if !metadata.is_dir() {
            return Err(Error::Invalid("Invalid MCP package directory."));
        }
        self.slot(id).await
    }

    pub async fn list(&self) -> Result<Vec<Summary>, Error> {
        installed(self.root.clone()).await
    }

    pub async fn listings(&self) -> Result<Vec<Listing>, Error> {
        let manifests = self.list().await?;
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || {
            manifests
                .into_iter()
                .map(|manifest| {
                    let website = read_file(&root.join(&manifest.id.0).join("config.json"))
                        .ok()
                        .and_then(|text| serde_json::from_str::<Transport>(&text).ok())
                        .and_then(|config| config.website());
                    Listing { manifest, website }
                })
                .collect()
        })
        .await
        .map_err(|_| Error::Connection)
    }

    pub async fn read(&self, id: &PackageId) -> Result<Package, Error> {
        self.installed_slot(id)
            .await?
            .actor
            .call_async(|slot| {
                Box::pin(async move {
                    if slot.retired {
                        return Err(Error::NotFound);
                    }
                    let path = slot.path.clone();
                    tokio::task::spawn_blocking(move || read_package(&path))
                        .await
                        .map_err(|_| Error::Connection)?
                })
            })
            .await
            .map_err(missing)
    }

    /// Publish a package, or return the identical one already under its id.
    /// Different files under the id are a conflict.
    pub async fn install(
        &self,
        input: Installation,
        installed_by: String,
    ) -> Result<Summary, Error> {
        self.publish(input, installed_by, false)
            .await
            .map(|published| published.manifest)
    }

    /// Publish a package in place of whatever is under its id. The caller
    /// has the owner's approval. A changed connection closes every session
    /// and drops their saved sign-ins.
    pub async fn replace(
        &self,
        input: Installation,
        installed_by: String,
    ) -> Result<Replaced, Error> {
        self.publish(input, installed_by, true).await
    }

    async fn publish(
        &self,
        input: Installation,
        installed_by: String,
        replace: bool,
    ) -> Result<Replaced, Error> {
        input.validate()?;
        let slot = self.slot(&input.id).await?;
        slot.actor
            .call_async(move |slot| {
                Box::pin(async move {
                    let _admission = slot.gate.enter().ok_or(Error::Paused)?;
                    if slot.retired {
                        return Err(Error::NotFound);
                    }
                    let path = slot.path.clone();
                    let published = tokio::task::spawn_blocking(move || {
                        install(&path, &input, installed_by, replace)
                    })
                    .await
                    .map_err(|_| Error::Connection)
                    .flatten();
                    // A failed replacement may still have published the new config,
                    // so no session that holds the old sign-in survives it.
                    if published.as_ref().map_or(replace, |p| p.reconnect) {
                        slot.close().await;
                    }
                    published
                })
            })
            .await
    }

    pub async fn remove(&self, id: &PackageId) -> Result<(), Error> {
        let entry = self.installed_slot(id).await?;
        let epoch = entry.epoch;
        let index = self.slots.clone();
        let id = id.0.clone();
        entry
            .actor
            .call_async(move |slot| {
                Box::pin(async move {
                    let _admission = slot.gate.enter().ok_or(Error::Paused)?;
                    if slot.retired {
                        return Err(Error::NotFound);
                    }
                    slot.close().await;
                    let path = slot.path.clone();
                    tokio::task::spawn_blocking(move || {
                        let parent = path.parent().expect("package parent");
                        require_dir(parent)?;
                        require_dir(&path)?;
                        let retired = parent.join(format!(".removed-{}", Uuid::new_v4()));
                        std::fs::rename(&path, &retired)?;
                        std::fs::File::open(parent)?.sync_all()?;
                        std::fs::remove_dir_all(retired)?;
                        std::fs::File::open(parent)?.sync_all()?;
                        Ok::<_, Error>(())
                    })
                    .await
                    .map_err(|_| Error::Connection)??;
                    slot.retired = true;
                    index
                        .call(move |slots| {
                            if slots.get(&id).is_some_and(|entry| entry.epoch == epoch) {
                                slots.remove(&id);
                            }
                        })
                        .await;
                    Ok(())
                })
            })
            .await
            .map_err(missing)
    }

    pub async fn request(
        &self,
        id: &PackageId,
        method: String,
        params: Value,
    ) -> Result<Value, Error> {
        self.installed_slot(id)
            .await?
            .actor
            .call_async(move |slot| {
                Box::pin(async move {
                    let _admission = slot.gate.enter().ok_or(Error::Paused)?;
                    let package = slot.package().await?;
                    slot.session()
                        .await?
                        .request(&package.config, method, params)
                        .await
                })
            })
            .await
            .map_err(missing)
    }

    pub async fn git_token(&self, id: &PackageId) -> Result<zeroize::Zeroizing<String>, Error> {
        self.installed_slot(id)
            .await?
            .actor
            .call_async(move |slot| {
                Box::pin(async move {
                    let _admission = slot.gate.enter().ok_or(Error::Paused)?;
                    let package = slot.package().await?;
                    slot.session().await?.git_token(&package.config).await
                })
            })
            .await
            .map_err(missing)
    }

    pub async fn connection_status(
        &self,
        id: &PackageId,
    ) -> Result<oauth::ConnectionStatus, Error> {
        self.installed_slot(id)
            .await?
            .actor
            .call_async(move |slot| {
                Box::pin(async move {
                    // Opening the session may adopt older sign-in files, a write
                    // that must wait for a backup or update.
                    let _admission = match slot.session {
                        Some(_) => None,
                        None => Some(slot.gate.enter().ok_or(Error::Paused)?),
                    };
                    slot.package().await?;
                    Ok(slot.session().await?.status())
                })
            })
            .await
            .map_err(missing)
    }

    pub async fn authorize(
        &self,
        id: &PackageId,
        redirect_uri: String,
    ) -> Result<oauth::Started, Error> {
        self.installed_slot(id)
            .await?
            .actor
            .call_async(move |slot| {
                Box::pin(async move {
                    let _admission = slot.gate.enter().ok_or(Error::Paused)?;
                    let package = slot.package().await?;
                    slot.session()
                        .await?
                        .authorize(&package.config, redirect_uri)
                        .await
                })
            })
            .await
            .map_err(missing)
    }

    pub async fn complete_authorization(
        &self,
        id: &PackageId,
        callback: oauth::Complete,
    ) -> Result<(), Error> {
        self.installed_slot(id)
            .await?
            .actor
            .call_async(move |slot| {
                Box::pin(async move {
                    let _admission = slot.gate.enter().ok_or(Error::Paused)?;
                    slot.package().await?;
                    slot.session().await?.complete(callback).await
                })
            })
            .await
            .map_err(missing)
    }

    pub async fn cancel_authorization(
        &self,
        id: &PackageId,
        request_id: Uuid,
    ) -> Result<(), Error> {
        self.installed_slot(id)
            .await?
            .actor
            .call_async(move |slot| {
                Box::pin(async move {
                    let _admission = slot.gate.enter().ok_or(Error::Paused)?;
                    slot.package().await?;
                    slot.session().await?.cancel(request_id);
                    Ok(())
                })
            })
            .await
            .map_err(missing)
    }

    pub async fn cancel_pending_authorization(
        &self,
        id: &PackageId,
    ) -> Result<oauth::ConnectionStatus, Error> {
        self.installed_slot(id)
            .await?
            .actor
            .call_async(move |slot| {
                Box::pin(async move {
                    let _admission = slot.gate.enter().ok_or(Error::Paused)?;
                    slot.package().await?;
                    Ok(slot.session().await?.cancel_pending())
                })
            })
            .await
            .map_err(missing)
    }

    pub async fn disconnect(&self, id: &PackageId) -> Result<(), Error> {
        self.installed_slot(id)
            .await?
            .actor
            .call_async(move |slot| {
                Box::pin(async move {
                    let _admission = slot.gate.enter().ok_or(Error::Paused)?;
                    slot.package().await?;
                    slot.session().await?.disconnect().await
                })
            })
            .await
            .map_err(missing)
    }
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
    use std::os::unix::ffi::OsStrExt;
    let c = |p: &Path| {
        std::ffi::CString::new(p.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from(ErrorKind::InvalidInput))
    };
    let (a, b) = (c(a)?, c(b)?);
    // SAFETY: two valid NUL-terminated paths relative to the cwd.
    #[cfg(target_os = "linux")]
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    // SAFETY: as above; macOS spells the exchange RENAME_SWAP.
    #[cfg(target_os = "macos")]
    let rc = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
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
