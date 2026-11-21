use super::*;
use crate::app::providers::{Availability, Provider, TurnInput, TurnOutcome};
use crate::app::providers::{FailureCode, MemoryContext, SessionState};
use crate::providers::cli::attempt::{Event, Protocol, StopCause};
use crate::providers::models::Models;
use protocol::ResultInfo;

type Emitted = Vec<Event<ResultInfo>>;

impl Claude {
    fn offline(account: &str) -> Self {
        let ready = Ready {
            host: Host::new(std::path::Path::new("/data"), std::path::Path::new("/bots")),
            program: PathBuf::new().into(),
            version: "2.1.999 (Claude Code)".into(),
            login: Login {
                config: claude_home::HostConfig {
                    dir: PathBuf::new(),
                    explicit: false,
                },
                account: account.into(),
            },
        };
        Self::new(ClaudeCode, Ok(ready)).with_tools("http://127.0.0.1:1/tools/v1/mcp".into())
    }
}

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

fn accepted() -> Event<ResultInfo> {
    Event::Accepted {
        model: "claude-haiku-5-5".into(),
        native_turn: "u-1".into(),
    }
}

#[test]
fn a_turn_is_accepted_by_its_echo_and_streams_text() {
    let mut t = tracker();
    t.on(&init(claude_home::tool_names(), connected())).unwrap();
    assert_eq!(
        t.on(&json!({"type":"user","uuid":"other","isReplay":true})),
        Ok(Emitted::new())
    );
    assert_eq!(
        t.on(&json!({"type":"user","uuid":"u-1","isReplay":true,"message":{"role":"user","content":"hello"}})),
        Ok(vec![accepted()])
    );
    assert!(
        t.on(&json!({"type":"user","message":{"content":7}}))
            .is_err()
    );
    t.on(&json!({"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-haiku-5-5"}}})).unwrap();
    t.on(&json!({"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"text"}}})).unwrap();
    let events = t.on(&json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}}));
    assert_eq!(events, Ok(vec![Event::Text("Hi".into())]));
    t.on(&json!({"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"text"}}})).unwrap();
    let events = t.on(&json!({"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"again"}}}));
    assert_eq!(
        events,
        Ok(vec![Event::EndMessage, Event::Text("again".into())])
    );
    let events =
        t.on(&json!({"type":"result","subtype":"success","is_error":false,"result":"Hi\n\nagain"}));
    assert_eq!(
        events,
        Ok(vec![Event::Finished(ResultInfo {
            success: true,
            message: "Hi\n\nagain".into(),
            num_turns: None,
            api_error: false,
        })])
    );
}

#[test]
fn the_result_reports_token_usage_with_cache_reads_and_writes_in_input() {
    let events = tracker().on(
        &json!({"type":"result","subtype":"success","is_error":false,
        "result":"ok","usage":{"input_tokens":5,"cache_read_input_tokens":1000,
        "cache_creation_input_tokens":200,"output_tokens":40}}),
    );
    assert_eq!(
        events.unwrap()[0],
        Event::Usage(crate::app::providers::Usage {
            input: 1205,
            cached_input: 1000,
            cache_write: 200,
            output: 40,
        })
    );
}

#[test]
fn a_rate_limit_event_reports_the_five_hour_and_weekly_windows() {
    use crate::app::providers::LimitWindow;
    let events = tracker().on(&json!({"type":"rate_limit_event","rate_limit_info":{
        "status":"allowed","resetsAt":1794837600,"rateLimitType":"five_hour",
        "unifiedWindows":{
            "five_hour":{"utilization":0.09,"resetsAt":1794837600,"status":"allowed"},
            "seven_day":{"utilization":0.16,"resetsAt":1795233600,"status":"allowed"}}}}));
    assert_eq!(
        events,
        Ok(vec![Event::Limits(vec![
            LimitWindow {
                minutes: 300,
                used_basis_points: 900,
                resets_at: chrono::DateTime::from_timestamp(1794837600, 0).unwrap(),
            },
            LimitWindow {
                minutes: 10080,
                used_basis_points: 1600,
                resets_at: chrono::DateTime::from_timestamp(1795233600, 0).unwrap(),
            },
        ])])
    );
}

#[test]
fn a_rate_limit_event_without_windows_reports_nothing() {
    let events = tracker().on(&json!({"type":"rate_limit_event",
        "rate_limit_info":{"status":"allowed","rateLimitType":"five_hour"}}));
    assert_eq!(events, Ok(Vec::new()));
}

#[test]
fn a_compact_boundary_reports_compaction() {
    let events = tracker().on(&json!({"type":"system","subtype":"compact_boundary",
        "compact_metadata":{"trigger":"auto","pre_tokens":150000}}));
    assert_eq!(events, Ok(vec![Event::Compacted]));
}

/// Claude Code switches models on its own when the chosen one is busy,
/// unavailable to the account, or refuses, and announces each switch.
#[test]
fn an_announced_model_fallback_keeps_the_turn_and_reports_the_model() {
    let mut t = tracker();
    t.on(&init(claude_home::tool_names(), connected())).unwrap();
    t.on(&json!({"type":"user","uuid":"u-1","isReplay":true}))
        .unwrap();
    let events = t.on(&json!({"type":"system","subtype":"model_fallback",
        "trigger":"model_not_found","original_model":"claude-haiku-5-5",
        "fallback_model":"claude-haiku-4-8",
        "content":"Switched to Haiku 4.8 because claude-haiku-5-5 is not available"}));
    assert_eq!(
        events,
        Ok(vec![Event::Rerouted {
            model: "claude-haiku-4-8".into(),
            native_turn: "u-1".into(),
        }])
    );
    t.on(&json!({"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-haiku-4-8"}}}))
        .unwrap();
    t.on(&json!({"type":"assistant","message":{"model":"claude-haiku-4-8","content":[]}}))
        .unwrap();
    t.on(&json!({"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-haiku-5-5"}}}))
        .unwrap();
    let refused = t
        .on(
            &json!({"type":"system","subtype":"model_refusal_fallback","direction":"retry",
            "original_model":"claude-haiku-4-8","fallback_model":"claude-sonnet-5-5"}),
        )
        .unwrap();
    assert_eq!(
        refused,
        vec![Event::Rerouted {
            model: "claude-sonnet-5-5".into(),
            native_turn: "u-1".into(),
        }]
    );
    t.on(&json!({"type":"assistant","message":{"model":"claude-sonnet-5-5","content":[]}}))
        .unwrap();
}

#[test]
fn a_model_fallback_before_the_echo_is_the_accepted_model() {
    let mut t = tracker();
    t.on(
        &json!({"type":"system","subtype":"model_fallback","trigger":"overloaded",
        "original_model":"claude-haiku-5-5","fallback_model":"claude-haiku-4-8"}),
    )
    .unwrap();
    assert_eq!(
        t.on(&json!({"type":"user","uuid":"u-1","isReplay":true})),
        Ok(vec![Event::Accepted {
            model: "claude-haiku-4-8".into(),
            native_turn: "u-1".into(),
        }])
    );
}

#[test]
fn an_unannounced_model_fails_the_turn_with_what_the_owner_can_do() {
    let violation = tracker()
        .on(&json!({"type":"assistant","message":{"model":"claude-opus-4-8","content":[]}}))
        .unwrap_err();
    assert_eq!(violation.code, FailureCode::ProfileMismatch);
    assert_eq!(
        violation.message,
        "Claude answered with claude-opus-4-8 instead of the chosen claude-haiku-5-5, \
         so else.bot stopped the turn. Send the message again, or choose another model \
         for this bot."
    );
}

#[test]
fn an_agent_tool_extra_server_or_malformed_inventory_is_refused() {
    let mut tools = claude_home::tool_names();
    tools.push("Agent".into());
    assert_eq!(
        tracker().on(&init(tools, connected())).unwrap_err().code,
        FailureCode::PolicyViolation
    );
    for servers in [
        json!([{ "name": "elsebot", "status": "connected" }, { "name": "other", "status": "connected" }]),
        json!([{ "name": "elsebot", "status": "failed" }]),
        json!([{ "name": "elsebot" }]),
        json!("elsebot"),
    ] {
        assert!(
            tracker()
                .on(&init(claude_home::tool_names(), servers.clone()))
                .is_err(),
            "{servers}"
        );
    }
    let mut event = init(claude_home::tool_names(), connected());
    event["tools"]
        .as_array_mut()
        .unwrap()
        .push(json!({"name": "Bash"}));
    assert!(
        tracker().on(&event).is_err(),
        "a non-string tool cannot hide"
    );
    let mut event = init(claude_home::tool_names(), connected());
    event["tools"] = Value::Null;
    assert!(tracker().on(&event).is_err());
}

#[test]
fn the_built_in_agent_list_alone_is_not_a_violation_but_its_activity_is() {
    let mut t = tracker();
    assert!(t.on(&init(claude_home::tool_names(), connected())).is_ok());
    let violation = t
        .on(&json!({"type":"system","subtype":"task_started","task_id":"x","task_type":"mcp_task","description":"elsebot/computer_run"}))
        .unwrap_err();
    assert_eq!(
        violation.message,
        "Claude reported native agent activity (task_started, mcp_task, elsebot/computer_run)"
    );
    let violation = tracker()
        .on(&json!({"type":"assistant","parent_tool_use_id":"toolu_1","message":{"model":"claude-haiku-5-5","content":[]}}))
        .unwrap_err();
    assert!(violation.message.contains("sub-agent"));
}

fn heartbeat(parent: &str, tool: &str) -> Value {
    json!({"type":"tool_progress","tool_use_id":format!("{parent}-heartbeat-0"),
        "tool_name":tool,"parent_tool_use_id":parent,"elapsed_time_seconds":30,
        "heartbeat":true,"session_id":"s","uuid":"h"})
}

#[test]
fn a_heartbeat_for_a_slow_main_thread_tool_call_is_not_a_sub_agent() {
    let mut t = tracker();
    t.on(
        &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"toolu_1","name":"mcp__elsebot__mcp_request","input":{}}]}}),
    )
    .unwrap();
    assert_eq!(
        t.on(&heartbeat("toolu_1", "mcp__elsebot__mcp_request")),
        Ok(Emitted::new())
    );
    for (parent, tool) in [
        ("toolu_unknown", "mcp__elsebot__mcp_request"),
        ("toolu_1", "mcp__elsebot__memory_search"),
    ] {
        let violation = t.on(&heartbeat(parent, tool)).unwrap_err();
        assert!(violation.message.contains("sub-agent"), "{parent} {tool}");
    }
    let violation = t
        .on(&json!({"type":"assistant","parent_tool_use_id":"toolu_1",
            "message":{"model":"claude-haiku-5-5","content":[]}}))
        .unwrap_err();
    assert!(violation.message.contains("sub-agent"));
}

#[test]
fn other_tools_models_and_permission_prompts_are_refused() {
    let mut t = tracker();
    assert!(
        t.on(
            &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"t1","name":"mcp__elsebot__memory_search","input":{}}]}})
        )
        .is_ok()
    );
    t.on(
        &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"t2","name":"WebFetch","input":{}}]}}),
    )
    .unwrap();
    let violation = t
        .on(&json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"t2","content":"fetched"}]}}))
        .unwrap_err();
    assert!(violation.message.contains("WebFetch"));
    let violation = tracker()
        .on(&json!({"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-opus-5-5"}}}))
        .unwrap_err();
    assert_eq!(violation.code, FailureCode::ProfileMismatch);
    let violation = tracker()
        .on(&json!({"type":"control_request","request_id":"r9","request":{"subtype":"can_use_tool","tool_name":"Write"}}))
        .unwrap_err();
    assert!(violation.message.contains("Write"));
    assert!(
        tracker()
            .on(&json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":7}]}}))
            .is_err()
    );
}

#[test]
fn a_call_to_an_unoffered_name_that_the_cli_refuses_keeps_the_turn() {
    let mut t = tracker();
    t.on(&init(claude_home::tool_names(), connected())).unwrap();
    t.on(
        &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"b1","name":"delegate","input":{"task":"x"}}]}}),
    )
    .unwrap();
    assert!(
        t.on(&heartbeat("b1", "delegate")).is_err(),
        "an unoffered call has no heartbeat"
    );
    let mut t = tracker();
    t.on(
        &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"b1","name":"delegate","input":{"task":"x"}}]}}),
    )
    .unwrap();
    let refused = t.on(&json!({"type":"user","message":{"content":[
        {"type":"tool_result","tool_use_id":"b1","is_error":true,
         "content":"<tool_use_error>Error: No such tool available: delegate</tool_use_error>"}]}}));
    assert_eq!(refused, Ok(Emitted::new()));
    t.on(
        &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"d1","name":"mcp__elsebot__delegate","input":{}}]}}),
    )
    .unwrap();
    let parked = t.on(&json!({"type":"user","message":{"content":[
        {"type":"tool_result","tool_use_id":"d1","content":[{"type":"text","text":"{\"status\":\"pending\",\"parked\":true}"}]}]}}));
    assert_eq!(parked, Ok(vec![Event::Parked]));
}

#[test]
fn a_pending_delegation_or_secret_request_is_noticed_from_its_tool_result() {
    let mut t = tracker();
    t.on(
        &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"d1","name":"mcp__elsebot__delegate","input":{}}]}}),
    )
    .unwrap();
    let events = t.on(&json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"d1","content":[{"type":"text","text":"{\"status\":\"pending\",\"parked\":true}"}]}]}}));
    assert_eq!(events, Ok(vec![Event::Parked]));
    let mut t = tracker();
    t.on(
        &json!({"type":"assistant","message":{"model":"claude-haiku-5-5","content":[
            {"type":"tool_use","id":"s1","name":"mcp__elsebot__request_secret","input":{}},
            {"type":"tool_use","id":"m1","name":"mcp__elsebot__memory_search","input":{}}]}}),
    )
    .unwrap();
    let events = t.on(&json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"m1","content":[{"type":"text","text":"pending"}]},
            {"type":"tool_result","tool_use_id":"s1","content":[{"type":"text","text":"{\"status\":\"pending\",\"parked\":true}"}]}]}}));
    assert_eq!(events, Ok(vec![Event::Parked]));
}

#[test]
fn a_no_tool_checkpoint_requires_an_empty_inventory() {
    let mut t = Tracker::new(Vec::new(), "claude-haiku-5-5", "u");
    assert!(t.on(&init(Vec::new(), json!([]))).is_ok());
    let mut t = Tracker::new(Vec::new(), "claude-haiku-5-5", "u");
    assert!(t.on(&init(claude_home::tool_names(), connected())).is_err());
}

#[tokio::test]
async fn profiles_need_explicit_supported_values() {
    let claude = Claude::detect(None, None, None, String::new()).await;
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

#[tokio::test]
async fn arguments_offer_only_host_and_elsebot_tools_and_resume_explicitly() {
    let profile = Profile {
        model: "claude-sonnet-5-5".into(),
        reasoning_effort: "medium".into(),
        service_tier: "default".into(),
    };
    let args: Vec<String> = args(&profile, true, "abc", "SYSTEM", ToolSet::HostAndElsebot)
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
    assert_eq!(allowed, offered_tools(ToolSet::HostAndElsebot));
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

async fn detect_with(dir: &std::path::Path, version: &str, status: &str) -> Availability {
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
    .await
    .availability()
}

#[tokio::test]
async fn any_running_logged_in_version_is_available() {
    let dir = tempfile::tempdir().unwrap();
    let version = "echo '2.1.999 (Claude Code)'";
    let logged_in = r#"echo '{"loggedIn":true}'"#;
    let availability = detect_with(dir.path(), version, logged_in).await;
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
                detect_with(dir.path(), version, status).await,
                Availability::Unavailable(_)
            ),
            "{version} {status}"
        );
    }
}

#[tokio::test]
async fn only_a_login_check_that_answered_reports_a_signed_out_cli() {
    let dir = tempfile::tempdir().unwrap();
    let version = "echo '2.1.999 (Claude Code)'";
    let not_logged_in = Availability::Unavailable(
        "Claude Code is not logged in on the host (run `claude` and log in).".into(),
    );
    assert_eq!(
        detect_with(dir.path(), version, "exit 1").await,
        not_logged_in
    );
    let flood = "head -c 2000000 /dev/zero | tr '\\0' a";
    assert_eq!(
        detect_with(dir.path(), version, flood).await,
        Availability::Unavailable(
            "Claude Code could not check its login (`claude auth status`: its output exceeded \
             1048576 bytes)."
                .into()
        )
    );
}

fn info(success: bool, num_turns: Option<i64>) -> ResultInfo {
    ResultInfo {
        success,
        message: "m".into(),
        num_turns,
        api_error: false,
    }
}

#[test]
fn only_a_provable_non_start_is_a_refusal() {
    let t = tracker();
    assert!(matches!(
        t.outcome(info(false, Some(0)), None, false),
        TurnOutcome::Failed {
            code: FailureCode::ProviderRefused,
            ..
        }
    ));
    assert!(matches!(
        t.outcome(info(false, Some(0)), Some(StopCause::Owner), false),
        TurnOutcome::Uncertain { .. }
    ));
    assert!(matches!(
        t.outcome(info(true, Some(1)), None, false),
        TurnOutcome::Uncertain { .. }
    ));
    assert!(matches!(
        t.outcome(info(false, None), None, false),
        TurnOutcome::Uncertain { .. }
    ));
    let mut active = tracker();
    active.on(&json!({"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-haiku-5-5"}}})).unwrap();
    assert!(matches!(
        active.outcome(info(false, Some(0)), None, false),
        TurnOutcome::Uncertain { .. }
    ));
}

#[test]
fn completion_requires_a_verified_init_and_a_stop_explains_a_failure() {
    let mut t = tracker();
    t.on(&json!({"type":"user","uuid":"u-1","isReplay":true}))
        .unwrap();
    assert!(matches!(
        t.outcome(info(true, Some(1)), None, true),
        TurnOutcome::Failed {
            code: FailureCode::PolicyViolation,
            ..
        }
    ));
    t.on(&init(claude_home::tool_names(), connected())).unwrap();
    assert_eq!(
        t.outcome(info(true, Some(1)), None, true),
        TurnOutcome::Completed
    );
    assert_eq!(
        t.outcome(info(false, Some(1)), Some(StopCause::Owner), true),
        TurnOutcome::Interrupted
    );
    assert!(matches!(
        t.outcome(info(false, Some(1)), None, true),
        TurnOutcome::Failed {
            code: FailureCode::ProviderFailed,
            ..
        }
    ));
}

#[test]
fn a_steered_message_is_its_own_line_and_its_echo_names_its_turn() {
    let mut t = tracker();
    let steer = crate::app::providers::Steer {
        turn_id: uuid::Uuid::new_v4(),
        text: "If they agreed, send the deal.".into(),
        parts: Vec::new(),
        files: Vec::new(),
    };
    let sent = t.steer_input(&steer).unwrap();
    let key = steer.turn_id.to_string();
    assert_eq!(
        (&sent["type"], &sent["uuid"]),
        (&json!("user"), &json!(key))
    );
    assert!(
        !t.continues(&info(true, Some(1))),
        "nothing owed before it is sent"
    );
    t.steer_sent(&steer);
    assert!(
        t.continues(&info(true, Some(1))),
        "a result before the echo"
    );
    assert!(
        !t.continues(&info(false, Some(1))),
        "a failed result ends it"
    );
    let echo = json!({"type":"user","uuid":key,"isReplay":true});
    assert_eq!(t.on(&echo).unwrap(), vec![Event::Steered(steer.turn_id)]);
    assert!(!t.continues(&info(true, Some(1))));
    assert!(t.on(&echo).unwrap().is_empty(), "an echo is taken once");
}

#[test]
fn a_notice_is_awaited_like_a_steer_and_its_echo_reports_nothing() {
    let mut t = tracker();
    let sent = t.notice_input("[else.bot] 5 minutes are left.").unwrap();
    assert_eq!(sent["type"], json!("user"));
    assert!(
        !t.continues(&info(true, Some(1))),
        "nothing owed before it is written"
    );
    t.notice_sent(&sent);
    assert!(
        t.continues(&info(true, Some(1))),
        "a result before the echo"
    );
    let echo = json!({"type":"user","uuid":sent["uuid"],"isReplay":true});
    assert!(t.on(&echo).unwrap().is_empty(), "not an owner message");
    assert!(!t.continues(&info(true, Some(1))));
}

#[test]
fn an_api_error_fails_the_turn_with_the_clis_message() {
    let mut t = tracker();
    t.on(&init(claude_home::tool_names(), connected())).unwrap();
    t.on(&json!({"type":"user","uuid":"u-1","isReplay":true}))
        .unwrap();
    let failure = "Failed to authenticate. API Error: 401 OAuth access token is invalid.";
    let report = json!({"type":"assistant","parent_tool_use_id":null,
        "error":"authentication_failed","is_api_error_message":true,"api_error_status":401,
        "message":{"model":"<synthetic>","role":"assistant","content":[{"type":"text","text":failure}]}});
    assert_eq!(t.on(&report), Ok(Emitted::new()));
    let Ok(events) = t.on(&json!({"type":"result","subtype":"success","is_error":true,
        "result":failure,"num_turns":1,"terminal_reason":"api_error"}))
    else {
        panic!("the result is accepted");
    };
    let Some(Event::Finished(result)) = events.last() else {
        panic!("the result finishes the turn");
    };
    let failed = TurnOutcome::Failed {
        code: FailureCode::ProviderFailed,
        message: failure.into(),
    };
    assert_eq!(t.outcome(result.clone(), None, true), failed);
    assert_eq!(
        t.outcome(result.clone(), Some(StopCause::Deadline), true),
        failed,
        "an API error as the deadline fires is not the interruption"
    );
    let model_output = json!({"type":"assistant","message":{"model":"<synthetic>","content":[]}});
    assert_eq!(
        tracker().on(&model_output).unwrap_err().code,
        FailureCode::ProfileMismatch
    );
}

async fn fake_claude(dir: &std::path::Path) -> (Claude, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let received = dir.join("received.jsonl");
    let program = dir.join("claude");
    std::fs::write(
        &program,
        format!(
            r#"#!/usr/bin/env python3
import json, os, sys
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
if os.environ.get("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS") != "1":
    out({{"type": "system", "subtype": "task_started", "task_id": "b1", "task_type": "mcp_task",
         "description": "elsebot/computer_run"}})
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
    )
    .await;
    assert!(matches!(claude.availability(), Availability::Available));
    (claude, received)
}

#[tokio::test]
async fn a_checkpoint_is_its_text_blocks_joined_by_a_blank_line() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("claude");
    std::fs::write(
        &program,
        r#"#!/usr/bin/env python3
import json, sys
a = sys.argv[1:]
if a[:1] == ["--version"]: print("2.1.999 (Claude Code)"); sys.exit()
if a[:1] == ["auth"]: print('{"loggedIn":true}'); sys.exit()
out = lambda e: print(json.dumps(e), flush=True)
model = a[a.index("--model") + 1]
out({"type": "system", "subtype": "init", "model": model, "tools": [], "mcp_servers": []})
msg = json.loads(sys.stdin.readline())
out({"type": "user", "uuid": msg["uuid"], "isReplay": True})
out({"type": "stream_event", "event": {"type": "message_start", "message": {"model": model}}})
for text in ["First.", "Second."]:
    out({"type": "stream_event", "event": {"type": "content_block_start", "content_block": {"type": "text"}}})
    out({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": text}}})
out({"type": "result", "subtype": "success", "is_error": False, "result": "Second.", "num_turns": 1})
"#,
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let claude = Claude::detect(
        Some(Host::new(dir.path(), &dir.path().join("bots"))),
        Some(program),
        Some(claude_home::HostConfig {
            dir: dir.path().join("config"),
            explicit: true,
        }),
        String::new(),
    )
    .await;
    let request = CheckpointRequest {
        bot_id: Uuid::new_v4(),
        profile: Some(Profile {
            model: "claude-opus-5-5".into(),
            reasoning_effort: "high".into(),
            service_tier: "default".into(),
        }),
        prompt: "Checkpoint the conversation.".into(),
        max_bytes: 1024,
    };
    let text = claude.checkpoint(request, &CancellationToken::new()).await;
    assert_eq!(text, Ok("First.\n\nSecond.".into()));
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
        steered: Vec::new(),
        files: Vec::new(),
        profile: Some(Profile {
            model: DEFINITION.fallback[0].into(),
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
            lines: Vec::new(),
        }),
        attempt: 1,
        delegation: Default::default(),
        secrets: Vec::new(),
        questions: Vec::new(),
        standing_permissions: Vec::new(),
        draft_tasks: Vec::new(),
        outgoing_drafts: Vec::new(),
        social_setups: Vec::new(),
        mcp_waits: Vec::new(),
        mcp_waits_in_progress: Vec::new(),
        computer_job_waits: Vec::new(),
        skill_installs: Vec::new(),
        computer_handoffs: Vec::new(),
        computer_control: None,
        ran_actions: Vec::new(),
        earlier_replies: Vec::new(),
        limit_continuation: None,
        skills: None,
        mcps: None,
        handoff: None,
        jobs: Vec::new(),
        run: None,
        rules: None,
    }
}

#[tokio::test]
async fn a_warm_session_receives_only_changed_memory_and_default_skills() {
    let dir = tempfile::tempdir().unwrap();
    let (claude, received) = fake_claude(dir.path()).await;
    let base = specialist_turn(None);
    let profile = base.profile.clone().unwrap();
    let fingerprint = claude.fingerprint(&claude.ready().unwrap(), &profile, &base);
    let native = Uuid::new_v4().to_string();
    let dirs = claude
        .ready()
        .unwrap()
        .host
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
        default_skills: Default::default(),
        context: Default::default(),
    };
    let current = super::super::default_skills::for_session(None).applied;
    let mut up_to_date = warm(Some(7));
    up_to_date.default_skills = current.clone();
    let mut stale = up_to_date.clone();
    stale
        .default_skills
        .insert("unslop".into(), "old-content-hash".into());
    let mut incompatible = up_to_date.clone();
    incompatible.fingerprint = "old-provider-fingerprint".into();
    let memory = "Owner correction: rebase branches, never merge them.";

    for (session, refreshed, skills_changed, same_session) in [
        (None, true, true, false),
        (Some(warm(Some(6))), true, true, true),
        (Some(warm(Some(7))), false, true, true),
        (Some(stale), false, true, true),
        (Some(up_to_date), false, false, true),
        (Some(incompatible), true, true, false),
    ] {
        let resumed = session.as_ref().map(|s| s.native_id.clone());
        let sink = super::super::recording::Recorded::default();
        let outcome = claude
            .run(specialist_turn(session), &sink, Stop::default())
            .await;
        assert_eq!(outcome, TurnOutcome::Completed);
        let text = super::super::recording::received(&received).pop().unwrap();
        assert_eq!(text.contains(memory), refreshed, "{resumed:?}: {text}");
        assert_eq!(text.contains("Avoid em dashes entirely."), skills_changed);
        assert!(text.ends_with("Clean up the stale branches."));
        let state = sink.last_session();
        assert_eq!(state.memory_revision, Some(7));
        let before_send = sink.sessions.lock().unwrap().first().unwrap().clone();
        if skills_changed {
            assert_ne!(before_send.default_skills, current);
        }
        assert_eq!(state.default_skills, current);
        if let Some(native) = resumed {
            assert_eq!(state.native_id == native, same_session);
        }
    }
}

#[tokio::test]
async fn files_reach_claude_in_order_with_images_inline() {
    use crate::app::messages::{Attachment, MessagePart};
    use crate::app::providers::InputFile;
    let dir = tempfile::tempdir().unwrap();
    let (claude, received) = fake_claude(dir.path()).await;
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
    let outcome = claude.run(input, &sink, Stop::default()).await;
    assert_eq!(outcome, TurnOutcome::Completed);

    let project = claude.project_dir(bot_id).unwrap();
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

async fn fake_inventory_claude(dir: &std::path::Path, builtins: Option<&[&str]>) -> Claude {
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
    .await
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
        let claude = fake_inventory_claude(dir.path(), None).await;
        let sink = super::super::recording::Recorded::default();
        let outcome = claude.run(turn_from(origin), &sink, Stop::default()).await;
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
        let claude = fake_inventory_claude(dir.path(), Some(&[tool])).await;
        let sink = super::super::recording::Recorded::default();
        let outcome = claude
            .run(turn_from(Origin::ConnectorEvent), &sink, Stop::default())
            .await;
        assert!(
            matches!(&outcome, TurnOutcome::Failed { code, message }
                if *code == FailureCode::PolicyViolation && message.contains("unexpected tools")),
            "{tool}: {outcome:?}"
        );
    }
}

#[test]
fn a_connector_event_never_resumes_a_session_with_host_tools() {
    use crate::app::providers::Origin;
    let claude = Claude::offline("");
    let owner = turn_from(Origin::OwnerRequest);
    let event = turn_from(Origin::ConnectorEvent);
    let profile = owner.profile.clone().unwrap();
    assert_ne!(
        claude.fingerprint(&claude.ready().unwrap(), &profile, &owner),
        claude.fingerprint(&claude.ready().unwrap(), &profile, &event)
    );
}

pub(crate) fn pinned_fingerprints() -> Vec<(String, &'static str, String)> {
    let mut claude = Claude::offline("acct:org");
    let mut input = specialist_turn(None);
    input.bot_id = Uuid::nil();
    input.bot_name = "Golden".into();
    input.delegation.registry = vec![(Uuid::from_u128(7), "Other".into())];
    let profile = Profile {
        model: "claude-opus-5-5".into(),
        reasoning_effort: "high".into(),
        service_tier: "default".into(),
    };
    crate::providers::fingerprints::each_case(
        &mut input,
        [
            "cc42a604e8a55ac941b487a5da1aec2c",
            "b0de700ec963e25e9bede87eedadc0fc",
            "1b03e3d631aca27de958e9ace0c63a34",
            "39ea0d905d81ac43c79d8298858119ce",
        ],
        |input, tools| {
            if !tools {
                claude.tool_url = None;
            }
            claude.fingerprint(&claude.ready().unwrap(), &profile, input)
        },
    )
}

#[test]
fn initialize_lists_models_by_their_exact_ids() {
    let answer = json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": "else-models",
            "response": { "models": [
                { "value": "default", "resolvedModel": "claude-opus-5-5",
                  "supportsEffort": true, "supportedEffortLevels": ["low", "max"] },
                { "value": "fable", "resolvedModel": "claude-fable-5-2",
                  "supportsEffort": true, "supportedEffortLevels": ["high"] },
                { "value": "claude-haiku-4-5-20251001" },
            ] },
        },
    })
    .to_string();
    assert!(protocol::listed_models(&answer, "other").is_none());
    assert!(protocol::listed_models(r#"{"type":"system"}"#, "else-models").is_none());
    let listed = protocol::listed_models(&answer, "else-models")
        .unwrap()
        .unwrap();
    let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "claude-opus-5-5",
            "claude-fable-5-2",
            "claude-haiku-4-5-20251001"
        ]
    );
    assert_eq!(listed[1].efforts.as_deref(), Some(&["high".to_owned()][..]));
    assert_eq!(
        listed[2].efforts.as_deref(),
        Some(&[][..]),
        "no effort, so not runnable"
    );
    let models = Models::new(&DEFINITION);
    models.listed(listed, chrono::Utc::now()).unwrap();
    assert_eq!(
        models.current().ids(),
        ["claude-opus-5-5", "claude-fable-5-2"]
    );
}

/// A probe that counts its runs and waits for the gate.
fn gated_probe(
    probes: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
    gate: &std::sync::Arc<tokio::sync::Semaphore>,
    entered: &std::sync::Arc<tokio::sync::Notify>,
) -> turn::Probe<Login> {
    let (probes, gate, entered) = (probes.clone(), gate.clone(), entered.clone());
    Box::new(move || {
        let (probes, gate, entered) = (probes.clone(), gate.clone(), entered.clone());
        Box::pin(async move {
            let n = probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            entered.notify_one();
            let _open = gate.acquire().await.unwrap();
            Err(format!("signed out ({n})"))
        })
    })
}

/// Rechecks made while a probe runs wait for it, then share one new probe,
/// since the running one may have read the state from before they asked.
#[tokio::test(flavor = "current_thread")]
async fn rechecks_during_a_probe_share_the_next_one() {
    use std::sync::atomic::Ordering;
    let probes = std::sync::Arc::default();
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let entered = std::sync::Arc::default();
    let probe = gated_probe(&probes, &gate, &entered);
    let claude = std::sync::Arc::new(Claude::probed(ClaudeCode, probe).await);
    entered.notified().await;
    assert_eq!(probes.load(Ordering::SeqCst), 1, "probed at startup");

    let held = gate.clone().acquire_owned().await.unwrap();
    let recheck = |claude: &std::sync::Arc<Claude>| {
        let claude = claude.clone();
        tokio::spawn(async move { claude.reprobe().await })
    };
    let first = recheck(&claude);
    entered.notified().await;
    let others = [recheck(&claude), recheck(&claude)];
    // On this one-thread runtime the others run up to the probe's lock.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    drop(held);
    first.await.unwrap();
    for recheck in others {
        assert_eq!(
            recheck.await.unwrap(),
            Availability::Unavailable("signed out (3)".into())
        );
    }
    assert_eq!(
        probes.load(Ordering::SeqCst),
        3,
        "two rechecks that arrived mid-probe, one new probe"
    );
}

/// A recheck whose caller goes away still finishes and is published, so
/// the next caller does not start it over.
#[tokio::test(flavor = "multi_thread")]
async fn a_recheck_outlives_its_caller() {
    use std::sync::atomic::Ordering;
    let probes = std::sync::Arc::default();
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    let entered = std::sync::Arc::default();
    let probe = gated_probe(&probes, &gate, &entered);
    let claude = std::sync::Arc::new(Claude::probed(ClaudeCode, probe).await);
    entered.notified().await;

    let held = gate.clone().acquire_owned().await.unwrap();
    let caller = {
        let claude = claude.clone();
        tokio::spawn(async move { claude.reprobe().await })
    };
    entered.notified().await;
    caller.abort();
    drop(held);
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while claude.availability() != Availability::Unavailable("signed out (2)".into()) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the abandoned recheck was published");
    assert_eq!(probes.load(Ordering::SeqCst), 2);
}

/// A login that expires mid-turn makes the provider unavailable for the
/// turns after it, without a restart.
#[tokio::test(flavor = "multi_thread")]
async fn a_login_that_expires_mid_turn_is_picked_up() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let login = dir.join("logged-in");
    std::fs::write(&login, "").unwrap();
    let program = dir.join("claude");
    std::fs::write(
        &program,
        format!(
            r#"#!/usr/bin/env python3
import json, os, sys
a = sys.argv[1:]
login = {login:?}
if a[:1] == ["--version"]: print("2.1.999 (Claude Code)"); sys.exit()
if a[:1] == ["auth"]:
    print(json.dumps({{"loggedIn": os.path.exists(login)}})); sys.exit(0 if os.path.exists(login) else 1)
out = lambda e: print(json.dumps(e), flush=True)
out({{"type": "system", "subtype": "init", "model": a[a.index("--model") + 1],
     "tools": a[a.index("--tools") + 1].split(","),
     "mcp_servers": [{{"name": "elsebot", "status": "connected"}}]}})
msg = json.loads(sys.stdin.readline())
out({{"type": "user", "uuid": msg["uuid"], "isReplay": True}})
os.remove(login)
failure = "Failed to authenticate. API Error: 401 OAuth access token is invalid."
out({{"type": "assistant", "parent_tool_use_id": None, "error": "authentication_failed",
     "is_api_error_message": True, "api_error_status": 401,
     "message": {{"model": "<synthetic>", "role": "assistant", "content": [{{"type": "text", "text": failure}}]}}}})
out({{"type": "result", "subtype": "success", "is_error": True, "result": failure, "num_turns": 1,
     "terminal_reason": "api_error"}})
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
    )
    .await;
    assert_eq!(claude.availability(), Availability::Available);

    let sink = super::super::recording::Recorded::default();
    let outcome = claude
        .run(specialist_turn(None), &sink, Stop::default())
        .await;
    assert!(
        matches!(
            &outcome,
            TurnOutcome::Failed {
                code: FailureCode::ProviderFailed,
                ..
            }
        ),
        "{outcome:?}"
    );
    let signed_out = Availability::Unavailable(
        "Claude Code is not logged in on the host (run `claude` and log in).".into(),
    );
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while claude.availability() != signed_out {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the failed turn rechecked the login");
}
