use super::*;
use crate::app::providers::MemoryContext;
use serde_json::Value;

fn tracker() -> Tracker {
    Tracker::new(claude_home::tool_names(), "claude-haiku-5-5", "u-1")
}

fn init(tools: Vec<String>, servers: Value) -> Value {
    json!({ "type": "system", "subtype": "init", "model": "claude-haiku-5-5",
                "tools": tools, "mcp_servers": servers,
                "agents": ["general-purpose", "Explore"] })
}

fn connected() -> Value {
    json!([{ "name": "elsebot", "status": "connected" }])
}

#[test]
fn a_turn_is_accepted_by_its_echo_and_streams_text() {
    let mut t = tracker();
    t.on(&init(claude_home::tool_names(), connected()));
    assert!(t.violation.is_none(), "{:?}", t.violation);
    // Another message's echo is not acceptance.
    assert!(
        t.on(&json!({"type":"user","uuid":"other","isReplay":true}))
            .is_empty()
    );
    assert_eq!(
        t.on(&json!({"type":"user","uuid":"u-1","isReplay":true})),
        vec![Step::Accepted]
    );
    t.on(&json!({"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-haiku-5-5"}}}));
    t.on(&json!({"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"text"}}}));
    let steps = t.on(&json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}}));
    assert_eq!(steps, vec![Step::Text("Hi".into())]);
    t.on(&json!({"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"text"}}}));
    let steps = t.on(&json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"again"}}}));
    assert_eq!(steps, vec![Step::EndMessage, Step::Text("again".into())]);
    let steps =
        t.on(&json!({"type":"result","subtype":"success","is_error":false,"result":"Hi\n\nagain"}));
    assert_eq!(
        steps,
        vec![Step::Result(ResultInfo {
            success: true,
            message: "Hi\n\nagain".into(),
            num_turns: None,
        })]
    );
    assert!(t.violation.is_none());
}

#[test]
fn an_agent_tool_or_extra_server_in_the_inventory_is_refused() {
    let mut tools = claude_home::tool_names();
    tools.push("Agent".into());
    let mut t = tracker();
    t.on(&init(tools, connected()));
    assert_eq!(t.violation.as_ref().unwrap().0, "policy_violation");
    let mut t = tracker();
    t.on(&init(
            claude_home::tool_names(),
            json!([{ "name": "elsebot", "status": "connected" }, { "name": "other", "status": "connected" }]),
        ));
    assert!(t.violation.is_some());
    // else.bot's server failing to connect is not a working inventory.
    let mut t = tracker();
    t.on(&init(
        claude_home::tool_names(),
        json!([{ "name": "elsebot", "status": "failed" }]),
    ));
    assert!(t.violation.is_some());
}

#[test]
fn the_built_in_agent_list_alone_is_not_a_violation_but_its_activity_is() {
    // The init event lists built-in agents even with no Agent tool.
    let mut t = tracker();
    t.on(&init(claude_home::tool_names(), connected()));
    assert!(t.violation.is_none());
    t.on(&json!({"type":"system","subtype":"task_started","task_id":"x"}));
    assert!(t.violation.as_ref().unwrap().1.contains("task_started"));
    let mut t = tracker();
    t.on(&json!({"type":"assistant","parent_tool_use_id":"toolu_1","message":{"model":"claude-haiku-5-5","content":[]}}));
    assert!(t.violation.as_ref().unwrap().1.contains("sub-agent"));
}

#[test]
fn other_tools_models_and_permission_prompts_are_refused() {
    let mut t = tracker();
    t.on(
        &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"t1","name":"mcp__elsebot__memory_search","input":{}}]}}),
    );
    assert!(t.violation.is_none());
    t.on(
        &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"t2","name":"WebFetch","input":{}}]}}),
    );
    assert!(t.violation.as_ref().unwrap().1.contains("WebFetch"));
    let mut t = tracker();
    t.on(&json!({"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-5-5"}}}));
    assert_eq!(t.violation.as_ref().unwrap().0, "profile_mismatch");
    let mut t = tracker();
    let steps = t.on(&json!({"type":"control_request","request_id":"r9","request":{"subtype":"can_use_tool","tool_name":"Write"}}));
    assert_eq!(steps, vec![Step::Control("r9".into())]);
    assert!(t.violation.as_ref().unwrap().1.contains("Write"));
}

#[test]
fn a_pending_delegation_or_secret_request_is_noticed_from_its_tool_result() {
    let mut t = tracker();
    t.on(
        &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"d1","name":"mcp__elsebot__delegate","input":{}}]}}),
    );
    let steps = t.on(&json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"d1","content":[{"type":"text","text":"{\"status\":\"pending\"}"}]}]}}));
    assert_eq!(steps, vec![Step::Parked]);
    // A pending secret request parks the attempt the same way; other
    // tools' results never do.
    let mut t = tracker();
    t.on(
        &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"s1","name":"mcp__elsebot__request_secret","input":{}},
            {"type":"tool_use","id":"m1","name":"mcp__elsebot__memory_search","input":{}}]}}),
    );
    let steps = t.on(&json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"m1","content":[{"type":"text","text":"pending"}]},
            {"type":"tool_result","tool_use_id":"s1","content":[{"type":"text","text":"{\"status\":\"pending\"}"}]}]}}));
    assert_eq!(steps, vec![Step::Parked]);
}

#[test]
fn a_no_tool_checkpoint_requires_an_empty_inventory() {
    let mut t = Tracker::new(Vec::new(), "claude-haiku-5-5", "u");
    t.on(&init(Vec::new(), json!([])));
    assert!(t.violation.is_none());
    let mut t = Tracker::new(Vec::new(), "claude-haiku-5-5", "u");
    t.on(&init(claude_home::tool_names(), connected()));
    assert!(t.violation.is_some());
}

#[test]
fn profiles_need_explicit_supported_values() {
    let claude = Claude::detect(None, None, None, String::new());
    let ok = Profile {
        model: "claude-opus-5-5".into(),
        reasoning_effort: "high".into(),
        service_tier: "default".into(),
    };
    assert!(claude.validate_profile(&ok).is_ok());
    for bad in [
        Profile {
            model: "opus".into(),
            ..ok.clone()
        },
        Profile {
            model: "claude-opus-4".into(),
            ..ok.clone()
        },
        Profile {
            reasoning_effort: "ultra".into(),
            ..ok.clone()
        },
        Profile {
            service_tier: "priority".into(),
            ..ok.clone()
        },
    ] {
        assert!(claude.validate_profile(&bad).is_err(), "{bad:?}");
    }
    assert!(matches!(
        claude.availability(),
        Availability::Unavailable(_)
    ));
}

#[test]
fn arguments_offer_only_host_and_elsebot_tools_and_resume_explicitly() {
    let claude = Claude::detect(None, None, None, String::new())
        .with_tools("http://127.0.0.1:1/tools/v1/mcp".into());
    let profile = Profile {
        model: "claude-sonnet-5-5".into(),
        reasoning_effort: "medium".into(),
        service_tier: "default".into(),
    };
    let args: Vec<String> = claude
        .args(&profile, &Session::Resume("abc".into()), "SYSTEM", true)
        .into_iter()
        .map(|a| a.into_string().unwrap())
        .collect();
    let after = |flag: &str| args[args.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert_eq!(after("--tools"), claude_home::HOST_TOOLS.join(","));
    assert_eq!(after("--model"), "claude-sonnet-5-5");
    assert_eq!(after("--effort"), "medium");
    assert_eq!(after("--resume"), "abc");
    assert_eq!(after("--setting-sources"), "");
    assert_eq!(after("--system-prompt"), "SYSTEM");
    assert!(after("--disallowedTools").contains("Agent"));
    let allowed: Vec<String> = after("--allowedTools")
        .split(',')
        .map(str::to_owned)
        .collect();
    assert_eq!(allowed, claude.turn_tools());
    assert!(allowed.iter().all(|t| {
        claude_home::HOST_TOOLS.contains(&t.as_str()) || t.starts_with("mcp__elsebot__")
    }));
    assert!(allowed.iter().any(|t| t == "Bash"));
    assert!(args.contains(&"--strict-mcp-config".to_owned()));
    assert!(
        !args
            .iter()
            .any(|a| a == "--continue" || a == "-c" || a == "--bare")
    );
}

fn detect_with(dir: &std::path::Path, version: &str, status: &str) -> Availability {
    use std::os::unix::fs::PermissionsExt;
    let program = dir.join("claude");
    std::fs::write(
        &program,
        format!("#!/bin/sh\ncase \"$1\" in\n--version) {version} ;;\nauth) {status} ;;\nesac\n"),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = claude_home::HostConfig {
        dir: dir.to_owned(),
        explicit: true,
    };
    Claude::detect(
        Some(Host::new(dir, dir)),
        Some(program),
        Some(config),
        String::new(),
    )
    .availability()
}

#[test]
fn any_running_logged_in_version_is_available() {
    let dir = tempfile::tempdir().unwrap();
    let version = "echo '2.1.999 (Claude Code)'";
    let logged_in = r#"echo '{"loggedIn":true}'"#;
    let availability = detect_with(dir.path(), version, logged_in);
    assert!(
        matches!(availability, Availability::Available),
        "{availability:?}"
    );
    for (version, status) in [
        ("exit 1", logged_in),
        ("true", logged_in),
        (version, r#"echo '{"loggedIn":false}'"#),
        (version, "echo 'not json'"),
        (version, "exit 1"),
    ] {
        assert!(
            matches!(
                detect_with(dir.path(), version, status),
                Availability::Unavailable(_)
            ),
            "{version} {status}"
        );
    }
}

fn info(success: bool, num_turns: Option<i64>) -> ResultInfo {
    ResultInfo {
        success,
        message: "m".into(),
        num_turns,
    }
}

#[test]
fn only_a_provable_non_start_is_a_refusal() {
    // No session could be opened: nothing ran.
    let t = tracker();
    assert!(matches!(
        decide(&t, &info(false, Some(0)), false, false),
        TurnOutcome::Failed { ref code, .. } if code == "provider_refused"
    ));
    // A success, model output, or an unknown turn count without the
    // echo may have consumed the message.
    assert!(matches!(
        decide(&t, &info(true, Some(1)), false, false),
        TurnOutcome::Uncertain { .. }
    ));
    assert!(matches!(
        decide(&t, &info(false, None), false, false),
        TurnOutcome::Uncertain { .. }
    ));
    let mut active = tracker();
    active.on(&json!({"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-haiku-5-5"}}}));
    assert!(matches!(
        decide(&active, &info(false, Some(0)), false, false),
        TurnOutcome::Uncertain { .. }
    ));
}

#[test]
fn completion_requires_a_verified_init() {
    let mut t = tracker();
    t.on(&json!({"type":"user","uuid":"u-1","isReplay":true}));
    assert!(matches!(
        decide(&t, &info(true, Some(1)), false, false),
        TurnOutcome::Failed { ref code, .. } if code == "policy_violation"
    ));
    t.on(&init(claude_home::tool_names(), connected()));
    assert_eq!(
        decide(&t, &info(true, Some(1)), false, false),
        TurnOutcome::Completed
    );
}

#[tokio::test]
async fn an_abandoned_read_keeps_its_partial_line() {
    use tokio::io::AsyncWriteExt;
    let (mut tx, rx) = tokio::io::duplex(64);
    let mut events = Events::new(BufReader::new(rx));
    tx.write_all(br#"{"type":"result","#).await.unwrap();
    // A timer wins the select while half the line has been read.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.next())
            .await
            .is_err()
    );
    tx.write_all(b"\"subtype\":\"success\"}\n").await.unwrap();
    let event = events.next().await.unwrap().unwrap();
    assert_eq!(event["subtype"], "success");
    drop(tx);
    assert_eq!(events.next().await.unwrap(), None);
}

fn fake_claude(dir: &std::path::Path) -> (Claude, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let received = dir.join("received.jsonl");
    let program = dir.join("claude");
    std::fs::write(
        &program,
        format!(
            r#"#!/usr/bin/env python3
import json, sys
a = sys.argv[1:]
if a[:1] == ["--version"]: print("2.1.999 (Claude Code)"); sys.exit()
if a[:1] == ["auth"]: print('{{"loggedIn":true}}'); sys.exit()
out = lambda e: print(json.dumps(e), flush=True)
out({{"type": "system", "subtype": "init", "model": a[a.index("--model") + 1],
     "tools": a[a.index("--tools") + 1].split(","),
     "mcp_servers": [{{"name": "elsebot", "status": "connected"}}]}})
msg = json.loads(sys.stdin.readline())
with open({received:?}, "a") as f: f.write(json.dumps(msg["message"]["content"]) + "\n")
out({{"type": "user", "uuid": msg["uuid"], "isReplay": True}})
out({{"type": "result", "subtype": "success", "is_error": False, "result": "ok", "num_turns": 1}})
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = claude_home::HostConfig {
        dir: dir.join("config"),
        explicit: true,
    };
    let claude = Claude::detect(
        Some(Host::new(dir, &dir.join("bots"))),
        Some(program),
        Some(config),
        String::new(),
    );
    assert!(matches!(claude.availability(), Availability::Available));
    (claude, received)
}

fn specialist_turn(session: Option<SessionState>) -> TurnInput {
    TurnInput {
        bot_id: Uuid::from_u128(7),
        conversation_id: Uuid::from_u128(8),
        turn_id: Uuid::new_v4(),
        turn_no: 3,
        bot_name: "GitHub".into(),
        text: "Clean up the stale branches.".into(),
        profile: Some(Profile {
            model: MODELS[0].into(),
            reasoning_effort: "medium".into(),
            service_tier: "default".into(),
        }),
        history: Vec::new(),
        session,
        tool_grant: None,
        memory: Some(MemoryContext {
            revision: 7,
            text: "[else.bot memory] Owner correction: rebase branches, never merge them.\n\n"
                .into(),
        }),
        attempt: 1,
        delegation: Default::default(),
        secrets: Vec::new(),
        handoff: None,
        jobs: Vec::new(),
    }
}

#[tokio::test]
async fn a_warm_session_receives_memory_whenever_its_revision_changed() {
    let dir = tempfile::tempdir().unwrap();
    let (claude, received) = fake_claude(dir.path());
    let base = specialist_turn(None);
    let profile = base.profile.clone().unwrap();
    let fingerprint = claude.fingerprint(&profile, &base);
    let native = Uuid::new_v4().to_string();
    let dirs = claude
        .host
        .as_ref()
        .unwrap()
        .bot_dirs(base.bot_id, ID)
        .unwrap();
    let project: String = std::fs::canonicalize(&dirs.project)
        .unwrap()
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let transcripts = dir.path().join("config/projects").join(project);
    std::fs::create_dir_all(&transcripts).unwrap();
    std::fs::write(transcripts.join(format!("{native}.jsonl")), "").unwrap();
    let warm = |memory_revision| SessionState {
        native_id: native.clone(),
        fingerprint: fingerprint.clone(),
        considered_through_turn: 2,
        memory_revision,
    };
    let memory = "Owner correction: rebase branches, never merge them.";

    for (session, refreshed) in [
        (None, true),
        (Some(warm(Some(6))), true),
        (Some(warm(Some(7))), false),
    ] {
        let resumed = session.as_ref().map(|s| s.native_id.clone());
        let sink = super::super::recording::Recorded::default();
        let outcome = claude
            .run(specialist_turn(session), &sink, CancellationToken::new())
            .await;
        assert_eq!(outcome, TurnOutcome::Completed);
        let text = super::super::recording::received(&received).pop().unwrap();
        assert_eq!(text.contains(memory), refreshed, "{resumed:?}: {text}");
        assert!(text.ends_with("Clean up the stale branches."));
        let state = sink.last_session();
        assert_eq!(state.memory_revision, Some(7));
        if let Some(native) = resumed {
            assert_eq!(state.native_id, native, "the same native session");
        }
    }
}
