mod config;
pub mod delivery;
pub mod oauth;
pub mod pimalaya;
pub mod store;
pub mod types;

use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{actor::Actor, credentials::SecretKey, scheduler::Scheduler};
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
        "hyper",
        "hyper_util",
        "h2",
        "rustls",
        "tokio_rustls",
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

struct Command {
    bot_id: Uuid,
    turn_id: Uuid,
    action_id: Option<Uuid>,
    command: MailCommand,
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
#[derive(Clone)]
struct Worker {
    commands: mpsc::Sender<Request>,
    changed: Arc<Notify>,
    stop: CancellationToken,
    done: tokio::sync::watch::Receiver<bool>,
}

pub struct ConnectorManager {
    pub db: sqlx::SqlitePool,
    pub key: Arc<SecretKey>,
    scheduler: Arc<Scheduler>,
    transport: Arc<dyn MailTransport>,
    workers: Actor<HashMap<Uuid, Worker>>,
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
            workers: Actor::default(),
            oauth: oauth::OAuth::default(),
        }
    }

    async fn worker(self: &Arc<Self>, id: Uuid) -> Result<Worker, MailError> {
        let manager = self.clone();
        self.workers
            .call(move |workers| {
                if let Some(worker) = workers.get(&id) {
                    return Ok(worker.clone());
                }
                if workers.len() >= 32 {
                    return Err(MailError::new("connector_limit"));
                }
                let (commands, receiver) = mpsc::channel(16);
                let (done, completed) = tokio::sync::watch::channel(false);
                let worker = Worker {
                    commands,
                    changed: Arc::new(Notify::new()),
                    stop: CancellationToken::new(),
                    done: completed,
                };
                workers.insert(id, worker.clone());
                let running = worker.clone();
                tokio::spawn(async move {
                    manager.account_loop(id, running, receiver).await;
                    let _ = done.send(true);
                });
                Ok(worker)
            })
            .await
    }

    pub async fn changed(self: &Arc<Self>, id: Uuid) {
        if let Ok(worker) = self.worker(id).await {
            worker.changed.notify_one();
        }
    }

    pub async fn forget(&self, id: Uuid) {
        if let Some(mut worker) = self.workers.call(move |workers| workers.remove(&id)).await {
            worker.stop.cancel();
            while !*worker.done.borrow_and_update() {
                if worker.done.changed().await.is_err() {
                    break;
                }
            }
        }
    }

    pub async fn replace_credential(
        self: &Arc<Self>,
        id: Uuid,
        credential: Credential,
    ) -> Result<(), MailError> {
        let worker = self.worker(id).await?;
        let (reply, result) = oneshot::channel();
        worker
            .commands
            .try_send(Request::Credential { credential, reply })
            .map_err(|_| MailError::new("mail_account_busy"))?;
        tokio::time::timeout(Duration::from_secs(120), result)
            .await
            .map_err(|_| MailError::new("mail_account_busy"))?
            .map_err(|_| MailError::new("mail_account_unavailable"))?
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
            .ok_or_else(|| MailError::new("maintenance_in_progress"))?;
        let account = store::account(&self.db, id)
            .await
            .map_err(|_| MailError::new("mail_unavailable"))?
            .ok_or_else(|| MailError::new("mail_account_unavailable"))?;
        credential.validate(account.config.provider)?;
        if !store::replace_credential(&self.db, &self.key, id, &credential)
            .await
            .map_err(|_| MailError::new("mail_unavailable"))?
        {
            return Err(MailError::new("mail_account_changed"));
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
        command.validate()?;
        let mutates = command.mutates();
        if mutates && action_id.is_none() {
            return Err(MailError::new("action_id_required"));
        }
        let (reply, result) = oneshot::channel();
        let command = Command {
            bot_id,
            turn_id,
            action_id,
            command,
            deadline: tokio::time::Instant::now() + Duration::from_secs(120),
            reply,
        };
        let account = store::account(&self.db, account_id)
            .await
            .map_err(|_| MailError::new("mail_unavailable"))?
            .ok_or_else(|| MailError::new("mail_forbidden"))?;
        self.authorization(&account, &command).await?;
        let worker = self.worker(account_id).await?;
        worker
            .commands
            .try_send(Request::Mail(command))
            .map_err(|_| MailError::new("mail_account_busy"))?;
        match tokio::time::timeout(Duration::from_secs(120), result).await {
            Ok(result) => result.map_err(|_| MailError::new("mail_account_unavailable"))?,
            Err(_) if mutates => Ok(
                json!({"action_id":action_id,"status":"pending","instruction":"Read mail_action_status or retry the same action ID. Do not create another action ID."}),
            ),
            Err(_) => Err(MailError::new("mail_account_busy")),
        }
    }

    pub async fn run(self: Arc<Self>, stop: CancellationToken) {
        loop {
            if let Ok(accounts) = store::accounts(&self.db).await {
                for account in accounts {
                    if account.state == "active" {
                        let _ = self.worker(account.id).await;
                    }
                }
            }
            tokio::select! { _ = stop.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(30)) => {} }
        }
        let workers: Vec<_> = self
            .workers
            .call(|workers| workers.drain().map(|(_, worker)| worker).collect())
            .await;
        for worker in &workers {
            worker.stop.cancel();
        }
        futures_util::future::join_all(workers.into_iter().map(|mut worker| async move {
            while !*worker.done.borrow_and_update() {
                if worker.done.changed().await.is_err() {
                    break;
                }
            }
        }))
        .await;
    }

    async fn authorization(&self, account: &Account, command: &Command) -> Result<(), MailError> {
        store::authorize(
            &self.db,
            account.id,
            account.generation,
            command.bot_id,
            command.turn_id,
            &command.command,
        )
        .await
        .map_err(|_| MailError::new("mail_forbidden"))?;
        Ok(())
    }

    async fn auth(&self, account: &Account) -> Result<Auth, MailError> {
        let _admission = self
            .scheduler
            .gate()
            .enter()
            .ok_or_else(|| MailError::new("maintenance_in_progress"))?;
        let mut credential = store::credential(&self.db, &self.key, account.id)
            .await
            .map_err(|_| MailError::new("mail_credential_unavailable"))?;
        if oauth::refresh(account.config.provider, &mut credential).await?
            && !store::save_credential(
                &self.db,
                &self.key,
                account.id,
                account.generation,
                &credential,
            )
            .await
            .map_err(|_| MailError::new("mail_unavailable"))?
        {
            return Err(MailError::new("mail_account_changed"));
        }
        let (secret, oauth) = match &credential {
            Credential::Password { password } => (password.clone(), false),
            Credential::OAuth2 { access_token, .. } => (
                access_token
                    .clone()
                    .ok_or_else(|| MailError::new("oauth_reauthentication_required"))?,
                true,
            ),
        };
        Ok(Auth {
            username: account.config.username.clone(),
            secret: zeroize::Zeroizing::new(secret),
            oauth,
        })
    }

    async fn dispatch(&self, id: Uuid, command: Command) {
        let result = self.perform(id, &command).await;
        let _ = command.reply.send(result);
    }

    async fn perform(&self, id: Uuid, command: &Command) -> Result<Value, MailError> {
        if tokio::time::Instant::now() >= command.deadline {
            return Err(MailError::new("mail_queue_expired"));
        }
        let _admission = self
            .scheduler
            .gate()
            .enter()
            .ok_or_else(|| MailError::new("maintenance_in_progress"))?;
        let account = store::account(&self.db, id)
            .await
            .map_err(|_| MailError::new("mail_unavailable"))?
            .ok_or_else(|| MailError::new("mail_forbidden"))?;
        self.authorization(&account, command).await?;
        let auth = self.auth(&account).await?;
        self.authorization(&account, command).await?;
        if command.command.mutates() {
            let action_id = command.action_id.expect("validated action ID");
            match store::begin_action(
                &self.db,
                id,
                account.generation,
                command.bot_id,
                command.turn_id,
                action_id,
                &command.command,
            )
            .await
            .map_err(|_| MailError::new("mail_action_conflict"))?
            {
                store::ActionStart::Existing(receipt) => return Ok(receipt),
                store::ActionStart::Dispatch => {}
            }
            let result = self
                .transport
                .execute(&account.config, &auth, &command.command)
                .await;
            let (status, receipt) = match result {
                Ok(value) => (
                    store::ActionStatus::Succeeded,
                    json!({"action_id":action_id,"status":"succeeded","result":value}),
                ),
                Err(error) => (
                    store::ActionStatus::Uncertain,
                    json!({"action_id":action_id,"status":"uncertain","error":error.code,"instruction":"Inspect the mailbox before starting a different action. This action will not be retried."}),
                ),
            };
            store::finish_action(&self.db, id, action_id, status, &receipt)
                .await
                .map_err(|_| MailError::new("mail_receipt_unavailable"))?;
            Ok(receipt)
        } else {
            self.transport
                .execute(&account.config, &auth, &command.command)
                .await
        }
    }

    async fn prepare_watch(&self, id: Uuid) -> Result<(Account, Auth), MailError> {
        let account = store::account(&self.db, id)
            .await
            .map_err(|_| MailError::new("mail_unavailable"))?
            .ok_or_else(|| MailError::new("mail_account_unavailable"))?;
        if account.state != "active" {
            return Err(MailError::new("mail_account_paused"));
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
        let cursor = loop {
            let old = store::cursor(&self.db, id)
                .await
                .map_err(|_| MailError::new("mail_unavailable"))?;
            let scan = self
                .transport
                .scan(&account.config, auth, old.as_ref())
                .await?;
            let more = scan.more || scan.arrivals.len() > store::MAX_ARRIVALS;
            {
                let _admission = self
                    .scheduler
                    .gate()
                    .enter()
                    .ok_or_else(|| MailError::new("maintenance_in_progress"))?;
                if !store::record_scan(&self.db, id, account.generation, old.as_ref(), &scan)
                    .await
                    .map_err(|_| MailError::new("mail_unavailable"))?
                {
                    return Err(MailError::new("mail_account_changed"));
                }
            }
            self.scheduler.wake();
            if !more {
                break scan.cursor;
            }
        };
        self.health(id, account.generation, "connected").await;
        self.transport
            .wait(&account.config, auth, &cursor, stop)
            .await
    }

    async fn health(&self, id: Uuid, generation: i64, code: &str) {
        if code == "mail_account_paused" {
            return;
        }
        if let Some(_admission) = self.scheduler.gate().enter() {
            let _ = store::set_health(&self.db, id, generation, code).await;
        }
    }

    async fn account_loop(
        self: Arc<Self>,
        id: Uuid,
        worker: Worker,
        mut commands: mpsc::Receiver<Request>,
    ) {
        let mut failures = 0u32;
        let mut retry_at = tokio::time::Instant::now();
        let mut watching: Option<Watching> = None;
        loop {
            if worker.stop.is_cancelled() {
                break;
            }
            if watching.is_none() && tokio::time::Instant::now() >= retry_at {
                match self.prepare_watch(id).await {
                    Ok((account, auth)) => {
                        let manager = self.clone();
                        let stop = worker.stop.child_token();
                        let watch_stop = stop.clone();
                        let generation = account.generation;
                        let task = tokio::spawn(async move {
                            tokio::select! { _ = watch_stop.cancelled() => Ok(()), result = manager.watch(&account,&auth,watch_stop.clone()) => result }
                        });
                        watching = Some(Watching {
                            task,
                            stop,
                            generation,
                        });
                    }
                    Err(error) => {
                        if let Ok(Some(account)) = store::account(&self.db, id).await
                            && account.state == "active"
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
                _ = worker.stop.cancelled() => break,
                command = commands.recv() => {
                    let Some(command) = command else { break; };
                    match command {
                        Request::Mail(command) => self.dispatch(id,command).await,
                        Request::Credential { credential,reply } => {
                            stop_watch(&mut watching).await;
                            let result = self.save_owner_credential(id,credential).await;
                            let _ = reply.send(result);
                            failures = 0;
                            retry_at = tokio::time::Instant::now();
                        }
                    }
                }
                _ = worker.changed.notified() => {
                    stop_watch(&mut watching).await;
                    failures = 0;
                    retry_at = tokio::time::Instant::now();
                }
                result = async { (&mut watching.as_mut().expect("active watch").task).await }, if watching.is_some() => {
                    let completed = watching.take().expect("completed watch");
                    match result {
                        Ok(Ok(())) => { failures = 0; retry_at = tokio::time::Instant::now(); }
                        result => {
                            let code = match result { Ok(Err(error)) => error.code, _ => "mail_watch_failed" };
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
