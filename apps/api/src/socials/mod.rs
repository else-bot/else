pub mod delivery;
pub mod providers;
pub mod store;
pub mod types;

use crate::{actor::Actor, credentials::SecretKey};
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use types::*;
use uuid::Uuid;

enum Operation {
    Command(Command),
    Send(SendMessage),
}
impl Operation {
    fn destination(&self) -> Option<&str> {
        match self {
            Self::Command(c) => c.destination(),
            Self::Send(message) => Some(&message.destination),
        }
    }
    fn mutates(&self) -> bool {
        match self {
            Self::Command(c) => c.mutates(),
            Self::Send(_) => true,
        }
    }
    fn sends(&self) -> bool {
        match self {
            Self::Command(c) => c.sends(),
            Self::Send(_) => true,
        }
    }
    fn payload(&self) -> Value {
        match self {
            Self::Command(c) => serde_json::to_value(c).expect("command serializes"),
            Self::Send(SendMessage {
                destination,
                body,
                reply_to,
            }) => json!({"send":destination,"body":body,"reply_to":reply_to}),
        }
    }
}
struct Request {
    cancel: CancellationToken,
    generation: i64,
    bot: Uuid,
    turn: Uuid,
    action: Option<Uuid>,
    operation: Operation,
    reply: oneshot::Sender<Result<Value, Error>>,
}
enum WorkerRequest {
    Action(Request),
    Discover {
        generation: i64,
        reply: oneshot::Sender<Result<Vec<Destination>, Error>>,
    },
    Resolve {
        generation: i64,
        username: String,
        reply: oneshot::Sender<Result<Destination, Error>>,
    },
}
impl WorkerRequest {
    fn reject(self) {
        match self {
            Self::Action(request) => {
                let _ = request.reply.send(Err(Error::Inactive));
            }
            Self::Discover { reply, .. } => {
                let _ = reply.send(Err(Error::Inactive));
            }
            Self::Resolve { reply, .. } => {
                let _ = reply.send(Err(Error::Inactive));
            }
        }
    }
}
#[derive(Clone)]
struct Worker {
    commands: mpsc::Sender<WorkerRequest>,
    changed: watch::Sender<u64>,
    stop: CancellationToken,
    done: watch::Receiver<bool>,
}
/// How long a listener must stay up before its connection counts as connected.
const LISTEN_SETTLE: Duration = Duration::from_secs(3);

pub struct Manager {
    pub db: SqlitePool,
    pub key: Arc<SecretKey>,
    transport: Arc<dyn providers::Transport>,
    scheduler: Option<Arc<crate::scheduler::Scheduler>>,
    workers: Actor<HashMap<Uuid, Worker>>,
}
impl Manager {
    pub fn new(db: SqlitePool, key: Arc<SecretKey>) -> Self {
        Self::with_transport(db, key, Arc::new(providers::Native))
    }
    pub fn with_transport(
        db: SqlitePool,
        key: Arc<SecretKey>,
        transport: Arc<dyn providers::Transport>,
    ) -> Self {
        Self {
            db,
            key,
            transport,
            scheduler: None,
            workers: Actor::default(),
        }
    }
    pub fn with_scheduler(mut self, scheduler: Arc<crate::scheduler::Scheduler>) -> Self {
        self.scheduler = Some(scheduler);
        self
    }
    async fn worker(self: &Arc<Self>, id: Uuid) -> Result<Worker, Error> {
        let manager = self.clone();
        self.workers
            .call(move |workers| {
                workers.retain(|_, w| !*w.done.borrow());
                if let Some(w) = workers.get(&id) {
                    return Ok(w.clone());
                }
                if workers.len() >= 32 {
                    return Err(Error::Busy);
                }
                let (tx, rx) = mpsc::channel(16);
                let (changed, revision) = watch::channel(0);
                let (done, completed) = watch::channel(false);
                let w = Worker {
                    commands: tx,
                    changed,
                    stop: CancellationToken::new(),
                    done: completed,
                };
                workers.insert(id, w.clone());
                let running = w.clone();
                tokio::spawn(async move {
                    manager.worker_loop(id, rx, revision, running.stop).await;
                    let _ = done.send(true);
                });
                Ok(w)
            })
            .await
    }
    pub async fn changed(self: &Arc<Self>, id: Uuid) {
        if let Ok(w) = self.worker(id).await {
            w.changed.send_modify(|n| *n += 1);
        }
    }
    pub async fn validate(&self, p: Provider, t: &Token) -> Result<Identity, Error> {
        tokio::time::timeout(Duration::from_secs(20), self.transport.identity(p, t))
            .await
            .map_err(|_| Error::Platform)?
    }
    pub async fn discover(self: &Arc<Self>, id: Uuid) -> Result<Vec<Destination>, Error> {
        let c = store::connection(&self.db, id).await?;
        let worker = self.worker(id).await?;
        let (reply, result) = oneshot::channel();
        worker
            .commands
            .try_send(WorkerRequest::Discover {
                generation: c.generation,
                reply,
            })
            .map_err(|_| Error::Busy)?;
        tokio::time::timeout(Duration::from_secs(30), result)
            .await
            .map_err(|_| Error::Platform)?
            .map_err(|_| Error::Inactive)?
    }
    async fn discover_session(
        &self,
        c: &Connection,
        session: &providers::Session,
    ) -> Result<Vec<Destination>, Error> {
        let list = tokio::time::timeout(
            Duration::from_secs(20),
            self.transport.destinations(session),
        )
        .await
        .map_err(|_| Error::Platform)??;
        store::save_destinations(&self.db, c.id, &list).await?;
        store::destinations(&self.db, c.id).await
    }
    /// The chat ID `destination` names. Tools resolve a public `@username`
    /// or share link when the bot first names a chat, so the owner approves,
    /// and triggers and receipts record, the ID incoming messages carry even
    /// if the username later moves. Only work for the owner may look one up.
    pub async fn resolve(
        self: &Arc<Self>,
        id: Uuid,
        bot: Uuid,
        turn: Uuid,
        destination: &str,
    ) -> Result<String, Error> {
        let Some(username) = public_username(destination)? else {
            return Ok(destination.to_owned());
        };
        let c = store::connection(&self.db, id).await?;
        let access = store::authorize(
            &mut *self.db.acquire().await?,
            &c,
            bot,
            turn,
            None,
            false,
            false,
        )
        .await?;
        if access != Access::Owner {
            return Err(Error::Forbidden);
        }
        let cancel = match &self.scheduler {
            Some(scheduler) => scheduler.cancellation(turn).await.unwrap_or_default(),
            None => CancellationToken::new(),
        };
        let worker = self.worker(id).await?;
        let (reply, result) = oneshot::channel();
        worker
            .commands
            .try_send(WorkerRequest::Resolve {
                generation: c.generation,
                username,
                reply,
            })
            .map_err(|_| Error::Busy)?;
        let found = tokio::select! {
            _ = cancel.cancelled() => return Err(Error::Inactive),
            r = tokio::time::timeout(Duration::from_secs(30), result) => r,
        };
        Ok(found
            .map_err(|_| Error::Platform)?
            .map_err(|_| Error::Inactive)??
            .id)
    }
    async fn resolve_session(
        &self,
        c: &Connection,
        session: &providers::Session,
        username: &str,
    ) -> Result<Destination, Error> {
        let found = tokio::time::timeout(
            Duration::from_secs(20),
            self.transport.resolve(c, session, username),
        )
        .await
        .map_err(|_| Error::Platform)??;
        store::save_destinations(&self.db, c.id, std::slice::from_ref(&found)).await?;
        Ok(found)
    }
    async fn request(
        self: &Arc<Self>,
        id: Uuid,
        bot: Uuid,
        turn: Uuid,
        action: Option<Uuid>,
        operation: Operation,
    ) -> Result<Value, Error> {
        let c = store::connection(&self.db, id).await?;
        if let Operation::Command(cmd) = &operation
            && !c.provider.supports(cmd.operation())
        {
            return Err(Error::Unsupported);
        }
        let mut conn = self.db.acquire().await?;
        store::authorize(
            &mut conn,
            &c,
            bot,
            turn,
            operation.destination(),
            operation.mutates(),
            operation.sends(),
        )
        .await?;
        drop(conn);
        let worker = self.worker(id).await?;
        let (tx, rx) = oneshot::channel();
        let cancel = if let Some(scheduler) = &self.scheduler {
            scheduler.cancellation(turn).await.unwrap_or_default()
        } else {
            CancellationToken::new()
        };
        worker
            .commands
            .try_send(WorkerRequest::Action(Request {
                cancel,
                generation: c.generation,
                bot,
                turn,
                action,
                operation,
                reply: tx,
            }))
            .map_err(|_| Error::Busy)?;
        tokio::time::timeout(Duration::from_secs(60), rx)
            .await
            .map_err(|_| Error::Uncertain)?
            .map_err(|_| Error::Inactive)?
    }
    pub async fn execute(
        self: &Arc<Self>,
        id: Uuid,
        bot: Uuid,
        turn: Uuid,
        action: Option<Uuid>,
        command: Command,
    ) -> Result<Value, Error> {
        command.validate()?;
        self.request(id, bot, turn, action, Operation::Command(command))
            .await
    }
    pub async fn send(
        self: &Arc<Self>,
        id: Uuid,
        bot: Uuid,
        turn: Uuid,
        action: Uuid,
        message: SendMessage,
    ) -> Result<Value, Error> {
        self.request(id, bot, turn, Some(action), Operation::Send(message))
            .await
    }
    async fn worker_loop(
        self: Arc<Self>,
        id: Uuid,
        mut requests: mpsc::Receiver<WorkerRequest>,
        mut changed: watch::Receiver<u64>,
        stop: CancellationToken,
    ) {
        loop {
            changed.borrow_and_update();
            let loaded = async {
                let c = store::connection(&self.db, id).await?;
                let token = store::credential(&self.db, &self.key, &c).await?;
                Ok::<_, Error>((c, token))
            }
            .await;
            let (c, token) = match loaded {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let session = Arc::new(providers::Session::new(c.provider, token));
            let listener_session = session.clone();
            let inbox_stop = stop.child_token();
            let running = inbox_stop.clone();
            let manager = self.clone();
            let account = c.clone();
            let inbox = tokio::spawn(async move {
                if account.state == "active" {
                    manager.inbox(account, listener_session, running).await;
                }
            });
            let reload = loop {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break false,
                    result = changed.changed() => {
                        if result.is_err() {
                            break false;
                        }
                        break true;
                    },
                    request = requests.recv() => {
                        let Some(message) = request else { break false };
                        let request = match message {
                            WorkerRequest::Discover { generation, reply } => {
                                if generation != c.generation {
                                    let _ = reply.send(Err(Error::Inactive));
                                    continue;
                                }
                                let (result, reload) = tokio::select! {
                                    biased;
                                    _ = stop.cancelled() => (Err(Error::Inactive), true),
                                    _ = changed.changed() => (Err(Error::Inactive), true),
                                    result = self.discover_session(&c, &session) => (result, false),
                                };
                                let _ = reply.send(result);
                                if reload { break true; }
                                continue;
                            }
                            WorkerRequest::Resolve { generation, username, mut reply } => {
                                if generation != c.generation || c.state != "active" {
                                    let _ = reply.send(Err(Error::Inactive));
                                    continue;
                                }
                                let (result, reload) = tokio::select! {
                                    biased;
                                    _ = stop.cancelled() => (Err(Error::Inactive), true),
                                    _ = changed.changed() => (Err(Error::Inactive), true),
                                    _ = reply.closed() => continue,
                                    result = self.resolve_session(&c, &session, &username) => (result, false),
                                };
                                let _ = reply.send(result);
                                if reload { break true; }
                                continue;
                            }
                            WorkerRequest::Action(request) => request,
                        };
                        if request.generation != c.generation || c.state != "active" {
                            let _ = request.reply.send(Err(Error::Inactive));
                            continue;
                        }
                        let (result, reload) = tokio::select! {
                            biased;
                            _ = stop.cancelled() => (Err(Error::Inactive), true),
                            _ = changed.changed() => (Err(Error::Inactive), true),
                            _ = request.cancel.cancelled() => (Err(Error::Inactive), false),
                            _ = self.revoked(&c, &request) => (Err(Error::Inactive), false),
                            result = self.dispatch(&c, &session, &request) => (result, false),
                        };
                        if result.is_err()
                            && request.operation.mutates()
                            && let Some(action) = request.action
                        {
                            let _ = store::uncertain(&self.db, c.id, Some(action)).await;
                        }
                        let _ = request.reply.send(result);
                        if reload {
                            break true;
                        }
                    }
                };
            };
            inbox_stop.cancel();
            let _ = inbox.await;
            let _ = store::uncertain(&self.db, id, None).await;
            if !reload {
                break;
            }
        }
        while let Ok(r) = requests.try_recv() {
            r.reject();
        }
    }
    async fn revoked(&self, c: &Connection, r: &Request) {
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let check = async {
                let mut conn = self.db.acquire().await?;
                store::authorize(
                    &mut conn,
                    c,
                    r.bot,
                    r.turn,
                    r.operation.destination(),
                    r.operation.mutates(),
                    r.operation.sends(),
                )
                .await
                .map(|_| ())
            }
            .await;
            if check.is_err() {
                return;
            }
        }
    }
    async fn dispatch(
        &self,
        c: &Connection,
        session: &providers::Session,
        r: &Request,
    ) -> Result<Value, Error> {
        let mut conn = self.db.acquire().await?;
        let access = store::authorize(
            &mut conn,
            c,
            r.bot,
            r.turn,
            r.operation.destination(),
            r.operation.mutates(),
            r.operation.sends(),
        )
        .await?;
        drop(conn);
        if r.operation.mutates() {
            let id = r
                .action
                .ok_or(Error::Invalid("Mutations require a stable action_id."))?;
            if let Some(result) = store::begin_action(
                &self.db,
                c,
                store::Action {
                    bot: r.bot,
                    turn: r.turn,
                    id,
                    destination: r.operation.destination().ok_or(Error::Forbidden)?,
                    payload: &r.operation.payload(),
                    send: r.operation.sends(),
                },
            )
            .await?
            {
                return Ok(result);
            }
        }
        let result = match &r.operation {
            Operation::Command(Command::Conversations) => {
                let list = store::destinations(&self.db, c.id).await?;
                Ok(
                    json!({"destinations":list.into_iter().filter(|d|access.allows(&d.id)).collect::<Vec<_>>()}),
                )
            }
            Operation::Command(Command::History {
                destination,
                before,
                limit,
            }) if c.provider == Provider::Telegram => {
                store::local_messages(&self.db, c.id, destination, before.as_deref(), None, *limit)
                    .await
            }
            Operation::Command(Command::Search {
                destination,
                query,
                limit,
            }) => {
                store::local_messages(&self.db, c.id, destination, None, Some(query), *limit).await
            }
            Operation::Command(Command::Message {
                destination,
                message_id,
            }) if c.provider == Provider::Telegram => {
                let row:Option<(String,String)>=sqlx::query_as("SELECT author_id,body FROM social_messages WHERE connection_id=?1 AND destination=?2 AND message_id=?3").bind(c.id).bind(destination).bind(message_id).fetch_optional(&self.db).await?;
                row.map(|(author, body)| json!({"id":message_id,"author_id":author,"body":body}))
                    .ok_or(Error::NotFound)
            }
            Operation::Command(cmd) => self.transport.execute(session, cmd).await,
            Operation::Send(SendMessage {
                destination,
                body,
                reply_to,
            }) => {
                let result = self
                    .transport
                    .send(session, destination, body, reply_to.as_deref())
                    .await;
                if let Ok(value) = &result {
                    let remote = value
                        .get("id")
                        .or_else(|| value.get("message_id"))
                        .map(|v| {
                            v.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| v.to_string())
                        });
                    if let Some(remote) = remote {
                        sqlx::query("INSERT OR IGNORE INTO social_sent_messages(connection_id,destination,message_id) VALUES(?1,?2,?3)").bind(c.id).bind(destination).bind(remote).execute(&self.db).await?;
                    }
                }
                result
            }
        };
        let result = match result {
            Ok(value)
                if serde_json::to_vec(&value).map_or(true, |bytes| bytes.len() > 256 * 1024) =>
            {
                Err(if r.operation.mutates() {
                    Error::Uncertain
                } else {
                    Error::Invalid("The result is too large. Request a smaller page.")
                })
            }
            other => other,
        };
        if r.operation.mutates() {
            return store::finish_action(
                &self.db,
                c.id,
                r.action.expect("validated action"),
                &result,
            )
            .await;
        }
        let mut conn = self.db.acquire().await?;
        store::authorize(
            &mut conn,
            c,
            r.bot,
            r.turn,
            r.operation.destination(),
            false,
            false,
        )
        .await?;
        result
    }
    async fn inbox(
        &self,
        c: Connection,
        session: Arc<providers::Session>,
        stop: CancellationToken,
    ) {
        loop {
            if stop.is_cancelled() {
                return;
            }
            let result = self.inbox_once(&c, &session, stop.clone()).await;
            if stop.is_cancelled() {
                return;
            }
            let health = match &result {
                Ok(()) => "connected",
                Err(Error::ReceiverConflict) => "receiver_conflict",
                Err(_) => "reconnecting",
            };
            if let Err(error) = &result {
                tracing::warn!(connection = %c.id, provider = c.provider.name(), %error, "social inbox stopped; retrying");
            }
            let _ = sqlx::query(
                "UPDATE social_connections SET health=?3 WHERE id=?1 AND generation=?2",
            )
            .bind(c.id)
            .bind(c.generation)
            .bind(health)
            .execute(&self.db)
            .await;
            tokio::select! {_=stop.cancelled()=>return,_=tokio::time::sleep(Duration::from_secs(10))=>{}}
        }
    }
    async fn inbox_once(
        &self,
        c: &Connection,
        session: &providers::Session,
        stop: CancellationToken,
    ) -> Result<(), Error> {
        let offset: i64 =
            sqlx::query_scalar("SELECT poll_offset FROM social_connections WHERE id=?1")
                .bind(c.id)
                .fetch_one(&self.db)
                .await?;
        let (tx, mut rx) = mpsc::channel::<Envelope>(16);
        let listen = self.transport.listen(c, session, offset, tx, stop.clone());
        tokio::pin!(listen);
        // A refused poll fails at once; one that stays open or delivers a
        // message is connected.
        let settled = tokio::time::sleep(LISTEN_SETTLE);
        tokio::pin!(settled);
        let mut connected = false;
        loop {
            let event = tokio::select! {
                biased;
                _ = stop.cancelled() => return Ok(()),
                r = &mut listen => return r,
                () = &mut settled, if !connected => None,
                event = rx.recv() => match event {
                    Some(event) => Some(event),
                    None => return Ok(()),
                },
            };
            if !connected {
                connected = true;
                sqlx::query(
                    "UPDATE social_connections SET health='connected' WHERE id=?1 AND generation=?2",
                )
                .bind(c.id)
                .bind(c.generation)
                .execute(&self.db)
                .await?;
            }
            if let Some(event) = event {
                store::record(&self.db, c, &event.incoming, event.next_offset).await?;
                let _ = event.ack.send(());
            }
        }
    }
    pub async fn run(self: Arc<Self>, stop: CancellationToken) {
        if let Ok(list) = store::connections(&self.db).await {
            for c in list {
                let _ = self.worker(c.id).await;
            }
        }
        stop.cancelled().await;
        let workers = self
            .workers
            .call(|workers| workers.values().cloned().collect::<Vec<_>>())
            .await;
        for w in &workers {
            w.stop.cancel()
        }
        futures_util::future::join_all(workers.into_iter().map(|mut w| async move {
            while !*w.done.borrow() {
                if w.done.changed().await.is_err() {
                    break;
                }
            }
        }))
        .await;
    }
}
