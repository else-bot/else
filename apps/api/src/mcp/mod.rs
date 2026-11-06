//! Shared MCP packages. Each package actor owns its files and live connection.

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
        "An MCP with that id is already installed with different files. Remove it in Settings before replacing it."
    )]
    Conflict,
    #[error("The shared MCP library could not be read or saved.")]
    Io(#[from] std::io::Error),
    #[error(
        "The MCP connection failed. Check its setup; do not repeat a mutation whose outcome is unknown."
    )]
    Connection,
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
    fn label(&self) -> &'static str {
        match self {
            Self::Http { .. } => "http",
            Self::Stdio { .. } => "stdio",
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
    fn validate(&self) -> Result<(), Error> {
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

#[derive(Serialize)]
pub struct Package {
    pub manifest: Summary,
    pub skill: String,
    pub documentation: String,
    #[serde(skip)]
    config: Transport,
}

struct Slot {
    path: PathBuf,
    connection: Option<transport::Connection>,
    retired: bool,
    gate: Arc<crate::maintenance::Gate>,
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
}

impl Library {
    pub fn new(root: PathBuf, gate: Arc<crate::maintenance::Gate>) -> Self {
        Self {
            root,
            gate,
            slots: Actor::default(),
        }
    }

    async fn slot(&self, id: &PackageId) -> Result<Entry, Error> {
        let id = id.0.clone();
        let path = self.root.join(&id);
        let gate = self.gate.clone();
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
                            connection: None,
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
        let root = self.root.clone();
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

    pub async fn install(
        &self,
        input: Installation,
        installed_by: String,
    ) -> Result<Summary, Error> {
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
                    tokio::task::spawn_blocking(move || install(&path, &input, installed_by))
                        .await
                        .map_err(|_| Error::Connection)?
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
                    if let Some(mut connection) = slot.connection.take() {
                        connection.shutdown().await;
                    }
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
                    if slot.retired {
                        return Err(Error::NotFound);
                    }
                    if slot.connection.is_none() {
                        let path = slot.path.clone();
                        let package = tokio::task::spawn_blocking(move || read_package(&path))
                            .await
                            .map_err(|_| Error::Connection)??;
                        slot.connection = Some(
                            transport::connect(
                                &package.config,
                                &slot.path,
                                slot.path
                                    .parent()
                                    .and_then(Path::parent)
                                    .expect("MCP data directory"),
                            )
                            .await?,
                        );
                    }
                    let result = slot
                        .connection
                        .as_mut()
                        .expect("connected MCP")
                        .request(method, params)
                        .await;
                    if matches!(result, Err(Error::Connection | Error::Timeout))
                        && let Some(mut connection) = slot.connection.take()
                    {
                        connection.shutdown().await;
                    }
                    result
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

fn install(path: &Path, input: &Installation, installed_by: String) -> Result<Summary, Error> {
    let parent = path.parent().expect("package parent");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    require_dir(parent)?;
    match read_package(path) {
        Ok(existing) => {
            if existing.manifest.name == input.name
                && existing.manifest.description == input.description
                && existing.skill == input.skill
                && existing.documentation == input.documentation
                && existing.config == input.config
            {
                return Ok(existing.manifest);
            }
            return Err(Error::Conflict);
        }
        Err(Error::Io(err)) if err.kind() == ErrorKind::NotFound => {
            if std::fs::symlink_metadata(path).is_ok() {
                return Err(Error::Invalid("The existing MCP package is incomplete."));
            }
        }
        Err(err) => return Err(err),
    }
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
        std::fs::File::open(&staging)?.sync_all()?;
        std::fs::rename(&staging, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok::<_, Error>(manifest)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(staging);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn input(id: &str) -> Installation {
        Installation {
            id: PackageId::try_from(id.to_owned()).unwrap(),
            name: "T3 Code".into(),
            description: "Code and threads".into(),
            skill: "Use T3 for code tasks.".into(),
            documentation: "Read the schemas before calling a tool.".into(),
            config: Transport::Http {
                url: "http://127.0.0.1:1/mcp".into(),
                headers: BTreeMap::from([("Authorization".into(), "Bearer private-token".into())]),
            },
        }
    }

    #[tokio::test]
    async fn packages_survive_restart_and_are_private_shared_and_removable() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mcp");
        let gate = Arc::new(crate::maintenance::Gate::default());
        let library = Library::new(root.clone(), gate.clone());
        let input = input("t3-code");
        let installed = library
            .install(input.clone(), "Maple".into())
            .await
            .unwrap();
        let repeated = library
            .install(input.clone(), "Scout".into())
            .await
            .unwrap();
        assert_eq!(installed.installed_at, repeated.installed_at);
        assert_eq!(repeated.installed_by, "Maple");
        let reopened = Library::new(root.clone(), gate);
        assert_eq!(reopened.list().await.unwrap().len(), 1);
        let package = reopened.read(&input.id).await.unwrap();
        assert_eq!(package.skill, input.skill);
        assert_eq!(package.documentation, input.documentation);
        assert_eq!(package.config.label(), "http");
        let public = serde_json::to_string(&package).unwrap();
        assert!(!public.contains("private-token"));
        assert!(!public.contains("config"));
        for name in ["manifest.json", "SKILL.md", "README.md", "config.json"] {
            assert_eq!(
                std::fs::metadata(root.join("t3-code").join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let mut changed = input.clone();
        changed.skill = "Something different".into();
        assert!(matches!(
            reopened.install(changed, "Scout".into()).await,
            Err(Error::Conflict)
        ));
        reopened.remove(&input.id).await.unwrap();
        assert!(reopened.list().await.unwrap().is_empty());
        assert!(matches!(
            reopened.read(&input.id).await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            reopened
                .request(&input.id, "tools/list".into(), serde_json::json!({}))
                .await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn simultaneous_same_id_installs_publish_one_complete_package() {
        let temp = tempfile::tempdir().unwrap();
        let library = Library::new(temp.path().join("mcp"), Arc::default());
        let a = input("shared");
        let mut b = a.clone();
        b.skill = "Different instructions".into();
        let (a, b) = tokio::join!(
            library.install(a, "Maple".into()),
            library.install(b, "Scout".into())
        );
        assert_ne!(a.is_ok(), b.is_ok());
        assert_eq!(library.list().await.unwrap().len(), 1);
        let package = library.read(&input("shared").id).await.unwrap();
        assert_eq!(
            package.skill,
            if a.is_ok() {
                "Use T3 for code tasks."
            } else {
                "Different instructions"
            }
        );
    }

    #[tokio::test]
    async fn unsafe_paths_links_and_reserved_credentials_are_rejected() {
        for id in [
            "",
            "../escape",
            "/absolute",
            ".hidden",
            "UPPER",
            "id/nested",
        ] {
            assert!(PackageId::try_from(id.to_owned()).is_err());
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mcp");
        let library = Library::new(root.clone(), Arc::default());
        let mut bad = input("bad");
        bad.config = Transport::Stdio {
            command: "env".into(),
            args: vec![],
            env: BTreeMap::from([("SECRET_KEY".into(), "must-not-be-injected".into())]),
        };
        assert!(matches!(
            library.install(bad, "Maple".into()).await,
            Err(Error::Invalid(_))
        ));
        assert!(!root.exists());
        library
            .install(input("safe"), "Maple".into())
            .await
            .unwrap();
        std::fs::remove_file(root.join("safe/SKILL.md")).unwrap();
        let outside = temp.path().join("outside");
        std::fs::write(&outside, "private").unwrap();
        symlink(&outside, root.join("safe/SKILL.md")).unwrap();
        assert!(library.read(&input("safe").id).await.is_err());
        library.remove(&input("safe").id).await.unwrap();
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "private");
    }

    #[tokio::test]
    async fn nonexistent_probes_and_removed_packages_do_not_exhaust_actors() {
        let temp = tempfile::tempdir().unwrap();
        let library = Library::new(temp.path().join("mcp"), Arc::default());
        for i in 0..1030 {
            let id = PackageId::try_from(format!("missing-{i}")).unwrap();
            assert!(matches!(library.read(&id).await, Err(Error::NotFound)));
        }
        for i in 0..3 {
            let package = input(&format!("installed-{i}"));
            library
                .install(package.clone(), "Maple".into())
                .await
                .unwrap();
            library.remove(&package.id).await.unwrap();
            library
                .install(package.clone(), "Scout".into())
                .await
                .unwrap();
        }
        assert_eq!(library.list().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn discovery_ignores_stray_directories_and_can_remove_a_damaged_config() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("mcp");
        let library = Library::new(root.clone(), Arc::default());
        library
            .install(input("valid"), "Maple".into())
            .await
            .unwrap();
        std::fs::create_dir(root.join("Stray folder")).unwrap();
        std::fs::create_dir(root.join("scratch")).unwrap();
        std::fs::write(root.join("valid/config.json"), "broken").unwrap();
        assert_eq!(library.list().await.unwrap().len(), 1);
        library.remove(&input("valid").id).await.unwrap();
        assert!(library.list().await.unwrap().is_empty());
    }

    #[test]
    fn startup_cleanup_removes_only_unpublished_or_retired_package_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for prefix in [".install-", ".removed-"] {
            let dir = root.join(format!("{prefix}{}", Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("config.json"), "private credentials").unwrap();
        }
        std::fs::create_dir(root.join(".unrelated")).unwrap();
        std::fs::create_dir(root.join("installed")).unwrap();
        cleanup(root).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        names.sort();
        assert_eq!(names, vec![".unrelated", "installed"]);
    }

    #[tokio::test]
    async fn maintenance_blocks_mutation_and_execution_but_not_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let gate = Arc::new(crate::maintenance::Gate::default());
        let library = Library::new(temp.path().join("mcp"), gate.clone());
        library
            .install(input("shared"), "Maple".into())
            .await
            .unwrap();
        let _closed = gate
            .close(std::time::Duration::from_secs(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(library.list().await.unwrap().len(), 1);
        assert!(matches!(
            library.install(input("new"), "Scout".into()).await,
            Err(Error::Paused)
        ));
        assert!(matches!(
            library.remove(&input("shared").id).await,
            Err(Error::Paused)
        ));
        assert!(matches!(
            library
                .request(
                    &input("shared").id,
                    "tools/list".into(),
                    serde_json::json!({})
                )
                .await,
            Err(Error::Paused)
        ));
    }
}
