use super::*;
use serde_json::{Value, json};

fn tracker(host: bool) -> Tracker {
    Tracker::new("grok-4.7", "session", host, true)
}
fn init(host: bool) -> Value {
    let mut tools = grok_home::MCP_TOOLS.to_vec();
    if host {
        tools.extend(grok_home::HOST_TOOL_NAMES);
    }
    json!({"type":"system","subtype":"init","session_id":"session","model":"grok-4.7","tools":tools,"mcp_servers":[{"name":"elsebot","status":"connected"}]})
}

#[test]
fn text_is_streamed_once_and_only_a_terminal_result_completes() {
    let mut t = tracker(true);
    t.on(&init(true)).unwrap();
    assert!(!t.accepted);
    assert_eq!(t.on(&json!({"type":"stream_event","event":{"type":"message_start","message":{"model":"grok-4.7"}}})).unwrap(), vec![Step::Accepted]);
    assert_eq!(t.on(&json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}})).unwrap(), vec![Step::Text("hello".into())]);
    assert!(t.on(&json!({"type":"assistant","message":{"model":"grok-4.7","content":[{"type":"text","text":"hello"}]}})).unwrap().is_empty());
    assert_eq!(
        t.on(
            &json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn"})
        )
        .unwrap(),
        vec![Step::Result(TurnOutcome::Completed)]
    );
}

#[test]
fn inventories_models_subagents_and_sessions_are_checked() {
    for event in [
        {
            let mut e = init(true);
            e["tools"]
                .as_array_mut()
                .unwrap()
                .push(json!("spawn_subagent"));
            e
        },
        {
            let mut e = init(true);
            e["mcp_servers"][0]["status"] = json!("failed");
            e
        },
        {
            let mut e = init(true);
            e["mcp_servers"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":"other","status":"connected"}));
            e
        },
        {
            let mut e = init(true);
            e["model"] = json!("grok-4.6");
            e
        },
        json!({"type":"assistant","parent_tool_use_id":"child"}),
        json!({"type":"system","subtype":"task_started"}),
        json!({"type":"assistant","session_id":"another"}),
        json!({"type":"control_request"}),
    ] {
        assert!(tracker(true).on(&event).is_err(), "{event}");
    }
    assert!(tracker(false).on(&init(true)).is_err());
    assert!(tracker(false).on(&init(false)).is_ok());
}

#[test]
fn truncated_refused_or_cancelled_results_do_not_complete() {
    for reason in ["max_tokens", "max_turn_requests", "refusal", "cancelled"] {
        let mut t = tracker(true);
        t.on(&init(true)).unwrap();
        t.on(&json!({"type":"stream_event","event":{"type":"message_start"}}))
            .unwrap();
        let steps = t
            .on(&json!({"type":"result","subtype":"success","is_error":false,"stop_reason":reason}))
            .unwrap();
        assert_ne!(steps, vec![Step::Result(TurnOutcome::Completed)]);
    }
    assert!(matches!(tracker(true).on(&json!({"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn"})).unwrap().as_slice(),[Step::Result(TurnOutcome::Uncertain{..})]));
}

#[test]
fn only_elsebot_mcp_calls_are_allowed_and_parking_is_preserved() {
    let mut t = tracker(false);
    t.on(&init(false)).unwrap();
    t.on(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"c1","name":"use_tool","input":{"tool_name":"elsebot__ask_owner"}}]}})).unwrap();
    assert_eq!(t.on(&json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"c1","content":"{\"parked\":true}"}]}})).unwrap(),vec![Step::Parked]);
    for name in ["other-ask_owner", "elsebot__not_real"] {
        assert!(t.on(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"c2","name":"use_tool","input":{"tool_name":name}}]}})).is_err());
    }
    assert!(t.on(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"run_terminal_command"}]}})).is_err());
}

#[test]
fn profiles_reject_settings_the_cli_cannot_apply() {
    let profile = Profile {
        model: "grok-4.7".into(),
        reasoning_effort: "high".into(),
        service_tier: "default".into(),
    };
    assert!(validate(&profile).is_ok());
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
        assert!(validate(&bad).is_err());
    }
    assert!(matches!(
        Grok::detect(None, None, None).availability(),
        Availability::Unavailable(_)
    ));
}

#[tokio::test]
async fn event_reads_survive_cancellation_and_reject_malformed_or_oversized_lines() {
    use tokio::io::AsyncWriteExt;
    let (mut writer, reader) = tokio::io::duplex(1024);
    let mut events = Events::new(BufReader::new(reader));
    writer.write_all(b"{\"type\":").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), events.next())
            .await
            .is_err()
    );
    writer.write_all(b"\"result\"}\n").await.unwrap();
    assert_eq!(events.next().await.unwrap(), Some(json!({"type":"result"})));
    assert!(Events::new(&b"oops\n"[..]).next().await.is_err());
    assert!(
        Events::new(&vec![b'x'; 8 * 1024 * 1024][..])
            .next()
            .await
            .is_err()
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
fn pending_mcp_and_dynamic_direct_tools_are_valid_but_foreign_tools_are_not() {
    let mut inventory = init(true);
    inventory["mcp_servers"][0]["status"] = json!("pending");
    inventory["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!("elsebot__list_bots"));
    let mut t = tracker(true);
    t.on(&inventory).unwrap();
    t.on(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"direct","name":"elsebot__ask_owner","input":{}}]}})).unwrap();
    assert_eq!(t.on(&json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"direct","content":"{\"parked\":true}"}]}})).unwrap(),vec![Step::Parked]);
    inventory["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!("other__list_bots"));
    assert!(tracker(true).on(&inventory).is_err());
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
