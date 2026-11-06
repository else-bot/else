use super::*;
use crate::config::{Origin, OwnerToken};
use axum::{
    Json, Router,
    http::StatusCode,
    response::IntoResponse,
    routing::{delete, get, post},
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::{net::TcpListener, sync::mpsc};

struct Server {
    flow: Flow,
    answers: mpsc::Receiver<Value>,
    cancellations: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server(status: Value, fail_answers: usize) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let (sent, answers) = mpsc::channel(8);
    let tries = Arc::new(AtomicUsize::new(0));
    let cancellations = Arc::new(AtomicUsize::new(0));
    let cancels = cancellations.clone();
    let read_status = status.clone();
    let router = Router::new()
        .route("/v1/bots/{bot}/mcp/fixture/connection", get(move || {
            let status = read_status.clone();
            async move { respond(status) }
        }))
        .route("/v1/bots/{bot}/mcp/fixture/oauth", delete(move || {
            let status = status.clone(); let cancels = cancels.clone();
            async move { cancels.fetch_add(1, Ordering::SeqCst); respond(status) }
        }))
        .route("/v1/bots/{bot}/questions/{question}/answer", post(move |Json(body): Json<Value>| {
            let sent = sent.clone(); let tries = tries.clone();
            async move {
                sent.send(body).await.unwrap();
                if tries.fetch_add(1, Ordering::SeqCst) < fail_answers {
                    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":{"code":"unavailable","message":"Confirmation unavailable"}}))).into_response()
                } else { Json(json!({"requeued":true})).into_response() }
            }
        }));
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let token = OwnerToken::parse(&"T".repeat(43)).unwrap();
    let client = Arc::new(Client::new(origin, &token, tokio::runtime::Handle::current()).unwrap());
    Server {
        flow: Flow {
            client,
            bot: Uuid::new_v4(),
            question: Uuid::new_v4(),
            consent: McpConsent {
                id: "fixture".into(),
                provider: Some("https://issuer.test".into()),
                scopes: vec!["read".into()],
            },
        },
        answers,
        cancellations,
        task,
    }
}
fn respond(status: Value) -> axum::response::Response {
    if status.is_null() {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error":{"code":"not_found","message":"MCP removed"}})),
        )
            .into_response()
    } else {
        Json(status).into_response()
    }
}

fn status(state: &str) -> Value {
    json!({"status":state,"provider":"https://issuer.test","scopes":["read"]})
}

#[tokio::test]
async fn failed_confirmation_retries_the_card_without_starting_oauth() {
    let mut server = server(status("connected"), 1).await;
    assert!(matches!(
        server.flow.recover().await.unwrap(),
        Resolution::ConfirmationRequired(_)
    ));
    assert_eq!(server.answers.recv().await.unwrap()["choice"], APPROVE);
    assert_eq!(server.flow.confirm().await.unwrap(), Resolution::Recorded);
    assert_eq!(server.answers.recv().await.unwrap()["choice"], APPROVE);
    assert_eq!(server.cancellations.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn reopening_a_connected_card_confirms_and_cancel_never_records_denial() {
    let mut server = server(status("connected"), 0).await;
    assert_eq!(server.flow.recover().await.unwrap(), Resolution::Recorded);
    assert_eq!(server.answers.recv().await.unwrap()["choice"], APPROVE);
    assert!(matches!(
        server.flow.cancel().await.unwrap(),
        Resolution::ConfirmationRequired(_)
    ));
    assert!(server.answers.try_recv().is_err());
}
#[tokio::test]
async fn disconnected_cards_cancel_the_pending_flow_before_denial() {
    let mut server = server(status("disconnected"), 0).await;
    assert_eq!(
        server.flow.recover().await.unwrap(),
        Resolution::Disconnected
    );
    assert!(server.answers.try_recv().is_err());
    assert_eq!(server.flow.cancel().await.unwrap(), Resolution::Recorded);
    assert_eq!(server.cancellations.load(Ordering::SeqCst), 1);
    assert_eq!(server.answers.recv().await.unwrap()["choice"], DENY);
}
#[tokio::test]
async fn unknown_connection_state_cannot_settle_a_card() {
    let mut server = server(json!({"status":"unknown"}), 0).await;
    assert!(server.flow.recover().await.is_err());
    assert!(server.flow.cancel().await.is_err());
    assert!(server.answers.try_recv().is_err());
}

#[tokio::test]
async fn another_shared_account_cannot_approve_the_card_but_can_decline_it() {
    for current in [
        json!({"status":"connected","provider":"https://other.test","scopes":["read"]}),
        json!({"status":"connected","provider":"https://issuer.test","scopes":[]}),
    ] {
        let mut server = server(current, 0).await;
        assert!(server.flow.recover().await.is_err());
        assert!(server.answers.try_recv().is_err());
        assert_eq!(server.flow.cancel().await.unwrap(), Resolution::Recorded);
        assert_eq!(server.answers.recv().await.unwrap()["choice"], DENY);
    }
}

#[tokio::test]
async fn a_broader_shared_account_approves_the_card() {
    let mut server = server(
        json!({"status":"connected","provider":"https://issuer.test","scopes":["read","write"]}),
        0,
    )
    .await;
    assert_eq!(server.flow.recover().await.unwrap(), Resolution::Recorded);
    assert_eq!(server.answers.recv().await.unwrap()["choice"], APPROVE);
}

#[tokio::test]
async fn a_removed_package_leaves_the_card_cancellable() {
    let mut server = server(Value::Null, 0).await;
    assert_eq!(
        server.flow.recover().await.unwrap(),
        Resolution::Disconnected
    );
    assert_eq!(server.flow.cancel().await.unwrap(), Resolution::Recorded);
    assert_eq!(server.answers.recv().await.unwrap()["choice"], DENY);
}
