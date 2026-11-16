//! Live device pairing for providers set up by scanning a code. A pairing
//! writes into a new device store, so a failed or abandoned one never
//! touches the connection it may replace. Success seals the store's key into
//! a new connection, or into the connection being paired again, in one
//! transaction.

use super::{Manager, providers::whatsapp::Vault, store, types::*};
use serde::Serialize;
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// What a pairing links the device to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Purpose {
    /// A bot's pending setup card: success creates a connection.
    Setup(Uuid),
    /// An existing connection the account signed out: success replaces its
    /// device, if the same account linked.
    Repair(Uuid),
}

/// What the pairing card shows.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Pairing {
    /// Waiting for a code, or linking the phone that scanned one.
    Starting,
    /// Scan `code` with WhatsApp → Linked devices before `expires_in` seconds.
    Code {
        code: String,
        expires_in: u64,
    },
    /// Linked; the connection now holds the device.
    Paired {
        connection: Box<Connection>,
    },
    /// No code was scanned in time. Start again for a new one.
    Expired,
    Failed {
        message: String,
    },
    Cancelled,
}
impl Pairing {
    pub fn finished(&self) -> bool {
        !matches!(self, Self::Starting | Self::Code { .. })
    }
}

/// A pairing as the card sees it. `revision` advances on every change, so
/// a card can wait for the next one.
#[derive(Debug, Clone, Serialize)]
pub struct View {
    pub revision: u64,
    #[serde(flatten)]
    pub pairing: Pairing,
}

/// How a provider reports a pairing's progress: the codes it shows and the
/// moment a phone scanned one. The pairing itself decides how it ends.
pub struct Progress(watch::Sender<View>);
impl Progress {
    pub fn new() -> (Self, watch::Receiver<View>) {
        let (view, watched) = watch::channel(View {
            revision: 1,
            pairing: Pairing::Starting,
        });
        (Self(view), watched)
    }
    pub fn code(&self, code: String, expires_in: u64) {
        self.set(Pairing::Code { code, expires_in });
    }
    pub fn linking(&self) {
        self.set(Pairing::Starting);
    }
    fn set(&self, pairing: Pairing) {
        self.0.send_modify(|v| {
            v.revision += 1;
            v.pairing = pairing;
        });
    }
}

pub(super) struct Live {
    pub(super) view: watch::Receiver<View>,
    pub(super) stop: CancellationToken,
}

const MAX_PAIRINGS: usize = 4;
/// Finished pairings are kept for their cards to read; past this many, all
/// but linked ones are forgotten.
const MAX_KEPT: usize = 64;
/// How long a card waits for the next change before asking again.
pub const POLL: Duration = Duration::from_secs(10);

impl Manager {
    /// Start pairing for `purpose`, or return the one already running, or a
    /// setup's that already linked. Any other finished pairing is replaced,
    /// which is how the owner gets a new code.
    pub async fn start_pairing(self: &Arc<Self>, purpose: Purpose) -> Result<View, Error> {
        let provider = match purpose {
            Purpose::Setup(id) => {
                let setup = store::setup(&self.db, id).await?;
                if setup.status != store::SetupStatus::Pending {
                    return Err(Error::Inactive);
                }
                setup.provider
            }
            Purpose::Repair(id) => store::connection(&self.db, id).await?.provider,
        };
        if provider.setup() != SetupKind::Pairing {
            return Err(Error::Invalid("This provider does not pair a device."));
        }
        let manager = self.clone();
        self.pairings
            .call(move |pairings| {
                if let Some(live) = pairings.get(&purpose) {
                    let view = live.view.borrow();
                    let linked = matches!(view.pairing, Pairing::Paired { .. })
                        && matches!(purpose, Purpose::Setup(_));
                    if !view.pairing.finished() || linked {
                        return Ok(view.clone());
                    }
                }
                let running = pairings
                    .values()
                    .filter(|live| !live.view.borrow().pairing.finished())
                    .count();
                if running >= MAX_PAIRINGS {
                    return Err(Error::Busy);
                }
                if pairings.len() >= MAX_KEPT {
                    pairings.retain(|_, live| {
                        let view = live.view.borrow();
                        !view.pairing.finished() || matches!(view.pairing, Pairing::Paired { .. })
                    });
                }
                let (progress, watched) = Progress::new();
                let first = watched.borrow().clone();
                let stop = CancellationToken::new();
                pairings.insert(
                    purpose,
                    Live {
                        view: watched,
                        stop: stop.clone(),
                    },
                );
                tokio::spawn(manager.pair(purpose, provider, progress, stop));
                Ok(first)
            })
            .await
    }

    /// The pairing's view once its revision passes `after`, or the current
    /// one when [`POLL`] runs out.
    pub async fn pairing(&self, purpose: Purpose, after: u64) -> Result<View, Error> {
        if let Purpose::Setup(id) = purpose
            && store::setup(&self.db, id).await?.status != store::SetupStatus::Pending
        {
            self.pairings
                .call(move |pairings| {
                    if let Some(live) = pairings.get(&purpose) {
                        live.stop.cancel();
                    }
                })
                .await;
        }
        let mut view = self
            .pairings
            .call(move |pairings| pairings.get(&purpose).map(|live| live.view.clone()))
            .await
            .ok_or(Error::NotFound)?;
        let _ = tokio::time::timeout(POLL, view.wait_for(|v| v.revision > after)).await;
        Ok(view.borrow().clone())
    }

    /// Stop a running pairing, or forget a finished one. A stopped pairing's
    /// device store is deleted unless it already linked.
    pub async fn stop_pairing(&self, purpose: Purpose) {
        self.pairings
            .call(move |pairings| {
                if let Some(live) = pairings.get(&purpose) {
                    live.stop.cancel();
                    if live.view.borrow().pairing.finished() {
                        pairings.remove(&purpose);
                    }
                }
            })
            .await;
    }

    async fn pair(
        self: Arc<Self>,
        purpose: Purpose,
        provider: Provider,
        progress: Progress,
        stop: CancellationToken,
    ) {
        let last = match self.link(purpose, provider, &progress, stop.clone()).await {
            Ok(connection) => Pairing::Paired {
                connection: Box::new(connection),
            },
            Err(_) if stop.is_cancelled() => Pairing::Cancelled,
            Err(Error::Expired) => Pairing::Expired,
            Err(error) => {
                tracing::warn!(%error, "social pairing failed");
                Pairing::Failed {
                    message: error.to_string(),
                }
            }
        };
        progress.set(last);
    }

    async fn link(
        self: &Arc<Self>,
        purpose: Purpose,
        provider: Provider,
        progress: &Progress,
        stop: CancellationToken,
    ) -> Result<Connection, Error> {
        let device = store::new_device(&self.db).await?;
        let linked = async {
            let key = DeviceKey::generate()?;
            let vault = Vault::new(self.db.clone(), device, &key);
            let identity = self.transport.pair(&self.db, vault, progress, stop).await?;
            let secret = Secret::Device { key, store: device };
            let connection = match purpose {
                Purpose::Setup(setup) => {
                    store::create(
                        &self.db,
                        &self.key,
                        provider,
                        &identity,
                        &secret,
                        Some(setup),
                        "",
                    )
                    .await?
                }
                Purpose::Repair(id) => {
                    let c = store::connection(&self.db, id).await?;
                    if identity.id != c.remote_id {
                        return Err(Error::Invalid(
                            "A different WhatsApp account was linked. Pair the account this connection belongs to, or add it as a new connection.",
                        ));
                    }
                    store::replace(&self.db, &self.key, &c, &secret).await?;
                    store::connection(&self.db, id).await?
                }
            };
            Ok(connection)
        }
        .await;
        match linked {
            Ok(connection) => {
                self.changed(connection.id).await;
                Ok(connection)
            }
            Err(error) => {
                let _ = store::drop_device(&self.db, device).await;
                Err(error)
            }
        }
    }
}
