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
            {"type":"tool_result","tool_use_id":"d1","content":[{"type":"text","text":"{\"status\":\"pending\",\"parked\":true}"}]}]}}));
    assert_eq!(steps, vec![Step::Parked]);
    let mut t = tracker();
    t.on(
        &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"s1","name":"mcp__elsebot__request_secret","input":{}},
            {"type":"tool_use","id":"m1","name":"mcp__elsebot__memory_search","input":{}}]}}),
    );
    let steps = t.on(&json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"m1","content":[{"type":"text","text":"pending"}]},
            {"type":"tool_result","tool_use_id":"s1","content":[{"type":"text","text":"{\"status\":\"pending\",\"parked\":true}"}]}]}}));
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
        .args(
            &profile,
            &Session::Resume("abc".into()),
            "SYSTEM",
            Tools::HostAndElsebot,
        )
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
    assert_eq!(allowed, claude.offered_tools(Tools::HostAndElsebot));
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
    let t = tracker();
    assert!(matches!(
        decide(&t, &info(false, Some(0)), false, false),
        TurnOutcome::Failed { ref code, .. } if code == "provider_refused"
    ));
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
        parts: Vec::new(),
        files: Vec::new(),
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
        questions: Vec::new(),
        outgoing_drafts: Vec::new(),
        social_setups: Vec::new(),
        handoff: None,
        jobs: Vec::new(),
        routine: None,
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

#[tokio::test]
async fn files_reach_claude_in_order_with_images_inline() {
    use crate::app::messages::{Attachment, MessagePart};
    use crate::app::providers::InputFile;
    let dir = tempfile::tempdir().unwrap();
    let (claude, received) = fake_claude(dir.path());
    let file = |name: &str, media_type: &str, data: Vec<u8>| InputFile {
        attachment: Attachment {
            id: Uuid::new_v4(),
            name: name.into(),
            media_type: media_type.into(),
            size: data.len() as u64,
        },
        data: data.into(),
    };
    let image = file("shot.png", "image/png", b"\x89PNG fake".to_vec());
    let report = file("report.pdf", "application/pdf", b"%PDF-1.7".to_vec());
    let huge = file(
        "huge.png",
        "image/png",
        vec![0; super::attachments::MAX_INLINE_IMAGE_BYTES + 1],
    );
    let text = |t: &str| MessagePart::Text { text: t.into() };
    let as_part = |f: &InputFile| MessagePart::File {
        file: f.attachment.clone(),
    };
    let parts = vec![
        as_part(&image),
        text(" see "),
        as_part(&report),
        text(" and "),
        as_part(&huge),
        text("  "),
    ];
    let mut input = specialist_turn(None);
    input.text = crate::app::messages::flatten(&parts);
    input.parts = parts;
    input.files = vec![image.clone(), report.clone(), huge.clone()];
    let bot_id = input.bot_id;
    let sink = super::super::recording::Recorded::default();
    let outcome = claude.run(input, &sink, CancellationToken::new()).await;
    assert_eq!(outcome, TurnOutcome::Completed);

    let project = claude.host.as_ref().unwrap().project_path(bot_id);
    for f in [&image, &report, &huge] {
        let staged = project.join(super::attachments::relative_path(&f.attachment));
        assert_eq!(
            std::fs::read(staged).unwrap(),
            *f.data,
            "{}",
            f.attachment.name
        );
    }
    let line = std::fs::read_to_string(&received).unwrap();
    let content: Value = serde_json::from_str(line.lines().last().unwrap()).unwrap();
    let blocks = content.as_array().expect("content blocks");
    assert_eq!(blocks.len(), 3, "{content}");
    assert_eq!(blocks[0]["type"], "text");
    assert!(
        blocks[0]["text"]
            .as_str()
            .unwrap()
            .ends_with(&prompt::file_reference(&image.attachment))
    );
    assert_eq!(
        blocks[1],
        json!({ "type": "image", "source": {
            "type": "base64", "media_type": "image/png", "data": "iVBORyBmYWtl",
        } })
    );
    assert_eq!(
        blocks[2]["text"],
        format!(
            " see {} and {}  ",
            prompt::file_reference(&report.attachment),
            prompt::file_reference(&huge.attachment)
        )
    );
}

fn fake_inventory_claude(dir: &std::path::Path, builtins: Option<&[&str]>) -> Claude {
    use std::os::unix::fs::PermissionsExt;
    let program = dir.join("claude");
    let launched = dir.join("launched.json");
    let builtins =
        builtins.map_or_else(|| "None".to_owned(), |b| serde_json::to_string(b).unwrap());
    let server_tools = serde_json::to_string(&claude_home::tool_names()).unwrap();
    std::fs::write(
        &program,
        format!(
            r#"#!/usr/bin/env python3
import json, sys
a = sys.argv[1:]
if a[:1] == ["--version"]: print("2.1.999 (Claude Code)"); sys.exit()
if a[:1] == ["auth"]: print('{{"loggedIn":true}}'); sys.exit()
out = lambda e: print(json.dumps(e), flush=True)
settings = open(a[a.index("--settings") + 1]).read()
with open({launched:?}, "w") as f: json.dump({{"args": a, "settings": json.loads(settings)}}, f)
builtins = {builtins}
if builtins is None: builtins = [t for t in a[a.index("--tools") + 1].split(",") if t]
mcp = "--mcp-config" in a
tools = builtins + ({server_tools} if mcp else [])
out({{"type": "system", "subtype": "init", "model": a[a.index("--model") + 1],
     "tools": tools,
     "mcp_servers": [{{"name": "elsebot", "status": "connected"}}] if mcp else []}})
msg = json.loads(sys.stdin.readline())
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
    Claude::detect(
        Some(Host::new(dir, &dir.join("bots"))),
        Some(program),
        Some(config),
        String::new(),
    )
    .with_tools("http://127.0.0.1:1/tools/v1/mcp".into())
}

fn turn_from(origin: crate::app::providers::Origin) -> TurnInput {
    let mut turn = specialist_turn(None);
    turn.tool_grant = Some("tg_test".into());
    turn.delegation.origin = origin;
    turn
}

#[tokio::test]
async fn a_connector_event_launches_with_only_elsebot_tools() {
    use crate::app::providers::Origin;
    for (origin, host_tools) in [
        (Origin::ConnectorEvent, false),
        (Origin::OwnerRequest, true),
        (Origin::RoutineRun, true),
        (Origin::Other, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let claude = fake_inventory_claude(dir.path(), None);
        let sink = super::super::recording::Recorded::default();
        let outcome = claude
            .run(turn_from(origin), &sink, CancellationToken::new())
            .await;
        assert_eq!(outcome, TurnOutcome::Completed, "{origin:?}");
        let launched: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("launched.json")).unwrap(),
        )
        .unwrap();
        let args: Vec<&str> = launched["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        let after = |flag: &str| args[args.iter().position(|a| *a == flag).unwrap() + 1];
        let allowed: Vec<&str> = after("--allowedTools").split(',').collect();
        let deny: Vec<&str> = launched["settings"]["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        assert!(args.contains(&"--mcp-config"), "{origin:?}");
        assert!(args.contains(&"--strict-mcp-config"), "{origin:?}");
        assert_eq!(after("--setting-sources"), "", "{origin:?}");
        if host_tools {
            assert_eq!(
                after("--tools"),
                claude_home::HOST_TOOLS.join(","),
                "{origin:?}"
            );
            assert!(allowed.contains(&"Bash"), "{origin:?}");
            assert!(!deny.contains(&"Bash"), "{origin:?}");
        } else {
            assert_eq!(after("--tools"), "", "{origin:?}");
            let mut expected = claude_home::tool_names();
            expected.sort();
            let mut allowed: Vec<String> = allowed.iter().map(|t| (*t).to_owned()).collect();
            allowed.sort();
            assert_eq!(allowed, expected);
            for tool in claude_home::HOST_TOOLS {
                assert!(deny.contains(tool), "{tool} denied in the settings");
            }
        }
        for tool in ["Agent", "WebFetch", "WebSearch"] {
            assert!(deny.contains(&tool), "{origin:?} {tool}");
        }
    }
}

#[tokio::test]
async fn a_connector_event_offered_host_tools_anyway_fails() {
    use crate::app::providers::Origin;
    for tool in ["Bash", "Read"] {
        let dir = tempfile::tempdir().unwrap();
        let claude = fake_inventory_claude(dir.path(), Some(&[tool]));
        let sink = super::super::recording::Recorded::default();
        let outcome = claude
            .run(
                turn_from(Origin::ConnectorEvent),
                &sink,
                CancellationToken::new(),
            )
            .await;
        assert!(
            matches!(&outcome, TurnOutcome::Failed { code, message }
                if code == "policy_violation" && message.contains("unexpected tools")),
            "{tool}: {outcome:?}"
        );
    }
}

#[test]
fn a_connector_event_never_resumes_a_session_with_host_tools() {
    use crate::app::providers::Origin;
    let claude = Claude::detect(None, None, None, String::new())
        .with_tools("http://127.0.0.1:1/tools/v1/mcp".into());
    let owner = turn_from(Origin::OwnerRequest);
    let event = turn_from(Origin::ConnectorEvent);
    let profile = owner.profile.clone().unwrap();
    assert_ne!(
        claude.fingerprint(&profile, &owner),
        claude.fingerprint(&profile, &event)
    );
}
