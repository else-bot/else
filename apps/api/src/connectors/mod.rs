mod cache;
mod config;
pub mod delivery;
pub mod documents;
pub mod oauth;
pub mod pimalaya;
mod readers;
pub mod store;
pub mod types;

use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::connector_core::actions::{Outcome, Start};
use crate::connector_core::workers::{Inbox, Worker, Workers};
use crate::{actor::Actor, credentials::SecretKey, scheduler::Scheduler};
use store::Whereabouts;
use types::*;

pub fn sensitive_log_target(target: &str) -> bool {
    [
        "io_imap",
        "io_smtp",
        "io_sasl",
        "imap_codec",
        "imap_types",
        "pimalaya_stream",
        "reqwest",
        "teloxide_core",
        "twilight_http",
        "twilight_gateway",
        "hyper",
        "hyper_util",
        "h2",
        "rustls",
        "tokio_rustls",
        "rmcp",
    ]
    .iter()
    .any(|prefix| {
        target == *prefix
            || target
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with("::"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn protocol_credentials_cannot_be_enabled_by_trace_directives() {
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(
                "trace,io_imap::client=trace,io_smtp::client=trace",
            ))
            .with(tracing_subscriber::filter::filter_fn(|meta| {
                !sensitive_log_target(meta.target())
            }))
            .with(
                tracing_subscriber::fmt::layer()
                    .without_time()
                    .with_ansi(false)
                    .with_writer(move || writer.clone()),
            );
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(target:"io_imap::client", "SASL private-app-password");
            tracing::trace!(target:"io_smtp::client", "private message body");
            tracing::trace!(target:"reqwest::body", "private refresh token");
            tracing::info!(target:"else_api::connectors", "safe health update");
        });
        let bytes = capture.0.lock().unwrap();
        let logged = String::from_utf8_lossy(&bytes);
        assert!(logged.contains("safe health update"));
        assert!(!logged.contains("private"));
    }

    #[test]
    fn reconnect_delays_grow_and_stay_bounded() {
        assert_eq!(retry_delay(1), Duration::from_secs(10));
        assert_eq!(retry_delay(2), Duration::from_secs(20));
        assert_eq!(retry_delay(5), Duration::from_secs(160));
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(300));
    }
}

/// How a mutating mail command that failed settles, logged at warn with
/// its phase and reason. A send that stopped before the server had the
/// whole message sent nothing; any other failed change may have happened.
fn failure_receipt(account_id: Uuid, action_id: Uuid, error: &MailError) -> (Outcome, Value) {
    let reason = error.reason();
    let phase = error.phase.map(SendPhase::as_str);
    tracing::warn!(
        %account_id,
        %action_id,
        phase,
        code = error.code.as_str(),
        %reason,
        "mail action failed"
    );
    if error.phase.is_some() && !error.may_have_sent() {
        (
            Outcome::Failed,
            json!({"error":error.code,"phase":phase,"reason":reason,"instruction":"Nothing was sent. The owner may approve sending it again."}),
        )
    } else {
        (
            Outcome::Uncertain,
            json!({"error":error.code,"phase":phase,"reason":reason,"instruction":"Inspect the mailbox before starting a different action. This action will not be retried."}),
        )
    }
}

/// Who asks for a mail command: a bot's running turn, limited by the
/// bot's grant on the account. The owner's reader never queues commands.
#[derive(Clone, Copy)]
enum Caller {
    Bot { bot_id: Uuid, turn_id: Uuid },
}

struct Command {
    caller: Caller,
    action_id: Option<Uuid>,
    command: MailCommand,
    queued: tokio::time::Instant,
    deadline: tokio::time::Instant,
    reply: oneshot::Sender<Result<Value, MailError>>,
}
enum Request {
    Mail(Command),
    Credential {
        credential: Credential,
        reply: oneshot::Sender<Result<(), MailError>>,
    },
}

pub struct ConnectorManager {
    pub db: sqlx::SqlitePool,
    pub key: Arc<SecretKey>,
    scheduler: Arc<Scheduler>,
    transport: Arc<dyn MailTransport>,
    workers: Workers<Request>,
    messages: Actor<cache::Messages>,
    readers: readers::Readers,
    /// One sign-in refresh at a time per account, so the owner's reader
    /// and the worker never spend the same OAuth refresh token twice.
    refreshes: Actor<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>,
    pub oauth: oauth::OAuth,
}

impl ConnectorManager {
    pub fn new(db: sqlx::SqlitePool, key: Arc<SecretKey>, scheduler: Arc<Scheduler>) -> Self {
        Self::with_transport(db, key, scheduler, Arc::new(pimalaya::Pimalaya))
    }
    pub fn with_transport(
        db: sqlx::SqlitePool,
        key: Arc<SecretKey>,
        scheduler: Arc<Scheduler>,
        transport: Arc<dyn MailTransport>,
    ) -> Self {
        Self {
            db,
            key,
            scheduler,
            transport,
            workers: Workers::default(),
            messages: Actor::default(),
            readers: readers::Readers::default(),
            refreshes: Actor::default(),
            oauth: oauth::OAuth::default(),
        }
    }

    async fn worker(self: &Arc<Self>, id: Uuid) -> Result<Worker<Request>, MailError> {
        self.worker_or_start(id).await.map(|(worker, _)| worker)
    }

    /// The account's worker, and whether this call started it.
    async fn worker_or_start(
        self: &Arc<Self>,
        id: Uuid,
    ) -> Result<(Worker<Request>, bool), MailError> {
        let manager = self.clone();
        self.workers
            .get_or_spawn(id, move |inbox| manager.account_loop(id, inbox))
            .await
            .map_err(|_| MailError::new(MailErrorCode::ConnectorLimit))
    }

    /// Drops what the owner's reader kept for the account, so a paused,
    /// reconfigured or rejected inbox serves nothing from before.
    async fn drop_reads(&self, id: Uuid, removed: bool) {
        self.messages
            .call(move |messages| messages.forget(id))
            .await;
        self.readers.close(id, removed).await;
        if removed {
            self.refreshes.call(move |locks| locks.remove(&id)).await;
        }
    }

    pub async fn changed(self: &Arc<Self>, id: Uuid) {
        self.drop_reads(id, false).await;
        // A worker this call starts reads the changed account itself; a
        // notification would restart its first watch for nothing.
        if let Ok((worker, false)) = self.worker_or_start(id).await {
            worker.changed();
        }
    }

    pub async fn forget(&self, id: Uuid) {
        self.workers.stop(id).await;
        self.drop_reads(id, true).await;
    }

    pub async fn replace_credential(
        self: &Arc<Self>,
        id: Uuid,
        credential: Credential,
    ) -> Result<(), MailError> {
        let worker = self.worker(id).await?;
        let (reply, result) = oneshot::channel();
        worker
            .try_send(Request::Credential { credential, reply })
            .map_err(|_| MailError::new(MailErrorCode::MailAccountBusy))?;
        tokio::time::timeout(Duration::from_secs(120), result)
            .await
            .map_err(|_| MailError::new(MailErrorCode::MailAccountBusy))?
            .map_err(|_| MailError::new(MailErrorCode::MailAccountUnavailable))?
    }

    /// Signs in with an owner's new password before it is saved. An OAuth
    /// credential comes from a completed sign-in and needs no check.
    pub async fn verify(
        &self,
        config: &MailConfig,
        credential: &Credential,
    ) -> Result<(), MailError> {
        let Credential::Password { password } = credential else {
            return Ok(());
        };
        let auth = Auth {
            username: config.username.clone(),
            secret: zeroize::Zeroizing::new(password.clone()),
            oauth: false,
        };
        self.transport.verify(config, &auth).await.map_err(|error| {
            match (error.code, config.provider) {
                (MailErrorCode::MailAuthenticationFailed, Provider::Icloud) => error.detail(
                    "Apple rejected the password. Use an app-specific password, not your Apple \
                     Account password.",
                ),
                (MailErrorCode::MailAuthenticationFailed, _) => error.detail(
                    "The mail server rejected the username or password. Check them, or use an \
                     app password from your provider.",
                ),
                _ => MailError::new(MailErrorCode::MailSignInFailed).detail(
                    "Could not sign in to the mail server. Check your connection and the server \
                     settings, then try again.",
                ),
            }
        })
    }

    async fn save_owner_credential(
        &self,
        id: Uuid,
        credential: Credential,
    ) -> Result<(), MailError> {
        let _admission = self
            .scheduler
            .gate()
            .enter()
            .ok_or_else(|| MailError::new(MailErrorCode::MaintenanceInProgress))?;
        let account = store::account(&self.db, id)
            .await?
            .ok_or_else(|| MailError::new(MailErrorCode::MailAccountUnavailable))?;
        credential.validate(account.config.provider)?;
        if !store::replace_credential(&self.db, &self.key, id, &credential).await? {
            return Err(MailError::new(MailErrorCode::MailAccountChanged));
        }
        Ok(())
    }

    pub async fn execute(
        self: &Arc<Self>,
        account_id: Uuid,
        bot_id: Uuid,
        turn_id: Uuid,
        action_id: Option<Uuid>,
        command: MailCommand,
    ) -> Result<Value, MailError> {
        self.submit(
            account_id,
            Caller::Bot { bot_id, turn_id },
            action_id,
            command,
        )
        .await
    }

    /// One message for the owner's reader. It never marks the message read
    /// and never waits behind the account's queued commands: a message read
    /// recently comes from the cache, and any other on the owner's own
    /// session. `message` is where a mail trigger's message arrived; the
    /// reader follows it to where it went, and searches for it by
    /// Message-ID when it left without saying where.
    pub async fn view(
        self: &Arc<Self>,
        account_id: Uuid,
        message: MessageRef,
    ) -> Result<Value, MailError> {
        MailCommand::View {
            message: message.clone(),
        }
        .validate()?;
        // Before the account loads, so a pause that lands after this read
        // saw the inbox active keeps the read from storing anything.
        let epoch = self
            .messages
            .call(move |messages| messages.epoch(account_id))
            .await;
        let account = store::account(&self.db, account_id)
            .await?
            .ok_or_else(|| MailError::new(MailErrorCode::MailForbidden))?;
        if account.state != AccountState::Active {
            return Err(MailError::new(MailErrorCode::MailAccountPaused));
        }
        let _admission = self
            .scheduler
            .gate()
            .enter()
            .ok_or_else(|| MailError::new(MailErrorCode::MaintenanceInProgress))?;
        let whereabouts = store::whereabouts(&self.db, account_id, &message).await?;
        let at = match whereabouts.as_ref().map(|w| &w.place) {
            Some(Place::Deleted) => return Err(MailError::new(MailErrorCode::MessageDeleted)),
            Some(Place::Moved { message, .. }) => message.clone(),
            Some(Place::Arrived) | None => Some(message.clone()),
        };
        let error = match at {
            Some(at) => match self.read_for_owner(&account, &at, epoch).await {
                Ok(fetched) => {
                    if let Some(w) = whereabouts.as_ref().filter(|w| w.message_id.is_none()) {
                        let _ =
                            store::remember(&self.db, w.event_id, &pimalaya::envelope(&fetched))
                                .await;
                    }
                    return pimalaya::view(&fetched, &at);
                }
                Err(error) => error,
            },
            None => MailError::new(MailErrorCode::MessageNotFound),
        };
        let Some(Whereabouts {
            event_id,
            message_id: Some(message_id),
            place,
        }) = whereabouts.filter(|_| {
            matches!(
                error.code,
                MailErrorCode::MessageNotFound | MailErrorCode::StaleMessageReference
            )
        })
        else {
            return Err(error);
        };
        let folder = match &place {
            Place::Moved { folder, .. } => Some(folder.clone()),
            Place::Arrived | Place::Deleted => None,
        };
        let auth = self.auth(&account).await?;
        let located = self
            .transport
            .execute(
                &account.config,
                &auth,
                &MailCommand::Locate { message_id, folder },
            )
            .await?;
        let found = serde_json::from_value::<Option<MessageRef>>(located["message"].clone())
            .map_err(|_| MailError::new(MailErrorCode::MailTransportFailed))?;
        let now = match &found {
            Some(found) => Place::Moved {
                kind: MoveKind::into_folder(&account.config, &found.folder),
                folder: found.folder.clone(),
                message: Some(found.clone()),
            },
            None => Place::Deleted,
        };
        let recorded = store::found(&self.db, event_id, &place, &now).await?;
        // A bot moved it while the search ran: its record wins.
        let now = if recorded {
            now
        } else {
            store::whereabouts(&self.db, account_id, &message)
                .await
                .ok()
                .flatten()
                .map_or(Place::Arrived, |w| w.place)
        };
        let found = match now {
            Place::Deleted => return Err(MailError::new(MailErrorCode::MessageDeleted)),
            Place::Moved {
                message: Some(found),
                ..
            } => found,
            Place::Moved { message: None, .. } | Place::Arrived => return Err(error),
        };
        let fetched = self.read_for_owner(&account, &found, epoch).await?;
        pimalaya::view(&fetched, &found)
    }

    /// The message at `message`, from the cache or the owner's session,
    /// cached for the next read unless the inbox paused since `epoch`.
    async fn read_for_owner(
        self: &Arc<Self>,
        account: &Account,
        message: &MessageRef,
        epoch: u64,
    ) -> Result<Fetched, MailError> {
        let started = tokio::time::Instant::now();
        let account_id = account.id;
        let ms = |at: Duration| at.as_millis() as u64;
        if let Some(fetched) = self.cached(account, message).await {
            tracing::info!(
                %account_id,
                path = "cached",
                total_ms = ms(started.elapsed()),
                "owner viewed a message"
            );
            return Ok(fetched);
        }
        let signed_in = std::sync::Mutex::new(None);
        let read = self.readers.read(account.id, message, || async {
            let at = tokio::time::Instant::now();
            let auth = self.auth(account).await?;
            let session = self.transport.reader(&account.config, &auth).await;
            *signed_in.lock().expect("unpoisoned") = Some(at.elapsed());
            session
        });
        let (fetched, path) = tokio::time::timeout(Duration::from_secs(120), read)
            .await
            .unwrap_or((
                Err(MailError::new(MailErrorCode::MailTimeout)),
                readers::Path::Cold,
            ));
        let signed_in = signed_in.into_inner().expect("unpoisoned");
        tracing::info!(
            %account_id,
            path = path.as_str(),
            sign_in_ms = signed_in.map(ms),
            total_ms = ms(started.elapsed()),
            "owner viewed a message"
        );
        let fetched = match fetched {
            Ok(fetched) => fetched,
            Err(error) => {
                if error.code.rejects_credential() {
                    self.health(account.id, account.generation, error.code)
                        .await;
                    // The watch signed in before the rejection; restart it
                    // so it stops reporting new mail on that sign-in.
                    self.changed(account.id).await;
                }
                return Err(error);
            }
        };
        let key = Self::cache_key(account, message);
        let stored = fetched.clone();
        self.messages.call(move |m| m.put(key, stored, epoch)).await;
        Ok(fetched)
    }

    fn cache_key(account: &Account, message: &MessageRef) -> cache::Key {
        cache::Key {
            account_id: account.id,
            generation: account.generation,
            message: message.clone(),
        }
    }

    /// The cached message, unless the server is known to reject the saved
    /// sign-in, so the reader still says to reconnect.
    async fn cached(&self, account: &Account, message: &MessageRef) -> Option<Fetched> {
        if matches!(
            account.health,
            Health::CredentialRejected | Health::ReauthenticationRequired
        ) {
            return None;
        }
        let key = Self::cache_key(account, message);
        self.messages.call(move |messages| messages.get(&key)).await
    }

    async fn submit(
        self: &Arc<Self>,
        account_id: Uuid,
        caller: Caller,
        action_id: Option<Uuid>,
        command: MailCommand,
    ) -> Result<Value, MailError> {
        command.validate()?;
        let mutates = command.mutates();
        if mutates && action_id.is_none() {
            return Err(MailError::new(MailErrorCode::ActionIdRequired));
        }
        let (reply, result) = oneshot::channel();
        let queued = tokio::time::Instant::now();
        let command = Command {
            caller,
            action_id,
            command,
            queued,
            deadline: queued + Duration::from_secs(120),
            reply,
        };
        let account = store::account(&self.db, account_id)
            .await?
            .ok_or_else(|| MailError::new(MailErrorCode::MailForbidden))?;
        self.authorization(&account, &command).await?;
        let worker = self.worker(account_id).await?;
        worker
            .try_send(Request::Mail(command))
            .map_err(|_| MailError::new(MailErrorCode::MailAccountBusy))?;
        match tokio::time::timeout(Duration::from_secs(120), result).await {
            Ok(result) => {
                result.map_err(|_| MailError::new(MailErrorCode::MailAccountUnavailable))?
            }
            Err(_) if mutates => Ok(
                json!({"action_id":action_id,"status":"pending","instruction":"Read mail_action_status or retry the same action ID. Do not create another action ID."}),
            ),
            Err(_) => Err(MailError::new(MailErrorCode::MailAccountBusy)),
        }
    }

    /// Starts a worker for each active account that has none. Startup awaits
    /// the first pass, so it never races an account created afterwards.
    pub(crate) async fn start_workers(self: &Arc<Self>) {
        if let Ok(accounts) = store::accounts(&self.db).await {
            for account in accounts {
                if account.state == AccountState::Active {
                    let _ = self.worker(account.id).await;
                }
            }
        }
    }

    pub async fn run(self: Arc<Self>, stop: CancellationToken) {
        loop {
            tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(30)) => {} }
            self.start_workers().await;
        }
        self.workers.shutdown().await;
    }

    async fn authorization(&self, account: &Account, command: &Command) -> Result<(), MailError> {
        match command.caller {
            Caller::Bot { bot_id, turn_id } => Ok(store::authorize(
                &self.db,
                account.id,
                account.generation,
                bot_id,
                turn_id,
                &command.command,
            )
            .await?),
        }
    }

    async fn auth(&self, account: &Account) -> Result<Auth, MailError> {
        let id = account.id;
        let refresh = self
            .refreshes
            .call(move |locks| locks.entry(id).or_default().clone())
            .await;
        let _refresh = refresh.lock().await;
        let _admission = self
            .scheduler
            .gate()
            .enter()
            .ok_or_else(|| MailError::new(MailErrorCode::MaintenanceInProgress))?;
        let mut credential = store::credential(&self.db, &self.key, account.id).await?;
        if oauth::refresh(account.config.provider, &mut credential).await?
            && !store::save_credential(
                &self.db,
                &self.key,
                account.id,
                account.generation,
                &credential,
            )
            .await?
        {
            return Err(MailError::new(MailErrorCode::MailAccountChanged));
        }
        let (secret, oauth) = match &credential {
            Credential::Password { password } => (password.clone(), false),
            Credential::OAuth2 { access_token, .. } => (
                access_token
                    .clone()
                    .ok_or_else(|| MailError::new(MailErrorCode::OauthReauthenticationRequired))?,
                true,
            ),
        };
        Ok(Auth {
            username: account.config.username.clone(),
            secret: zeroize::Zeroizing::new(secret),
            oauth,
        })
    }

    /// Runs one command. When the server rejects the credential or the
    /// OAuth grant expired, it stops the watch session and records that
    /// before it replies, so a session that signed in earlier cannot report
    /// the inbox connected, and the bot's next call sees it. It returns
    /// whether the watch must sign in again.
    async fn dispatch(&self, id: Uuid, command: Command, watching: &mut Option<Watching>) -> bool {
        let mut rejected = None;
        let result = self.perform(id, &command, &mut rejected).await;
        if let Some((generation, health)) = rejected {
            stop_watch(watching).await;
            self.health(id, generation, health).await;
        }
        let _ = command.reply.send(result);
        rejected.is_some()
    }

    async fn perform(
        &self,
        id: Uuid,
        command: &Command,
        rejected: &mut Option<(i64, MailErrorCode)>,
    ) -> Result<Value, MailError> {
        if tokio::time::Instant::now() >= command.deadline {
            return Err(MailError::new(MailErrorCode::MailQueueExpired));
        }
        let _admission = self
            .scheduler
            .gate()
            .enter()
            .ok_or_else(|| MailError::new(MailErrorCode::MaintenanceInProgress))?;
        let account = store::account(&self.db, id)
            .await?
            .ok_or(store::StoreError::NotFound)?;
        // Before signing in, so a command that lost its access while queued
        // refreshes no credential and records no health.
        self.authorization(&account, command).await?;
        let started = tokio::time::Instant::now();
        let auth = self.auth(&account).await.inspect_err(|error| {
            if error.code == MailErrorCode::OauthReauthenticationRequired {
                *rejected = Some((
                    account.generation,
                    MailErrorCode::OauthReauthenticationRequired,
                ));
            }
        })?;
        let action_id = match (command.command.mutates(), command.caller) {
            (false, _) => {
                self.authorization(&account, command).await?;
                None
            }
            (true, Caller::Bot { bot_id, turn_id }) => {
                let action_id = command.action_id.expect("validated action ID");
                match store::begin_action(
                    &self.db,
                    id,
                    account.generation,
                    bot_id,
                    turn_id,
                    action_id,
                    &command.command,
                )
                .await?
                {
                    Start::Existing(receipt) => return Ok(receipt),
                    Start::Dispatch => Some(action_id),
                }
            }
        };
        // A bot always reads the mailbox itself, so a message moved or
        // deleted elsewhere is gone for it at once; its read fills the
        // owner's cache.
        let result = match &command.command {
            MailCommand::Read { message } => {
                let epoch = self.messages.call(move |m| m.epoch(id)).await;
                let signed_in = started.elapsed();
                let fetched = self.transport.fetch(&account.config, &auth, message).await;
                log_read(id, command, started, signed_in);
                match fetched {
                    Ok(fetched) => {
                        let key = Self::cache_key(&account, message);
                        let stored = fetched.clone();
                        self.messages.call(move |m| m.put(key, stored, epoch)).await;
                        pimalaya::read(&fetched, message)
                    }
                    Err(error) => Err(error),
                }
            }
            _ => {
                self.transport
                    .execute(&account.config, &auth, &command.command)
                    .await
            }
        };
        if matches!(&result, Err(error) if error.code == MailErrorCode::MailAuthenticationFailed) {
            *rejected = Some((account.generation, MailErrorCode::MailAuthenticationFailed));
        }
        if let (
            Ok(value),
            MailCommand::Move { message, .. }
            | MailCommand::Archive { message }
            | MailCommand::Trash { message }
            | MailCommand::Delete { message },
        ) = (&result, &command.command)
        {
            let key = Self::cache_key(&account, message);
            self.messages.call(move |m| m.remove(&key)).await;
            let Caller::Bot { turn_id, .. } = command.caller;
            let to = destination(&account.config, &command.command, value);
            if let Err(error) = store::relocate(&self.db, id, turn_id, message, &to).await {
                tracing::warn!(account_id = %id, ?error, "could not record where a bot moved mail");
            }
        }
        let Some(action_id) = action_id else {
            return result;
        };
        let (outcome, fields) = match result {
            Ok(value) => (Outcome::Succeeded, json!({ "result": value })),
            Err(error) => failure_receipt(id, action_id, &error),
        };
        store::ACTIONS
            .finish(&self.db, id, action_id, outcome, fields)
            .await
            .map_err(|_| MailError::new(MailErrorCode::MailReceiptUnavailable))
    }

    async fn prepare_watch(&self, id: Uuid) -> Result<(Account, Auth), MailError> {
        let account = store::account(&self.db, id)
            .await?
            .ok_or_else(|| MailError::new(MailErrorCode::MailAccountUnavailable))?;
        if account.state != AccountState::Active {
            return Err(MailError::new(MailErrorCode::MailAccountPaused));
        }
        let auth = self.auth(&account).await?;
        Ok((account, auth))
    }

    async fn watch(
        &self,
        account: &Account,
        auth: &Auth,
        stop: CancellationToken,
    ) -> Result<(), MailError> {
        let id = account.id;
        let since = store::health_version(&self.db, id)
            .await
            .map_err(|_| MailError::new(MailErrorCode::MailAccountUnavailable))?;
        let cursor = loop {
            let old = store::cursor(&self.db, id).await?;
            // Only the server waits stop early. A database write that began
            // finishes, so a stopped watch leaves no late health behind.
            let scan = tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                scan = self.transport.scan(&account.config, auth, old.as_ref()) => scan?,
            };
            let more = scan.more || scan.arrivals.len() > store::MAX_ARRIVALS;
            {
                let _admission = self
                    .scheduler
                    .gate()
                    .enter()
                    .ok_or_else(|| MailError::new(MailErrorCode::MaintenanceInProgress))?;
                if !store::record_scan(&self.db, id, account.generation, old.as_ref(), &scan)
                    .await?
                {
                    return Err(MailError::new(MailErrorCode::MailAccountChanged));
                }
            }
            self.scheduler.wake();
            if !more {
                break scan.cursor;
            }
        };
        if let Some(_admission) = self.scheduler.gate().enter() {
            let _ = store::mark_connected(&self.db, id, account.generation, since).await;
        }
        self.transport
            .wait(&account.config, auth, &cursor, stop)
            .await
    }

    async fn health(&self, id: Uuid, generation: i64, code: MailErrorCode) {
        if code == MailErrorCode::MailAccountPaused {
            return;
        }
        if code.rejects_credential() {
            self.drop_reads(id, false).await;
        }
        if let Some(_admission) = self.scheduler.gate().enter() {
            let health = Health::from(code);
            let _ = store::set_health(&self.db, id, generation, &health).await;
        }
    }

    async fn account_loop(self: Arc<Self>, id: Uuid, inbox: Inbox<Request>) {
        let Inbox {
            requests: mut commands,
            mut changed,
            stop,
        } = inbox;
        let mut failures = 0u32;
        let mut retry_at = tokio::time::Instant::now();
        let mut watching: Option<Watching> = None;
        loop {
            if stop.is_cancelled() {
                break;
            }
            if watching.is_none() && tokio::time::Instant::now() >= retry_at {
                match self.prepare_watch(id).await {
                    Ok((account, auth)) => {
                        let manager = self.clone();
                        let watch_stop = stop.child_token();
                        let task_stop = watch_stop.clone();
                        let generation = account.generation;
                        let task =
                            tokio::spawn(
                                async move { manager.watch(&account, &auth, task_stop).await },
                            );
                        watching = Some(Watching {
                            task,
                            stop: watch_stop,
                            generation,
                        });
                    }
                    Err(error) => {
                        if let Ok(Some(account)) = store::account(&self.db, id).await
                            && account.state == AccountState::Active
                        {
                            self.health(id, account.generation, error.code).await;
                        }
                        failures = failures.saturating_add(1);
                        retry_at = tokio::time::Instant::now() + retry_delay(failures);
                    }
                }
            }
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                command = commands.recv() => {
                    let Some(command) = command else { break; };
                    match command {
                        Request::Mail(command) => {
                            if self.dispatch(id,command,&mut watching).await {
                                retry_at = tokio::time::Instant::now();
                            }
                        }
                        Request::Credential { credential,reply } => {
                            stop_watch(&mut watching).await;
                            let result = self.save_owner_credential(id,credential).await;
                            let _ = reply.send(result);
                            failures = 0;
                            retry_at = tokio::time::Instant::now();
                        }
                    }
                }
                result = changed.changed() => {
                    if result.is_err() { break; }
                    stop_watch(&mut watching).await;
                    failures = 0;
                    retry_at = tokio::time::Instant::now();
                }
                result = async { (&mut watching.as_mut().expect("active watch").task).await }, if watching.is_some() => {
                    let completed = watching.take().expect("completed watch");
                    match result {
                        Ok(Ok(())) => { failures = 0; retry_at = tokio::time::Instant::now(); }
                        result => {
                            let code = match result { Ok(Err(error)) => error.code, _ => MailErrorCode::MailWatchFailed };
                            self.health(id,completed.generation,code).await;
                            failures = failures.saturating_add(1);
                            retry_at = tokio::time::Instant::now() + retry_delay(failures);
                        }
                    }
                }
                _ = tokio::time::sleep_until(retry_at), if watching.is_none() => {}
            }
        }
        stop_watch(&mut watching).await;
    }
}

/// Where a successful move or delete left its message, from the server's
/// reply. A move whose reply names no UID leaves only the folder known.
fn destination(config: &MailConfig, command: &MailCommand, reply: &Value) -> Place {
    let folder = match command {
        MailCommand::Move { folder, .. } => folder.clone(),
        MailCommand::Archive { .. } => config.archive_folder.clone(),
        MailCommand::Trash { .. } => config.trash_folder.clone(),
        _ => return Place::Deleted,
    };
    let message = serde_json::from_value::<MessageRef>(reply["message"].clone())
        .ok()
        .filter(|m| m.folder == folder);
    Place::Moved {
        kind: MoveKind::into_folder(config, &folder),
        folder,
        message,
    }
}

/// Where a bot's read spent its time: waiting in the account's queue,
/// refreshing the sign-in, and connecting and fetching the message.
fn log_read(account_id: Uuid, command: &Command, started: tokio::time::Instant, auth: Duration) {
    let ms = |at: Duration| at.as_millis() as u64;
    tracing::info!(
        %account_id,
        queue_ms = ms(started - command.queued),
        auth_ms = ms(auth),
        fetch_ms = ms(started.elapsed() - auth),
        "bot read a message"
    );
}

struct Watching {
    task: tokio::task::JoinHandle<Result<(), MailError>>,
    stop: CancellationToken,
    generation: i64,
}
async fn stop_watch(watching: &mut Option<Watching>) {
    if let Some(watching) = watching.take() {
        watching.stop.cancel();
        let _ = watching.task.await;
    }
}
fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs((5u64 << failures.min(6)).min(300))
}
