use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::http::StatusCode;
use else_api::app::providers::{
    Availability, CheckpointRequest, Fenced, Profile, Provider, Selection, SessionState, TurnInput,
    TurnOutcome, TurnSink, Usage,
};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use super::{Builder, TestApp, until};

pub const GATE: &str = "[gate]";
/// Like [`GATE`], but the reply's opening chunks ("Synthetic reply ✓ fro")
/// reach the turn in one write before the hold, so a sink that throttles
/// its events still reports them.
pub const GATE_AFTER_OPENING: &str = "[gate:after-opening]";
pub const GATE_EVERY_ATTEMPT: &str = "[gate:every-attempt]";

enum Message {
    Record { input: Box<TurnInput>, held: bool },
    Grant(Uuid, oneshot::Sender<watch::Receiver<Option<Granted>>>),
    Inputs(Uuid, oneshot::Sender<Vec<TurnInput>>),
    Hold(Uuid, oneshot::Sender<()>),
    Release(Uuid),
}

/// The tool grant an attempt was given, and whether its input asks to hold
/// it: a gate until [`Harness::release`], or a synthetic hang until it is
/// stopped. Other directives in the same input can still end it early.
#[derive(Clone)]
struct Granted {
    token: String,
    held: bool,
}

#[derive(Default)]
struct State {
    grants: HashMap<Uuid, watch::Sender<Option<Granted>>>,
    inputs: Vec<TurnInput>,
    held: HashMap<Uuid, oneshot::Sender<()>>,
    released: HashSet<Uuid>,
}

impl State {
    fn grant_updates(&mut self, turn: Uuid) -> &watch::Sender<Option<Granted>> {
        self.grants
            .entry(turn)
            .or_insert_with(|| watch::channel(None).0)
    }

    fn handle(&mut self, message: Message) {
        match message {
            Message::Record { input, held } => {
                if let Some(token) = input.tool_grant.clone() {
                    self.grant_updates(input.turn_id)
                        .send_replace(Some(Granted { token, held }));
                }
                self.inputs.push(*input);
            }
            Message::Grant(turn, reply) => {
                let _ = reply.send(self.grant_updates(turn).subscribe());
            }
            Message::Inputs(turn, reply) => {
                let inputs = self.inputs.iter().filter(|i| i.turn_id == turn);
                let _ = reply.send(inputs.cloned().collect());
            }
            Message::Hold(turn, open) => {
                if self.released.remove(&turn) {
                    let _ = open.send(());
                } else {
                    self.held.insert(turn, open);
                }
            }
            Message::Release(turn) => {
                if !self
                    .held
                    .remove(&turn)
                    .is_some_and(|open| open.send(()).is_ok())
                {
                    // An interrupted attempt may have dropped its receiver.
                    self.released.insert(turn);
                }
            }
        }
    }
}

/// Wraps [`Synthetic`] with test controls: gates, held hangs, failing
/// checkpoints, forgotten sessions and a projects root.
pub struct Recorder {
    pub inner: Synthetic,
    mailbox: mpsc::Sender<Message>,
    projects: Option<std::path::PathBuf>,
    hangs_held: watch::Sender<bool>,
    checkpoint_error: watch::Sender<Option<String>>,
    forgotten: watch::Sender<HashSet<String>>,
}

impl Recorder {
    pub fn new(inner: Synthetic, projects: Option<std::path::PathBuf>) -> Self {
        let (mailbox, mut messages) = mpsc::channel(64);
        tokio::spawn(async move {
            let mut state = State::default();
            while let Some(message) = messages.recv().await {
                state.handle(message);
            }
        });
        Self {
            inner,
            mailbox,
            projects,
            hangs_held: watch::Sender::new(false),
            checkpoint_error: watch::Sender::new(None),
            forgotten: watch::Sender::new(HashSet::new()),
        }
    }

    /// Hold or release the drain of every cancelled `[synthetic:hang]` turn,
    /// so a test can queue messages behind one at any speed.
    pub fn hold_hangs(&self, held: bool) {
        self.hangs_held.send_replace(held);
    }

    /// Make checkpoints fail with `reason` (or succeed again with `None`).
    pub fn fail_checkpoints(&self, reason: Option<&str>) {
        self.checkpoint_error
            .send_replace(reason.map(str::to_owned));
    }

    /// Lose these native sessions, as if their files were removed.
    pub fn forget_sessions(&self, native_ids: &[String]) {
        self.forgotten
            .send_modify(|forgotten| forgotten.extend(native_ids.iter().cloned()));
    }

    async fn ask<T>(&self, message: impl FnOnce(oneshot::Sender<T>) -> Message) -> T {
        let (reply, response) = oneshot::channel();
        self.mailbox.send(message(reply)).await.unwrap();
        response.await.unwrap()
    }
}

struct Gated<'a> {
    inner: &'a dyn TurnSink,
    gate: Option<(&'a Recorder, Uuid)>,
    /// The opening chunks still to collect, and those collected so far.
    opening: std::sync::Mutex<Option<(usize, String)>>,
    open: tokio::sync::OnceCell<()>,
    stop: else_api::app::providers::Stop,
}

#[async_trait::async_trait]
impl TurnSink for Gated<'_> {
    async fn packet(&self, selection: &Selection, payload: &str) -> Result<(), Fenced> {
        self.inner.packet(selection, payload).await
    }
    async fn retire_session(&self) {
        self.inner.retire_session().await
    }
    async fn sending(&self) -> Result<(), Fenced> {
        self.inner.sending().await
    }
    async fn running(&self) -> Result<(), Fenced> {
        self.inner.running().await
    }
    async fn text(&self, chunk: &str) -> Result<(), Fenced> {
        let opened = {
            let mut opening = self.opening.lock().unwrap();
            match opening.as_mut() {
                Some((left, text)) => {
                    text.push_str(chunk);
                    *left -= 1;
                    if *left > 0 {
                        return Ok(());
                    }
                    opening.take().map(|(_, text)| text)
                }
                None => None,
            }
        };
        if let Some(text) = opened {
            return self.inner.text(&text).await;
        }
        if let Some((recorder, turn)) = self.gate {
            let held = self
                .open
                .get_or_init(|| recorder.ask(|open| Message::Hold(turn, open)));
            tokio::select! {
                _ = held => {}
                () = self.stop.escalated() => return Err(Fenced),
            }
        }
        self.inner.text(chunk).await
    }
    async fn end_message(&self) -> Result<(), Fenced> {
        let opened = self.opening.lock().unwrap().take();
        if let Some((_, text)) = opened {
            self.inner.text(&text).await?;
        }
        self.inner.end_message().await
    }
    async fn usage(&self, usage: Usage) {
        self.inner.usage(usage).await
    }
    async fn session(&self, state: &SessionState) -> Result<(), Fenced> {
        self.inner.session(state).await
    }
    async fn reported(&self, settings: &Value, native_turn_id: &str) -> Result<(), Fenced> {
        self.inner.reported(settings, native_turn_id).await
    }
}

#[async_trait::async_trait]
impl Provider for Recorder {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn label(&self) -> &'static str {
        self.inner.label()
    }
    fn project_dir(&self, bot: Uuid) -> Option<std::path::PathBuf> {
        let root = self.projects.as_deref()?;
        else_api::providers::host::project_dir(root, bot).ok()
    }
    fn availability(&self) -> Availability {
        Availability::Available
    }
    fn uses_profile(&self) -> bool {
        false
    }
    fn validate_profile(&self, profile: &Profile) -> Result<(), String> {
        self.inner.validate_profile(profile)
    }
    async fn checkpoint(
        &self,
        request: CheckpointRequest,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<String, String> {
        if let Some(reason) = self.checkpoint_error.borrow().clone() {
            return Err(reason);
        }
        self.inner.checkpoint(request, cancel).await
    }
    async fn run(
        &self,
        mut input: TurnInput,
        sink: &dyn TurnSink,
        stop: else_api::app::providers::Stop,
    ) -> TurnOutcome {
        let forgotten = input
            .session
            .as_ref()
            .is_some_and(|s| self.forgotten.borrow().contains(&s.native_id));
        if forgotten {
            input.session = None;
        }
        let opening = input.attempt == 1 && input.text.contains(GATE_AFTER_OPENING);
        let gated = input.text.contains(GATE_EVERY_ATTEMPT)
            || opening
            || (input.attempt == 1 && input.text.contains(GATE));
        let directives =
            || std::iter::once(&input.text).chain(input.steered.iter().map(|s| &s.message.text));
        let hang = directives().any(|text| text.contains("[synthetic:hang]"));
        let hangs = hang || directives().any(|text| text.contains("[synthetic:ignore-cancel]"));
        self.mailbox
            .send(Message::Record {
                input: Box::new(input.clone()),
                held: gated || hangs,
            })
            .await
            .unwrap();
        let sink = Gated {
            inner: sink,
            gate: gated.then_some((self, input.turn_id)),
            opening: std::sync::Mutex::new(opening.then(|| (3, String::new()))),
            open: tokio::sync::OnceCell::new(),
            stop: stop.clone(),
        };
        let outcome = self.inner.run(input, &sink, stop.clone()).await;
        if hang && stop.token().is_cancelled() {
            let _ = self.hangs_held.subscribe().wait_for(|held| !held).await;
        }
        // A reply that ended inside its opening still reaches the turn.
        let opened = sink.opening.lock().unwrap().take();
        if let Some((_, text)) = opened {
            let _ = sink.inner.text(&text).await;
        }
        outcome
    }
}

pub struct Harness {
    pub app: TestApp,
    pub recorder: Arc<Recorder>,
}

/// A running app whose first provider is a recorder, the default for turns.
pub async fn harness(mut builder: Builder) -> Harness {
    let db = builder.database().await;
    sqlx::query("UPDATE installation SET default_provider_id = 'synthetic'")
        .execute(&db.pool)
        .await
        .unwrap();
    let recorder = Arc::new(Recorder::new(
        Synthetic::new(std::time::Duration::from_millis(5)),
        builder.projects.clone(),
    ));
    let mut providers: Vec<Arc<dyn Provider>> = vec![recorder.clone()];
    providers.extend(builder.extra_providers());
    let app = builder.start_with(providers).await;
    Harness { app, recorder }
}

impl Harness {
    async fn grant(&self, turn: Uuid, old: Option<&str>) -> Granted {
        let mut updates = self.recorder.ask(|r| Message::Grant(turn, r)).await;
        let granted = until(
            &format!("turn {turn} to supply a new grant"),
            updates.wait_for(|granted| {
                granted
                    .as_ref()
                    .is_some_and(|granted| Some(granted.token.as_str()) != old)
            }),
        )
        .await
        .unwrap();
        granted.clone().unwrap()
    }

    pub async fn continued(&self, turn: Uuid, old: &str) -> String {
        let granted = self.grant(turn, Some(old)).await;
        assert!(
            granted.held,
            "turn {turn} resumed without a gate, so it can finish before the test acts; \
             start it with GATE_EVERY_ATTEMPT"
        );
        self.app.wait_turn(turn, &["running"]).await;
        granted.token
    }

    pub async fn release(&self, turn: Uuid) {
        self.recorder
            .mailbox
            .send(Message::Release(turn))
            .await
            .unwrap();
    }

    pub async fn send(&self, bot: &str, text: &str) -> Uuid {
        let sent = self.app.send(bot, Uuid::new_v4(), text, None).await;
        assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
        sent.body["turn"]["id"].as_str().unwrap().parse().unwrap()
    }

    pub async fn running(&self, bot: &str, text: &str) -> (Uuid, String) {
        let turn = self.send(bot, text).await;
        self.started(turn).await
    }

    pub async fn started(&self, turn: Uuid) -> (Uuid, String) {
        self.app.wait_turn(turn, &["running"]).await;
        (turn, self.grant(turn, None).await.token)
    }

    /// A grant for `turn` with exactly `tools`, for a test that checks a
    /// tool set other than the one the scheduler issued.
    pub async fn grant_with(&self, turn: Uuid, tools: Vec<else_api::grants::Tool>) -> String {
        let (bot, conversation): (Uuid, Uuid) =
            sqlx::query_as("SELECT bot_id, conversation_id FROM turns WHERE id = ?1")
                .bind(turn)
                .fetch_one(&self.app.db.pool)
                .await
                .unwrap();
        self.app
            .state
            .grants
            .issue(
                bot,
                conversation,
                turn,
                tools,
                else_api::grants::DEFAULT_TTL,
            )
            .await
    }

    pub async fn inputs(&self, turn: Uuid) -> Vec<TurnInput> {
        self.recorder.ask(|r| Message::Inputs(turn, r)).await
    }

    pub async fn packets(&self) -> Vec<String> {
        self.recorder.inner.packets().await
    }
}

pub struct Called {
    pub status: StatusCode,
    pub result: Value,
    pub error: bool,
    pub body: Value,
}

impl Called {
    pub fn code(&self) -> &str {
        self.result
            .as_str()
            .and_then(|text| text.split(':').next())
            .unwrap_or_default()
    }
}

pub async fn tool(app: &TestApp, token: &str, name: &str, args: Value) -> Called {
    let reply = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": name, "arguments": args } })),
            Some(token),
        )
        .await;
    let text = reply.body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    Called {
        status: reply.status,
        result: serde_json::from_str(&text).unwrap_or(json!(text)),
        error: reply.body["result"]["isError"] == true,
        body: reply.body,
    }
}
