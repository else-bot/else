use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use else_wire::mcp::{InstalledMcp, McpAuthorization, McpConnectionStatus};
use futures_util::future::BoxFuture;
use serde_json::Value;
use uuid::Uuid;

use super::{
    Error, Installation, Package, PackageId, Replaced, Summary, TaskSecrets, Transport, install,
    installed, missing, oauth, read_file, read_package, require_dir,
};
use crate::actor::Actor;
use crate::db::task_secrets::{self, Redact};

/// A package's actor state. Every bot's operations on one package, upstream
/// calls included, run one at a time in its mailbox, because they share one
/// connection and one sign-in. A slow server delays only its own package.
enum Slot {
    Live(Box<Live>),
    /// Removed. An operation that looked up this slot before the removal
    /// finds this and reports the package missing, even after the id is
    /// installed again under a new slot.
    Retired,
}

pub(super) struct Live {
    path: PathBuf,
    /// One connection and one saved sign-in, shared by every bot.
    session: Option<oauth::Session>,
}

impl Live {
    pub(super) async fn package(&self) -> Result<Package, Error> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || read_package(&path))
            .await
            .map_err(|_| Error::Connection)?
    }

    async fn session(&mut self, config: &Config) -> Result<&mut oauth::Session, Error> {
        if self.session.is_none() {
            let store = oauth::Store::new(self.path.clone(), config.oauth_key.clone());
            self.session = Some(oauth::Session::open(store, config.oauth_client.clone()).await?);
        }
        Ok(self.session.as_mut().expect("opened MCP session"))
    }

    /// The session, once the package's files are confirmed still installed.
    pub(super) async fn installed_session(
        &mut self,
        config: &Config,
    ) -> Result<&mut oauth::Session, Error> {
        self.package().await?;
        self.session(config).await
    }

    /// The connection config with its task secrets filled in now, so a
    /// secret the owner replaced applies to the next connection, and the
    /// values to redact from what the server returns.
    async fn filled(&self, config: &Config) -> Result<(Transport, Vec<Redact>), Error> {
        let package = self.package().await?;
        TaskSecrets::fill(config.task_secrets.as_ref(), &package.config).await
    }

    async fn close(&mut self) {
        if let Some(mut session) = self.session.take() {
            session.close().await;
        }
    }
}

/// The maintenance admission an operation holds while it runs.
#[derive(Clone, Copy)]
pub(super) enum Admit {
    /// Reads files only, so it runs during a backup.
    Never,
    /// Writes: refused once a backup or update waits.
    Write,
    /// Called for running turns: allowed until a backup captures.
    Turn,
    /// Writes only when it opens the session, which may adopt older sign-in
    /// files.
    IfOpening,
}

impl Admit {
    fn enter(
        self,
        gate: &Arc<crate::maintenance::Gate>,
        live: &Live,
    ) -> Result<Option<crate::maintenance::TurnAdmission>, Error> {
        let write = || {
            gate.enter()
                .map(crate::maintenance::TurnAdmission::Open)
                .ok_or(Error::Paused)
        };
        Ok(match self {
            Self::Never => None,
            Self::IfOpening if live.session.is_some() => None,
            Self::Write | Self::IfOpening => Some(write()?),
            Self::Turn => Some(gate.enter_turn().ok_or(Error::Paused)?),
        })
    }
}

#[derive(Clone)]
pub(super) struct Entry {
    actor: Actor<Slot>,
}

impl Entry {
    pub(super) async fn live<R: Send + 'static>(
        &self,
        config: Arc<Config>,
        admit: Admit,
        f: impl for<'a> FnOnce(&'a mut Live, &'a Config) -> BoxFuture<'a, Result<R, Error>>
        + Send
        + 'static,
    ) -> Result<R, Error> {
        self.actor
            .call_async(move |slot| {
                Box::pin(async move {
                    let Slot::Live(live) = slot else {
                        return Err(Error::NotFound);
                    };
                    let _admission = admit.enter(&config.gate, live)?;
                    f(live, &config).await
                })
            })
            .await
    }

    /// Delete the package's files, retire the slot and drop it from `index`.
    /// Only this call removes an id's entry, so the entry there is this one.
    async fn retire(
        &self,
        config: Arc<Config>,
        index: Actor<HashMap<String, Entry>>,
        id: String,
    ) -> Result<(), Error> {
        self.actor
            .call_async(move |slot| {
                Box::pin(async move {
                    let Slot::Live(live) = slot else {
                        return Err(Error::NotFound);
                    };
                    let _admission = Admit::Write.enter(&config.gate, live)?;
                    live.close().await;
                    let path = live.path.clone();
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
                    *slot = Slot::Retired;
                    index.call(move |slots| slots.remove(&id)).await;
                    Ok(())
                })
            })
            .await
    }
}

/// Configuration every package actor reads and none changes.
pub struct Config {
    /// Where packages are installed.
    pub root: PathBuf,
    /// The maintenance gate writes and turn calls are admitted through.
    pub gate: Arc<crate::maintenance::Gate>,
    /// Encrypts saved sign-ins.
    pub oauth_key: Option<Arc<crate::credentials::SecretKey>>,
    pub oauth_client: Option<Arc<oauth::RegisteredClient>>,
    pub task_secrets: Option<TaskSecrets>,
}

#[derive(Clone)]
pub struct Library {
    pub(super) config: Arc<Config>,
    slots: Actor<HashMap<String, Entry>>,
}

impl Library {
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            slots: Actor::default(),
        }
    }

    pub async fn check_secrets(&self, config: &Transport) -> Result<(), Error> {
        TaskSecrets::fill(self.config.task_secrets.as_ref(), config)
            .await
            .map(drop)
    }

    async fn slot(&self, id: &PackageId) -> Result<Entry, Error> {
        let id = id.0.clone();
        let path = self.config.root.join(&id);
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
                        actor: Actor::new(Slot::Live(Box::new(Live {
                            path,
                            session: None,
                        }))),
                    })
                    .clone())
            })
            .await
    }

    pub(super) async fn installed_slot(&self, id: &PackageId) -> Result<Entry, Error> {
        let metadata = tokio::fs::symlink_metadata(self.config.root.join(&id.0))
            .await
            .map_err(Error::from)
            .map_err(missing)?;
        if !metadata.is_dir() {
            return Err(Error::Invalid("Invalid MCP package directory."));
        }
        self.slot(id).await
    }

    /// Run `f` in the actor of the installed package `id`.
    pub(super) async fn on_installed<R: Send + 'static>(
        &self,
        id: &PackageId,
        admit: Admit,
        f: impl for<'a> FnOnce(&'a mut Live, &'a Config) -> BoxFuture<'a, Result<R, Error>>
        + Send
        + 'static,
    ) -> Result<R, Error> {
        self.installed_slot(id)
            .await?
            .live(self.config.clone(), admit, f)
            .await
            .map_err(missing)
    }

    pub async fn list(&self) -> Result<Vec<Summary>, Error> {
        installed(self.config.root.clone()).await
    }

    pub async fn listings(&self) -> Result<Vec<InstalledMcp>, Error> {
        let manifests = self.list().await?;
        let root = self.config.root.clone();
        tokio::task::spawn_blocking(move || {
            manifests
                .into_iter()
                .map(|manifest| {
                    let website = read_file(&root.join(&manifest.id.0).join("config.json"))
                        .ok()
                        .and_then(|text| serde_json::from_str::<Transport>(&text).ok())
                        .and_then(|config| config.website());
                    InstalledMcp {
                        website,
                        ..InstalledMcp::from(&manifest)
                    }
                })
                .collect()
        })
        .await
        .map_err(|_| Error::Connection)
    }

    pub async fn read(&self, id: &PackageId) -> Result<Package, Error> {
        self.on_installed(id, Admit::Never, |live, _| Box::pin(live.package()))
            .await
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
        self.slot(&input.id)
            .await?
            .live(self.config.clone(), Admit::Write, move |live, _| {
                Box::pin(async move {
                    let path = live.path.clone();
                    let published = tokio::task::spawn_blocking(move || {
                        install(&path, &input, installed_by, replace)
                    })
                    .await
                    .map_err(|_| Error::Connection)
                    .flatten();
                    // A failed replacement may still have published the new config,
                    // so no session that holds the old sign-in survives it.
                    if published.as_ref().map_or(replace, |p| p.reconnect) {
                        live.close().await;
                    }
                    published
                })
            })
            .await
    }

    pub async fn remove(&self, id: &PackageId) -> Result<(), Error> {
        self.installed_slot(id)
            .await?
            .retire(self.config.clone(), self.slots.clone(), id.0.clone())
            .await
            .map_err(missing)
    }

    pub async fn request(
        &self,
        id: &PackageId,
        method: String,
        params: Value,
    ) -> Result<Value, Error> {
        self.on_installed(id, Admit::Turn, |live, config| {
            Box::pin(async move {
                let (transport, secrets) = live.filled(config).await?;
                let session = live.session(config).await?;
                let result = session.request(&transport, method, params).await?;
                Ok(if secrets.is_empty() {
                    result
                } else {
                    task_secrets::redact_json(result, &secrets)
                })
            })
        })
        .await
    }

    pub async fn git_token(&self, id: &PackageId) -> Result<zeroize::Zeroizing<String>, Error> {
        self.on_installed(id, Admit::Turn, |live, config| {
            Box::pin(async move {
                let (transport, _) = live.filled(config).await?;
                live.session(config).await?.git_token(&transport).await
            })
        })
        .await
    }

    pub async fn connection_status(&self, id: &PackageId) -> Result<McpConnectionStatus, Error> {
        self.on_installed(id, Admit::IfOpening, |live, config| {
            Box::pin(async move {
                let session = live.installed_session(config).await?;
                Ok(session.status())
            })
        })
        .await
    }

    pub async fn authorize(
        &self,
        id: &PackageId,
        redirect_uri: String,
    ) -> Result<McpAuthorization, Error> {
        self.on_installed(id, Admit::Write, |live, config| {
            Box::pin(async move {
                let (transport, _) = live.filled(config).await?;
                let session = live.session(config).await?;
                session.authorize(&transport, redirect_uri).await
            })
        })
        .await
    }

    pub async fn complete_authorization(
        &self,
        id: &PackageId,
        callback: oauth::Complete,
    ) -> Result<(), Error> {
        self.on_installed(id, Admit::Write, |live, config| {
            Box::pin(async move {
                let session = live.installed_session(config).await?;
                session.complete(callback).await
            })
        })
        .await
    }

    pub async fn cancel_authorization(
        &self,
        id: &PackageId,
        request_id: Uuid,
    ) -> Result<(), Error> {
        self.on_installed(id, Admit::Write, move |live, config| {
            Box::pin(async move {
                let session = live.installed_session(config).await?;
                session.cancel(request_id);
                Ok(())
            })
        })
        .await
    }

    pub async fn cancel_pending_authorization(
        &self,
        id: &PackageId,
    ) -> Result<McpConnectionStatus, Error> {
        self.on_installed(id, Admit::Write, |live, config| {
            Box::pin(async move {
                let session = live.installed_session(config).await?;
                Ok(session.cancel_pending())
            })
        })
        .await
    }

    pub async fn disconnect(&self, id: &PackageId) -> Result<(), Error> {
        self.on_installed(id, Admit::Write, |live, config| {
            Box::pin(async move {
                let session = live.installed_session(config).await?;
                session.disconnect().await
            })
        })
        .await
    }
}
