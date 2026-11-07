use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use else_api::app::providers::{
    Availability, CheckpointRequest, Fenced, Profile, Provider, Selection, SessionState, TurnInput,
    TurnOutcome, TurnSink,
};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::{TestApp, TestDb};

pub const GATE: &str = "[gate]";
pub const GATE_EVERY_ATTEMPT: &str = "[gate:every-attempt]";

enum Message {
    Record(Box<TurnInput>),
    Grant(Uuid, oneshot::Sender<Option<String>>),
    Inputs(Uuid, oneshot::Sender<Vec<TurnInput>>),
    Hold(Uuid, oneshot::Sender<()>),
    Release(Uuid),
}

#[derive(Default)]
struct State {
    grants: HashMap<Uuid, String>,
    inputs: Vec<TurnInput>,
    held: HashMap<Uuid, oneshot::Sender<()>>,
    released: HashSet<Uuid>,
}

impl State {
    fn handle(&mut self, message: Message) {
        match message {
            Message::Record(input) => {
                if let Some(token) = input.tool_grant.clone() {
                    self.grants.insert(input.turn_id, token);
                }
                self.inputs.push(*input);
            }
            Message::Grant(turn, reply) => {
                let _ = reply.send(self.grants.get(&turn).cloned());
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
            Message::Release(turn) => match self.held.remove(&turn) {
                Some(open) => {
                    let _ = open.send(());
                }
                None => {
                    self.released.insert(turn);
                }
            },
        }
    }
}

pub struct Recorder {
    pub inner: Synthetic,
    mailbox: mpsc::Sender<Message>,
}

impl Recorder {
    async fn ask<T>(&self, message: impl FnOnce(oneshot::Sender<T>) -> Message) -> T {
        let (reply, response) = oneshot::channel();
        self.mailbox.send(message(reply)).await.unwrap();
        response.await.unwrap()
    }
}

struct Gated<'a> {
    inner: &'a dyn TurnSink,
    gate: Option<(&'a Recorder, Uuid)>,
    open: tokio::sync::OnceCell<()>,
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
        if let Some((recorder, turn)) = self.gate {
            self.open
                .get_or_init(|| recorder.ask(|open| Message::Hold(turn, open)))
                .await;
        }
        self.inner.text(chunk).await
    }
    async fn end_message(&self) -> Result<(), Fenced> {
        self.inner.end_message().await
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
        self.inner.checkpoint(request, cancel).await
    }
    async fn run(
        &self,
        input: TurnInput,
        sink: &dyn TurnSink,
        stop: else_api::app::providers::Stop,
    ) -> TurnOutcome {
        self.mailbox
            .send(Message::Record(Box::new(input.clone())))
            .await
            .unwrap();
        let gated = input.text.contains(GATE_EVERY_ATTEMPT)
            || (input.text.contains(GATE) && input.attempt == 1);
        let sink = Gated {
            inner: sink,
            gate: gated.then_some((self, input.turn_id)),
            open: tokio::sync::OnceCell::new(),
        };
        self.inner.run(input, &sink, stop).await
    }
}

pub struct Harness {
    pub app: TestApp,
    pub recorder: Arc<Recorder>,
}

pub async fn harness(max_active: usize) -> Harness {
    harness_with(max_active, None).await
}

pub async fn harness_with(
    max_active: usize,
    mail: Option<Arc<dyn else_api::connectors::types::MailTransport>>,
) -> Harness {
    harness_with_providers(max_active, mail, Vec::new()).await
}

/// A recorder harness whose registry also has `extra` providers after the
/// default recorder.
pub async fn harness_with_providers(
    max_active: usize,
    mail: Option<Arc<dyn else_api::connectors::types::MailTransport>>,
    extra: Vec<Arc<dyn Provider>>,
) -> Harness {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    sqlx::query("UPDATE installation SET default_provider_id = 'synthetic'")
        .execute(&db.pool)
        .await
        .unwrap();
    let (mailbox, mut messages) = mpsc::channel(64);
    tokio::spawn(async move {
        let mut state = State::default();
        while let Some(message) = messages.recv().await {
            state.handle(message);
        }
    });
    let recorder = Arc::new(Recorder {
        inner: Synthetic::new(Duration::from_millis(5)),
        mailbox,
    });
    let mut providers: Vec<Arc<dyn Provider>> = vec![recorder.clone()];
    providers.extend(extra);
    let registry = else_api::providers::Registry::new(providers);
    let app = match mail {
        Some(mail) => TestApp::with_mail(db, max_active, registry, mail),
        None => TestApp::with_registry(db, max_active, registry),
    };
    Harness { app, recorder }
}

impl Harness {
    pub async fn grant(&self, turn: Uuid) -> String {
        for _ in 0..400 {
            if let Some(token) = self.recorder.ask(|r| Message::Grant(turn, r)).await {
                return token;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("turn {turn} never started");
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
        self.app
            .wait_turn_for(&turn.to_string(), &["running"], Duration::from_secs(20))
            .await;
        (turn, self.grant(turn).await)
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
