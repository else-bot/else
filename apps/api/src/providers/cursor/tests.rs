use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{Value, json};

use super::*;
use crate::app::providers::{Fenced, Origin, Selection, SessionState, Steer};
use crate::db::task_secrets::{self, Meta, Mode};
use crate::providers::cli::attempt::{Event, Protocol};

const MODEL: &str = "composer-2";

/// A server whose data directory holds the fake SDK where the real one is
/// installed, and a Vault that may hold a key.
struct Server {
    dir: tempfile::TempDir,
    cursor: Cursor,
}

impl Server {
    async fn new(key: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let sdk = cursor_home::sdk_dir(&data).join("node_modules/@cursor/sdk");
        std::fs::create_dir_all(&sdk).unwrap();
        std::fs::write(
            sdk.join("package.json"),
            json!({
                "name": "@cursor/sdk",
                "version": cursor_home::SDK_VERSION,
                "type": "module",
                "exports": "./index.mjs",
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(sdk.join("index.mjs"), include_str!("fake_sdk.mjs")).unwrap();
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let secret = {
            use base64::Engine;
            let text = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
            crate::credentials::SecretKey::parse(&text).unwrap()
        };
        let vault = Vault {
            pool,
            key: std::sync::Arc::new(secret),
        };
        if let Some(key) = key {
            let env = BTreeMap::from([(cursor_home::KEY_VAR.to_owned(), key.to_owned())]);
            let meta = Meta {
                description: None,
                website: None,
            };
            task_secrets::put(
                &vault.pool,
                &vault.key,
                cursor_home::VAULT_ENTRY,
                &env,
                &meta,
                Mode::CreateOnly,
            )
            .await
            .unwrap();
        }
        let host = Host::new(&data, &dir.path().join("bots"));
        let node = super::super::executable(None, "node")
            .unwrap()
            .expect("the Cursor tests run the sidecar with node");
        let cursor = Cursor {
            models: Models::new(&DEFINITION),
            ready: cursor_home::node(&host, Ok(Some(node))).map(|node| Ready {
                host,
                node,
                data_dir: data,
            }),
            vault: Some(vault),
            tool_url: Some("http://127.0.0.1:9/tools/v1/mcp".into()),
            installing: tokio::sync::Mutex::new(()),
        };
        Self { dir, cursor }
    }

    /// What the fake SDK was asked, in order.
    fn calls(&self) -> Vec<Value> {
        let home = self
            .dir
            .path()
            .join("data/bots")
            .join(Uuid::nil().to_string())
            .join("cursor-home");
        std::fs::read_to_string(home.join("calls.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    async fn run(&self, input: TurnInput, sink: &Sink, stop: Stop) -> TurnOutcome {
        self.cursor.run(input, sink, stop).await
    }
}

fn input(text: &str, origin: Origin) -> TurnInput {
    let mut input = crate::providers::cli::session::tests::input();
    input.text = text.into();
    input.delegation.origin = origin;
    input.tool_grant = Some("grant-1".into());
    input.profile = Some(Profile {
        model: MODEL.into(),
        reasoning_effort: "high".into(),
        service_tier: "default".into(),
    });
    input
}

#[derive(Default)]
struct Sink {
    sessions: Mutex<Vec<SessionState>>,
    text: Mutex<String>,
    steers: Mutex<Vec<Steer>>,
    steered: Mutex<Vec<Uuid>>,
    refused: Mutex<Vec<Uuid>>,
    retired: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl TurnSink for Sink {
    async fn packet(&self, _: &Selection, _: &str) -> Result<(), Fenced> {
        Ok(())
    }
    async fn sending(&self) -> Result<(), Fenced> {
        Ok(())
    }
    async fn running(&self) -> Result<(), Fenced> {
        Ok(())
    }
    async fn text(&self, chunk: &str) -> Result<(), Fenced> {
        self.text.lock().unwrap().push_str(chunk);
        Ok(())
    }
    async fn end_message(&self) -> Result<(), Fenced> {
        Ok(())
    }
    async fn session(&self, state: &SessionState) -> Result<(), Fenced> {
        self.sessions.lock().unwrap().push(state.clone());
        Ok(())
    }
    async fn reported(&self, _: &Value, _: &str) -> Result<(), Fenced> {
        Ok(())
    }
    async fn steers(&self) -> Vec<Steer> {
        std::mem::take(&mut *self.steers.lock().unwrap())
    }
    async fn steer_sending(&self, _: Uuid) -> Result<bool, Fenced> {
        Ok(true)
    }
    async fn steer_refused(&self, turn_id: Uuid) {
        self.refused.lock().unwrap().push(turn_id);
    }
    async fn retire_session(&self) {
        self.retired
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    async fn steered(&self, turn_id: Uuid) -> Result<(), Fenced> {
        self.steered.lock().unwrap().push(turn_id);
        Ok(())
    }
}

impl Sink {
    fn last_session(&self) -> SessionState {
        self.sessions.lock().unwrap().last().cloned().unwrap()
    }
}

#[tokio::test]
async fn an_owner_turn_creates_an_agent_and_the_next_turn_resumes_it_with_the_same_tools() {
    let server = Server::new(Some("test-key")).await;
    let sink = Sink::default();
    let outcome = server
        .run(input("hi", Origin::OwnerRequest), &sink, Stop::default())
        .await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(*sink.text.lock().unwrap(), "Hello there");
    let first = sink.last_session();

    let mut next = input("again", Origin::OwnerRequest);
    next.session = Some(first.clone());
    next.turn_no = 4;
    let sink = Sink::default();
    assert_eq!(
        server.run(next, &sink, Stop::default()).await,
        TurnOutcome::Completed
    );
    assert_eq!(sink.last_session().native_id, first.native_id);

    let calls = server.calls();
    let opens: Vec<&Value> = calls.iter().filter(|c| c["call"] != "send").collect();
    assert_eq!(opens.len(), 2);
    assert_eq!(opens[0]["call"], "create");
    assert_eq!(opens[1]["call"], "resume");
    assert_eq!(opens[1]["agentId"], first.native_id.as_str());
    for open in opens {
        let options = &open["options"];
        assert_eq!(
            options["tools"],
            json!([
                "shell", "read", "edit", "delete", "grep", "glob", "ls", "mcp"
            ])
        );
        assert_eq!(options["disallowedTools"], json!(["task"]));
        assert_eq!(options["local"]["settingSources"], json!([]));
        assert_eq!(
            options["model"],
            json!({"id": "composer-2", "params": [{"id": "effort", "value": "high"}, {"id": "fast", "value": "false"}]})
        );
        assert_eq!(
            options["mcpServers"]["elsebot"]["headers"]["Authorization"],
            "Bearer grant-1"
        );
        assert!(
            open["store"]
                .as_str()
                .unwrap()
                .ends_with("cursor-home/agents")
        );
    }
    let sends: Vec<&str> = calls
        .iter()
        .filter(|c| c["call"] == "send")
        .map(|c| c["message"]["text"].as_str().unwrap())
        .collect();
    assert!(sends[0].contains("[else.bot instructions]"), "{}", sends[0]);
    assert!(
        !sends[1].contains("[else.bot instructions]"),
        "the resumed agent holds them: {}",
        sends[1]
    );
}

#[tokio::test]
async fn a_recorded_agent_the_store_lost_is_replaced_by_a_new_one() {
    let server = Server::new(Some("test-key")).await;
    let sink = Sink::default();
    assert_eq!(
        server
            .run(input("hi", Origin::OwnerRequest), &sink, Stop::default())
            .await,
        TurnOutcome::Completed
    );
    let mut lost = sink.last_session();
    lost.native_id = "agent-gone".into();
    let mut next = input("again", Origin::OwnerRequest);
    next.session = Some(lost);
    let sink = Sink::default();
    assert_eq!(
        server.run(next, &sink, Stop::default()).await,
        TurnOutcome::Completed
    );
    assert_ne!(sink.last_session().native_id, "agent-gone");
    let sends: Vec<Value> = server
        .calls()
        .into_iter()
        .filter(|c| c["call"] == "send")
        .collect();
    assert!(
        sends[1]["message"]["text"]
            .as_str()
            .unwrap()
            .contains("[else.bot instructions]"),
        "a new agent gets the instructions again"
    );
}

#[tokio::test]
async fn an_event_turn_offers_only_else_bot_tools_and_a_shell_call_ends_it() {
    let server = Server::new(Some("test-key")).await;
    let outcome = server
        .run(
            input("SHELL", Origin::ConnectorEvent),
            &Sink::default(),
            Stop::default(),
        )
        .await;
    assert!(
        matches!(&outcome, TurnOutcome::Failed { code, message } if code == "policy_violation" && message.contains("shell")),
        "{outcome:?}"
    );
    let create = &server.calls()[0];
    assert_eq!(create["options"]["tools"], json!(["mcp"]));
    assert_eq!(create["options"]["disallowedTools"], json!(["task"]));
}

#[tokio::test]
async fn a_usage_limit_fails_the_turn_as_provider_unavailable() {
    let server = Server::new(Some("test-key")).await;
    let outcome = server
        .run(
            input("LIMIT", Origin::OwnerRequest),
            &Sink::default(),
            Stop::default(),
        )
        .await;
    assert!(
        matches!(&outcome, TurnOutcome::Failed { code, message } if code == "provider_unavailable" && message.contains("usage limit")),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn without_a_working_key_turns_and_listing_name_the_vault_step() {
    for key in [None, Some("wrong-key")] {
        let server = Server::new(key).await;
        let outcome = server
            .run(
                input("hi", Origin::OwnerRequest),
                &Sink::default(),
                Stop::default(),
            )
            .await;
        assert!(
            matches!(&outcome, TurnOutcome::Failed { code, message } if code == "provider_unavailable" && message.contains("Settings › Vault")),
            "{key:?}: {outcome:?}"
        );
        let listed = server.cursor.list_models().await.unwrap_err();
        assert!(listed.contains("Settings › Vault"), "{key:?}: {listed}");
    }
}

#[tokio::test]
async fn the_account_s_models_list_with_the_efforts_and_tiers_their_parameters_take() {
    let server = Server::new(Some("test-key")).await;
    let listed = server.cursor.list_models().await.unwrap();
    assert_eq!(
        listed,
        vec![
            Listed {
                id: "composer-2".into(),
                efforts: Some(vec!["default".into(), "low".into(), "high".into()]),
                tiers: Some(vec!["default".into(), "priority".into()]),
            },
            Listed {
                id: "plain".into(),
                efforts: Some(vec!["default".into()]),
                tiers: Some(vec!["default".into()]),
            },
            Listed {
                id: "constrained".into(),
                efforts: Some(vec!["default".into(), "low".into(), "high".into()]),
                tiers: Some(vec!["default".into(), "priority".into()]),
            },
        ]
    );
}

#[tokio::test]
async fn stop_cancels_the_running_run() {
    let server = Server::new(Some("test-key")).await;
    let stop = Stop::default();
    let sink = Sink::default();
    let run = server.run(input("SLOW", Origin::OwnerRequest), &sink, stop.clone());
    let request = async {
        while !sink.text.lock().unwrap().contains("working") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop.request();
    };
    let (outcome, ()) = tokio::join!(run, request);
    assert_eq!(outcome, TurnOutcome::Interrupted);
    assert!(server.calls().iter().any(|c| c["call"] == "cancel"));
}

#[tokio::test]
async fn a_joined_owner_message_steers_the_running_run() {
    let server = Server::new(Some("test-key")).await;
    let stop = Stop::default();
    let sink = Sink::default();
    let joined = Uuid::new_v4();
    let run = server.run(input("SLOW", Origin::OwnerRequest), &sink, stop.clone());
    let steer = async {
        while !sink.text.lock().unwrap().contains("working") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        sink.steers.lock().unwrap().push(steer(joined));
        stop.steer();
    };
    let (outcome, ()) = tokio::join!(run, steer);
    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(*sink.steered.lock().unwrap(), vec![joined]);
    assert!(sink.text.lock().unwrap().contains("steered: "));
    assert!(
        server
            .calls()
            .iter()
            .any(|c| c["call"] == "steer" && c["text"].as_str().unwrap().contains("use tabs"))
    );
}

#[test]
fn an_else_bot_tool_that_parks_parks_the_turn_and_another_server_is_refused() {
    let profile = Profile {
        model: MODEL.into(),
        reasoning_effort: "default".into(),
        service_tier: "default".into(),
    };
    let mut tracker = Tracker::new(&profile, true, true);
    let call = |server: &str, status: &str, result: Value| Incoming::Notification {
        method: "run/message".into(),
        params: json!({ "message": {
            "type": "tool_call", "call_id": "c1", "name": "mcp", "status": status,
            "args": { "providerIdentifier": server, "toolName": "mcp_wait" },
            "result": result,
        }}),
    };
    let parked = json!({ "status": "success", "value": {
        "content": [{ "text": { "text": "{\"parked\":true}" } }], "isError": false } });
    assert_eq!(
        tracker
            .on(&call("elsebot", "running", Value::Null))
            .unwrap(),
        vec![]
    );
    assert_eq!(
        tracker
            .on(&call("elsebot", "completed", parked.clone()))
            .unwrap(),
        vec![Event::Parked]
    );
    let done = json!({ "status": "success", "value": {
        "content": [{ "text": { "text": "{\"parked\":false}" } }], "isError": false } });
    let mut fresh = Tracker::new(&profile, true, true);
    assert_eq!(
        fresh.on(&call("elsebot", "completed", done)).unwrap(),
        vec![]
    );
    let refused = fresh
        .on(&call("github", "running", Value::Null))
        .unwrap_err();
    assert_eq!(refused.code, "policy_violation");
}

#[test]
fn node_older_than_the_sdk_needs_is_unavailable_with_the_fix() {
    let dir = tempfile::tempdir().unwrap();
    let node = dir.path().join("node");
    std::fs::write(&node, "#!/bin/sh\necho v20.11.0\n").unwrap();
    let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&node, mode).unwrap();
    let host = Host::new(&dir.path().join("data"), &dir.path().join("bots"));
    let why = cursor_home::node(&host, Ok(Some(node))).unwrap_err();
    assert!(
        why.contains("22.13") && why.contains("ELSE_NODE_BIN"),
        "{why}"
    );
}

fn steer(turn_id: Uuid) -> Steer {
    Steer {
        turn_id,
        text: "use tabs".into(),
        parts: Vec::new(),
        files: Vec::new(),
    }
}

#[tokio::test]
async fn a_steer_cursor_turns_down_waits_for_the_next_attempt() {
    let server = Server::new(Some("test-key")).await;
    let stop = Stop::default();
    let sink = Sink::default();
    let joined = Uuid::new_v4();
    let run = server.run(
        input("SLOW REVERT", Origin::OwnerRequest),
        &sink,
        stop.clone(),
    );
    let steer_then_stop = async {
        while !sink.text.lock().unwrap().contains("working") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        sink.steers.lock().unwrap().push(steer(joined));
        stop.steer();
        while sink.refused.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop.request();
    };
    let (outcome, ()) = tokio::join!(run, steer_then_stop);
    assert_eq!(outcome, TurnOutcome::Interrupted);
    assert_eq!(*sink.refused.lock().unwrap(), vec![joined]);
    assert!(sink.steered.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_changed_profile_starts_a_new_agent_instead_of_resuming() {
    let server = Server::new(Some("test-key")).await;
    let sink = Sink::default();
    assert_eq!(
        server
            .run(input("hi", Origin::OwnerRequest), &sink, Stop::default())
            .await,
        TurnOutcome::Completed
    );
    let mut next = input("again", Origin::OwnerRequest);
    next.session = Some(sink.last_session());
    next.profile.as_mut().unwrap().reasoning_effort = "low".into();
    assert_eq!(
        server.run(next, &Sink::default(), Stop::default()).await,
        TurnOutcome::Completed
    );
    let opens: Vec<Value> = server
        .calls()
        .into_iter()
        .filter(|c| c["call"] != "send")
        .collect();
    assert_eq!(opens[1]["call"], "create");
}

#[tokio::test]
async fn a_profile_the_model_s_variants_do_not_offer_is_refused_before_delivery() {
    let server = Server::new(Some("test-key")).await;
    server
        .cursor
        .models
        .listed(
            server.cursor.list_models().await.unwrap(),
            chrono::Utc::now(),
        )
        .unwrap();
    let mut turn = input("hi", Origin::OwnerRequest);
    turn.profile = Some(Profile {
        model: "constrained".into(),
        reasoning_effort: "high".into(),
        service_tier: "priority".into(),
    });
    let outcome = server.run(turn, &Sink::default(), Stop::default()).await;
    assert!(
        matches!(&outcome, TurnOutcome::Failed { code, message } if code == "provider_refused" && message.contains("no high effort in Fast mode")),
        "{outcome:?}"
    );
    let mut turn = input("hi", Origin::OwnerRequest);
    turn.profile = Some(Profile {
        model: "constrained".into(),
        reasoning_effort: "default".into(),
        service_tier: "priority".into(),
    });
    assert_eq!(
        server.run(turn, &Sink::default(), Stop::default()).await,
        TurnOutcome::Completed
    );
    let create = server
        .calls()
        .into_iter()
        .find(|c| c["call"] == "create")
        .unwrap();
    assert_eq!(
        create["options"]["model"]["params"],
        json!([{"id": "reasoning", "value": "low"}, {"id": "fast", "value": "true"}])
    );
}

#[tokio::test]
async fn a_stream_that_breaks_without_a_result_is_uncertain_and_retires_the_agent() {
    let server = Server::new(Some("test-key")).await;
    let sink = Sink::default();
    let outcome = server
        .run(input("LOST", Origin::OwnerRequest), &sink, Stop::default())
        .await;
    assert!(
        matches!(&outcome, TurnOutcome::Uncertain { message } if message.contains("connection reset")),
        "{outcome:?}"
    );
    assert!(sink.retired.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn bots_cannot_read_the_cursor_key_from_the_vault() {
    let server = Server::new(Some("test-key")).await;
    let vault = server.cursor.vault.as_ref().unwrap();
    let names = [cursor_home::VAULT_ENTRY.to_owned()];
    assert!(matches!(
        task_secrets::resolve(&vault.pool, Some(&vault.key), &names).await,
        Err(task_secrets::SecretError::Unknown(_))
    ));
    assert!(matches!(
        task_secrets::login(&vault.pool, Some(&vault.key), cursor_home::VAULT_ENTRY).await,
        Err(task_secrets::SecretError::Unknown(_))
    ));
}
