use super::*;
use crate::app::providers::Origin;
use crate::providers::cli::attempt::{Event, Protocol, StopCause};
use protocol::Terminal;
use serde_json::{Value, json};

fn tracker(host: bool) -> Tracker {
    Tracker::new("grok-4.7", "session", host, true)
}

fn init(host: bool) -> Value {
    let mut tools = grok_home::MCP_TOOLS.to_vec();
    if host {
        tools.extend(grok_home::HOST_TOOLS.iter().map(|(_, name)| *name));
    }
    json!({"type":"system","subtype":"init","session_id":"session","model":"grok-4.7","tools":tools,"mcp_servers":[{"name":"elsebot","status":"connected"}]})
}

fn result(stop_reason: &str) -> Value {
    json!({"type":"result","subtype":"success","is_error":false,"stop_reason":stop_reason})
}

fn finish(t: &mut Tracker, event: &Value, stopping: Option<StopCause>) -> TurnOutcome {
    match t.on(event).unwrap().pop() {
        Some(Event::Finished(terminal)) => t.outcome(terminal, stopping, true),
        other => panic!("not terminal: {other:?}"),
    }
}

#[test]
fn a_native_grok_on_path_wins_and_the_installed_binary_replaces_anything_else() {
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str, body: &str, mode: u32| {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::from_mode(mode);
        std::fs::set_permissions(&path, mode).unwrap();
        path
    };
    let native = file("path/grok", "\x7fELF", 0o755);
    let script = file("npm/.bin/grok", "#!/bin/sh\nexec node grok.js\n", 0o755);
    let home = dir.path().join("home");
    assert_eq!(installed(&home), None);
    file("home/bin/grok", "\x7fELF", 0o644);
    assert_eq!(installed(&home), None);
    let binary = file("home/bin/grok", "\x7fELF", 0o755);
    assert_eq!(installed(&home), Some(binary.clone()));

    let some = |path: &PathBuf| Some(path.clone());
    assert_eq!(choose(Ok(some(&native)), some(&binary)), Ok(some(&native)));
    assert_eq!(choose(Ok(some(&script)), some(&binary)), Ok(some(&binary)));
    assert_eq!(choose(Ok(None), some(&binary)), Ok(some(&binary)));
    assert_eq!(choose(Err("shim".into()), some(&binary)), Ok(some(&binary)));
    assert_eq!(choose(Ok(some(&script)), None), Ok(some(&script)));
    assert_eq!(choose(Err("shim".into()), None), Err("shim".into()));
}

#[test]
fn a_configured_npm_script_gives_way_to_the_installed_binary() {
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str, body: &str| {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
        std::fs::set_permissions(&path, mode).unwrap();
        std::fs::canonicalize(path).unwrap()
    };
    let script = file("npm/.bin/grok", "#!/bin/sh\nexec node grok.js\n");
    let native = file("opt/grok", "\x7fELF");
    let home = dir.path().join("home");
    assert_eq!(lookup(Some(&script), Some(&home)), Ok(Some(script.clone())));
    let binary = file("home/bin/grok", "\x7fELF");
    let home = std::fs::canonicalize(home).unwrap();
    assert_eq!(lookup(Some(&script), Some(&home)), Ok(Some(binary)));
    assert_eq!(lookup(Some(&native), Some(&home)), Ok(Some(native)));
    let missing = dir.path().join("missing/grok");
    assert!(lookup(Some(&missing), Some(&home)).is_err());
}

#[test]
fn text_is_streamed_once_and_only_a_terminal_result_completes() {
    let mut t = tracker(true);
    t.on(&init(true)).unwrap();
    assert_eq!(
        t.on(&json!({"type":"stream_event","event":{"type":"message_start","message":{"model":"grok-4.7"}}})),
        Ok(vec![Event::Accepted { model: "grok-4.7".into(), native_turn: "session".into() }])
    );
    assert_eq!(
        t.on(&json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}})),
        Ok(vec![Event::Text("hello".into())])
    );
    assert_eq!(
        t.on(&json!({"type":"assistant","message":{"model":"grok-4.7","content":[{"type":"text","text":"hello"}]}})),
        Ok(vec![])
    );
    assert_eq!(
        finish(&mut t, &result("end_turn"), None),
        TurnOutcome::Completed
    );
    assert!(t.resumable(&TurnOutcome::Completed));
    assert!(
        !t.resumable(&TurnOutcome::Interrupted),
        "only a completed native turn is resumed"
    );
}

#[test]
fn inventories_models_subagents_and_sessions_are_checked() {
    let edit = |change: fn(&mut Value)| {
        let mut event = init(true);
        change(&mut event);
        event
    };
    for event in [
        edit(|e| {
            e["tools"]
                .as_array_mut()
                .unwrap()
                .push(json!("spawn_subagent"))
        }),
        edit(|e| {
            e["tools"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name": "spawn_subagent"}))
        }),
        edit(|e| e["tools"].as_array_mut().unwrap().push(json!(7))),
        edit(|e| e["tools"] = json!("search_tool")),
        edit(|e| e["mcp_servers"][0]["status"] = json!("failed")),
        edit(|e| e["mcp_servers"][0]["name"] = json!(["elsebot"])),
        edit(|e| {
            e["mcp_servers"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":"other","status":"connected"}))
        }),
        edit(|e| {
            e["mcp_servers"]
                .as_array_mut()
                .unwrap()
                .push(json!("elsebot"))
        }),
        edit(|e| e["model"] = json!("grok-4.6")),
        edit(|e| e["model"] = json!(4.7)),
        edit(|e| e["session_id"] = json!(1)),
        json!({"type":"assistant","parent_tool_use_id":"child"}),
        json!({"type":"system","subtype":"task_started"}),
        json!({"type":"assistant","session_id":"another"}),
        json!({"type":"control_request"}),
        json!({"subtype":"init"}),
    ] {
        assert!(tracker(true).on(&event).is_err(), "{event}");
    }
    assert!(tracker(false).on(&init(true)).is_err());
    assert!(tracker(false).on(&init(false)).is_ok());
    let mut t = tracker(true);
    t.on(&init(true)).unwrap();
    assert!(t.on(&init(true)).is_err(), "a second inventory is refused");
}

#[test]
fn errors_are_classified_by_their_cause() {
    let model = tracker(true)
        .on(&json!({"type":"assistant","message":{"model":"grok-4.6","content":[]}}))
        .unwrap_err();
    assert_eq!(model.code, "profile_mismatch");
    let tool = tracker(true)
        .on(&json!({"type":"assistant","message":{"content":[{"type":"server_tool_use"}]}}))
        .unwrap_err();
    assert_eq!(tool.code, "policy_violation");
}

#[test]
fn a_grok_that_fails_before_opening_a_session_reports_its_own_error() {
    // Grok 1.0.77's output, verbatim apart from usage fields, for an expired login.
    let unopened = json!({"type":"system","subtype":"init","session_id":"","apiKeySource":"user","model":"unknown","cwd":"","permissionMode":"default","tools":[],"slash_commands":[],"mcp_servers":[],"skills":[],"uuid":"82a378fb-dfaf-4779-b369-29be16172cff"});
    let failed = json!({"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":0,"num_turns":0,"stop_reason":null,"errors":["Not signed in. To authenticate without a browser, run:\n  grok login --device-code"],"session_id":"","uuid":"920e941e-75bd-40da-a5a9-837d4fc0308e"});
    let output = json!({"type":"assistant","session_id":"","message":{"content":[]}});

    let mut t = tracker(true);
    assert_eq!(t.on(&unopened), Ok(vec![]));
    assert!(t.on(&output).is_err(), "no output without a session");
    let Some(Event::Finished(terminal)) = t.on(&failed).unwrap().pop() else {
        panic!("terminal");
    };
    match t.outcome(terminal, None, false) {
        TurnOutcome::Failed { code, message } => {
            assert_eq!(code, "provider_failed");
            assert!(message.contains("Not signed in."), "{message}");
            assert!(!message.contains("[\""), "{message}");
        }
        other => panic!("{other:?}"),
    }

    let mut t = tracker(true);
    t.on(&init(true)).unwrap();
    assert!(
        t.on(&output).is_err(),
        "an opened session cannot lose its ID"
    );
}

#[test]
fn malformed_known_events_fail_and_unknown_events_are_ignored() {
    let mut t = tracker(true);
    t.on(&init(true)).unwrap();
    for event in [
        json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":7}}}),
        json!({"type":"stream_event","event":{}}),
        json!({"type":"assistant","message":{}}),
        json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"use_tool","id":"c1","input":{}}]}}),
        json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"read_file"}]}}),
        json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":5}]}}),
        json!({"type":"result","subtype":1}),
        json!({"type":7}),
    ] {
        assert!(t.on(&event).is_err(), "{event}");
    }
    assert_eq!(
        t.on(&json!({"type":"progress","anything":[1, 2]})),
        Ok(vec![])
    );
    assert_eq!(
        t.on(&json!({"type":"system","subtype":"status"})),
        Ok(vec![])
    );
}

#[test]
fn truncated_refused_or_cancelled_results_do_not_complete() {
    for reason in ["max_tokens", "max_turn_requests", "refusal", "cancelled"] {
        let mut t = tracker(true);
        t.on(&init(true)).unwrap();
        assert_ne!(
            finish(&mut t, &result(reason), None),
            TurnOutcome::Completed
        );
    }
    let mut t = tracker(true);
    t.on(&init(true)).unwrap();
    assert!(matches!(
        finish(&mut t, &result("cancelled"), None),
        TurnOutcome::Failed { code, .. } if code == "provider_refused"
    ));
    assert_eq!(
        finish(&mut t, &result("cancelled"), Some(StopCause::Owner)),
        TurnOutcome::Interrupted
    );
    let mut fresh = tracker(true);
    let Some(Event::Finished(terminal)) = fresh.on(&result("end_turn")).unwrap().pop() else {
        panic!("terminal");
    };
    assert!(matches!(
        fresh.outcome(terminal, None, false),
        TurnOutcome::Uncertain { .. }
    ));
    let mut t = tracker(true);
    assert_eq!(
        t.on(&json!({"type":"error","message":"boom"})),
        Ok(vec![Event::Finished(Terminal::Error("boom".into()))])
    );
}

#[test]
fn only_elsebot_mcp_calls_are_allowed_and_parking_is_preserved() {
    let mut t = tracker(false);
    t.on(&init(false)).unwrap();
    t.on(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"c1","name":"use_tool","input":{"tool_name":"elsebot__ask_owner"}}]}})).unwrap();
    assert_eq!(
        t.on(&json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"c1","content":"{\"parked\":true}"}]}})),
        Ok(vec![Event::Parked])
    );
    for name in ["other-ask_owner", "elsebot__not_real"] {
        assert!(t.on(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"c2","name":"use_tool","input":{"tool_name":name}}]}})).is_err());
    }
    assert!(t.on(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"c3","name":"run_terminal_command"}]}})).is_err());
}

#[test]
fn profiles_reject_settings_the_cli_cannot_apply() {
    let profile = Profile {
        model: "grok-4.7".into(),
        reasoning_effort: "high".into(),
        service_tier: "default".into(),
    };
    assert!(DEFINITION.validate(&profile).is_ok());
    for bad in [
        Profile {
            model: "grok".into(),
            ..profile.clone()
        },
        Profile {
            reasoning_effort: "max".into(),
            ..profile.clone()
        },
        Profile {
            service_tier: "priority".into(),
            ..profile
        },
    ] {
        assert!(DEFINITION.validate(&bad).is_err());
    }
    assert!(matches!(
        Grok::detect(None, None, None).availability(),
        Availability::Unavailable(_)
    ));
}

#[test]
fn the_fingerprint_follows_the_account_not_the_login_path() {
    let root = tempfile::tempdir().unwrap();
    let login = root.path().join("login");
    std::fs::create_dir(&login).unwrap();
    let auth = |user: &str| {
        let entry =
            json!({"key": "secret", "refresh_token": "secret", "user_id": user, "team_id": "t"});
        std::fs::write(
            login.join("auth.json"),
            json!({"https://auth.x.ai::id": entry}).to_string(),
        )
        .unwrap();
    };
    let grok = Grok {
        ready: Ok(Ready {
            host: Host::new(&root.path().join("data"), &root.path().join("bots")),
            program: PathBuf::new(),
            version: "grok 1.0".into(),
            login: login.clone(),
        }),
        tool_url: Some("http://127.0.0.1:1/mcp".into()),
    };
    let ready = grok.ready.as_ref().unwrap();
    let mut input = crate::providers::cli::session::tests::input();
    let profile = Profile {
        model: "grok-4.7".into(),
        reasoning_effort: "high".into(),
        service_tier: "default".into(),
    };
    auth("alice");
    let alice = grok.fingerprint(ready, &input, &profile);
    assert_eq!(alice, grok.fingerprint(ready, &input, &profile));
    assert!(!grok_home::host_account(&login).contains("secret"));
    auth("bob");
    assert_ne!(alice, grok.fingerprint(ready, &input, &profile), "account");
    let bob = grok.fingerprint(ready, &input, &profile);
    input.delegation.origin = Origin::ConnectorEvent;
    assert_ne!(
        bob,
        grok.fingerprint(ready, &input, &profile),
        "tool policy"
    );
}

#[test]
fn provider_homes_link_login_and_keep_grants_out_of_config() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let login = root.path().join("login");
    std::fs::create_dir(&login).unwrap();
    std::fs::write(login.join("auth.json"), "login").unwrap();
    let host = Host::new(&root.path().join("data"), &root.path().join("bots"));
    let dirs = host.bot_dirs(Uuid::new_v4(), ID).unwrap();
    grok_home::prepare(
        &dirs,
        &login,
        Some("http://127.0.0.1:1234/mcp"),
        "test prompt",
        true,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_link(dirs.provider_home.join("auth.json")).unwrap(),
        login.join("auth.json")
    );
    let config = std::fs::read_to_string(dirs.provider_home.join("config.toml")).unwrap();
    assert!(config.contains("${ELSE_TASK_GRANT}"));
    assert_eq!(
        std::fs::metadata(dirs.provider_home.join("config.toml"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    grok_home::prepare(&dirs, &login, None, "next prompt", false).unwrap();
    assert!(
        !std::fs::read_to_string(dirs.provider_home.join("config.toml"))
            .unwrap()
            .contains("mcp_servers")
    );
    assert!(!grok_home::session_exists(
        &dirs.provider_home,
        &dirs.project,
        "../../session"
    ));
}

#[test]
fn hashed_session_groups_resume_only_their_original_directory() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().canonicalize().unwrap();
    let id = Uuid::new_v4().to_string();
    let group = root.path().join("sessions/slug-hash");
    std::fs::create_dir_all(group.join(&id)).unwrap();
    std::fs::write(group.join(".cwd"), project.to_str().unwrap()).unwrap();
    std::fs::write(group.join(&id).join("chat_history.jsonl"), "{}").unwrap();
    assert!(grok_home::session_exists(root.path(), &project, &id));
    std::fs::write(group.join(".cwd"), "/other/project").unwrap();
    assert!(!grok_home::session_exists(root.path(), &project, &id));
}
