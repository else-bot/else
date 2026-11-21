use super::policy::Reported;
use super::*;
use crate::app::providers::{
    Availability, FailureCode, HistoryEntry, MemoryContext, Provider, SessionState,
};
use crate::db::turns::ItemKind;
use serde_json::{Value, json};
use uuid::Uuid;

impl Codex {
    fn offline(tool_url: Option<&str>) -> Self {
        let codex = Self::new(
            CodexServer,
            Ok(Ready {
                host: Host::new(std::path::Path::new("/data"), std::path::Path::new("/bots")),
                program: PathBuf::new().into(),
                version: "codex-cli 9.0.0".into(),
                login: PathBuf::new(),
            }),
        );
        match tool_url {
            Some(url) => codex.with_tools(url.to_owned()),
            None => codex,
        }
    }

    fn fingerprint_of(&self, profile: &Profile, input: &TurnInput, account: &str) -> String {
        self.fingerprint(self.ready.as_ref().unwrap(), profile, input, account)
    }
}

fn input(history: Vec<(i64, ItemKind, &str)>) -> TurnInput {
    TurnInput {
        bot_id: Uuid::new_v4(),
        conversation_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        turn_no: 9,
        bot_name: "Work".into(),
        text: "latest".into(),
        parts: Vec::new(),
        steered: Vec::new(),
        files: Vec::new(),
        profile: None,
        history: history
            .into_iter()
            .map(|(turn_no, kind, text)| HistoryEntry {
                turn_no,
                kind,
                text: text.into(),
                status: crate::db::turns::TurnStatus::Completed,
                error: None,
                parts: Vec::new(),
            })
            .collect(),
        session: None,
        tool_grant: None,
        memory: None,
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

#[test]
fn saved_task_permissions_are_refreshed_even_in_resumed_sessions() {
    let mut turn = input(Vec::new());
    turn.delegation.origin = crate::app::providers::Origin::OwnerRequest;
    turn.standing_permissions
        .push(crate::app::questions::PermissionTarget::Task {
            task: "Manage Tailscale".into(),
            target: "SSH host omarchy-m1".into(),
            kind: crate::app::questions::Kind::Destructive,
            covers: crate::app::questions::Covers::Host,
        });
    let skills = super::super::default_skills::Update {
        text: None,
        applied: Default::default(),
    };
    let recorded = prompt::turn_packet(
        &turn,
        turn.turn_no,
        None,
        &skills,
        &crate::providers::context::for_session(&turn, None),
    )
    .0;
    assert!(recorded.contains("Manage Tailscale"));
    assert!(recorded.contains("SSH host omarchy-m1"));
    assert!(!recorded.contains("Continue the owner's request with these results."));
    turn.standing_permissions.clear();
    let revoked = prompt::turn_packet(
        &turn,
        turn.turn_no,
        None,
        &skills,
        &crate::providers::context::for_session(&turn, None),
    )
    .0;
    assert!(revoked.contains("Current saved scopes that apply to this request: []"));
    assert!(!revoked.contains("Manage Tailscale"));
    turn.delegation.task_depth = 1;
    assert!(
        prompt::turn_packet(
            &turn,
            turn.turn_no,
            None,
            &skills,
            &crate::providers::context::for_session(&turn, None)
        )
        .0
        .contains("[else.bot saved permissions]"),
        "work delegated for an owner request sees its task scopes"
    );
    for (origin, sees) in [
        (crate::app::providers::Origin::RoutineRun, true),
        (crate::app::providers::Origin::WebhookWake, false),
    ] {
        turn.delegation.origin = origin;
        assert_eq!(
            prompt::turn_packet(
                &turn,
                turn.turn_no,
                None,
                &skills,
                &crate::providers::context::for_session(&turn, None)
            )
            .0
            .contains("[else.bot saved permissions]"),
            sees,
            "a task delegated by a {origin:?}"
        );
    }
}

#[test]
fn packet_includes_only_missed_history() {
    let input = input(vec![
        (1, ItemKind::User, "a"),
        (1, ItemKind::Assistant, "b"),
        (2, ItemKind::User, "c"),
    ]);
    assert_eq!(prompt::packet(&input, 2), "latest");
    let packet = prompt::packet(&input, 1);
    assert!(packet.contains("[turn 2] Owner: c") && !packet.contains("Owner: a"));
    assert!(packet.ends_with("Owner's latest message:\nlatest"));
    let full = prompt::packet(&input, 0);
    assert!(full.contains("[turn 1] Owner: a\n[turn 1] Assistant: b\n[turn 2] Owner: c"));
}

#[test]
fn history_names_earlier_files_by_their_staged_copies() {
    use crate::app::messages::{Attachment, MessagePart};
    let file = Attachment {
        id: Uuid::nil(),
        name: "plan.pdf".into(),
        media_type: "application/pdf".into(),
        size: 9,
    };
    let mut input = input(vec![
        (1, ItemKind::User, "Read [File: plan.pdf]"),
        (1, ItemKind::Assistant, "Done."),
    ]);
    input.history[0].parts = vec![
        MessagePart::Text {
            text: "Read ".into(),
        },
        MessagePart::File { file: file.clone() },
    ];
    let packet = prompt::packet(&input, 0);
    assert!(
        packet.contains(&format!(
            "[turn 1] Owner: Read [File: plan.pdf (application/pdf, 9 bytes), saved at .attachments/{}/plan.pdf]\n",
            Uuid::nil()
        )),
        "{packet}"
    );
    assert!(packet.ends_with("latest"));
}

#[test]
fn packet_respects_the_budget_and_states_omissions() {
    let big = "x".repeat(20 * 1024);
    let input = input(vec![
        (1, ItemKind::User, "first"),
        (2, ItemKind::User, &big),
        (3, ItemKind::User, &big),
    ]);
    let packet = prompt::packet(&input, 0);
    assert!(
        packet.contains("[turn 1] Owner: first"),
        "first owner message kept"
    );
    assert!(packet.contains("omitted"));
    assert!(packet.len() < prompt::HISTORY_BUDGET + 2048);
}

#[test]
fn a_welcome_does_not_displace_the_first_owner_message() {
    let big = "x".repeat(20 * 1024);
    let input = input(vec![
        (1, ItemKind::Assistant, "Welcome to else.bot."),
        (2, ItemKind::User, "Remember my original request."),
        (3, ItemKind::Assistant, &big),
        (4, ItemKind::User, &big),
    ]);
    let (packet, selection) = prompt::packet_with_selection(&input, 0);
    assert_eq!(
        packet
            .matches("Owner: Remember my original request.")
            .count(),
        1
    );
    assert_eq!(selection.selected, vec![2, 4]);
    assert_eq!(selection.omitted, vec![1, 3]);
    assert!(packet.len() < prompt::HISTORY_BUDGET + 2048);
}

#[test]
fn a_welcome_keeps_its_place_in_history() {
    let input = input(vec![
        (1, ItemKind::Assistant, "Welcome to else.bot."),
        (2, ItemKind::User, "My original request."),
        (2, ItemKind::Assistant, "First reply."),
    ]);
    let (packet, selection) = prompt::packet_with_selection(&input, 0);
    assert!(packet.contains(
        "[turn 1] Assistant: Welcome to else.bot.\n[turn 2] Owner: My original request.\n[turn 2] Assistant: First reply."
    ));
    assert_eq!(selection.selected, vec![1, 2]);
    assert!(selection.omitted.is_empty());
}

#[test]
fn a_large_first_message_still_fits_the_budget() {
    let first = "f".repeat(30 * 1024);
    let other = "o".repeat(10 * 1024);
    let input = input(vec![
        (1, ItemKind::User, &first),
        (2, ItemKind::User, &other),
        (3, ItemKind::User, &other),
        (4, ItemKind::User, &other),
    ]);
    let packet = prompt::packet(&input, 0);
    assert!(
        packet.len() <= prompt::HISTORY_BUDGET + 1024,
        "{}",
        packet.len()
    );
    assert!(packet.contains("omitted"));
}

#[test]
fn reported_settings_are_checked_field_by_field() {
    let profile = Profile {
        model: "gpt-6.1-sol".into(),
        reasoning_effort: "medium".into(),
        service_tier: "priority".into(),
    };
    let partial =
        Reported::from_response(&json!({ "model": "gpt-6.1-sol", "reasoningEffort": null }));
    assert_eq!(
        partial.mismatch(&profile),
        None,
        "unknown fields are not mismatches"
    );
    let other_model =
        Reported::from_response(&json!({ "model": "gpt-5", "reasoningEffort": null }));
    assert!(other_model.mismatch(&profile).unwrap().contains("gpt-5"));
    let other_tier = Reported::from_response(&json!({ "serviceTier": "default" }));
    assert!(other_tier.mismatch(&profile).is_some());
}

#[tokio::test]
async fn catalog_offers_current_and_legacy_models_as_exact_profiles() {
    let codex = Codex::detect(None, None, None).await;
    let catalog = codex.catalog();
    let featured: Vec<&str> = catalog
        .models
        .iter()
        .filter(|model| model.featured)
        .map(|model| model.id.as_str())
        .collect();
    assert_eq!(
        featured,
        [
            "gpt-6.2-sol",
            "gpt-6.2-astra",
            "gpt-6.1-terra",
            "gpt-6-luna"
        ]
    );
    for model in DEFINITION.fallback {
        assert!(catalog.model(model).is_some(), "missing {model}");
        for effort in DEFINITION.efforts {
            for tier in DEFINITION.tiers {
                let profile = Profile {
                    model: (*model).into(),
                    reasoning_effort: (*effort).into(),
                    service_tier: (*tier).into(),
                };
                assert!(codex.validate_profile(&profile).is_ok(), "{profile:?}");
            }
        }
    }
}

#[tokio::test]
async fn profile_validation_refuses_delegating_effort() {
    let codex = Codex::detect(None, None, None).await;
    assert!(codex.ready.is_err());
    let ok = Profile {
        model: "gpt-6.1-sol".into(),
        reasoning_effort: "medium".into(),
        service_tier: "priority".into(),
    };
    assert!(codex.validate_profile(&ok).is_ok());
    for bad in [
        Profile {
            reasoning_effort: "ultra".into(),
            ..ok.clone()
        },
        Profile {
            model: "gpt-4".into(),
            ..ok.clone()
        },
        Profile {
            service_tier: "turbo".into(),
            ..ok.clone()
        },
    ] {
        assert!(codex.validate_profile(&bad).is_err(), "{bad:?}");
    }
    assert!(matches!(codex.availability(), Availability::Unavailable(_)));
}

#[tokio::test]
async fn fingerprint_changes_with_profile_and_name() {
    let a = input(vec![]);
    let profile = Profile {
        model: "gpt-6.1-sol".into(),
        reasoning_effort: "medium".into(),
        service_tier: "priority".into(),
    };
    let codex = Codex::offline(None);
    let base = codex.fingerprint_of(&profile, &a, "chatgpt:a");
    assert_ne!(
        base,
        codex.fingerprint_of(&profile, &a, "chatgpt:b"),
        "account"
    );
    let high = Profile {
        reasoning_effort: "high".into(),
        ..profile.clone()
    };
    assert_ne!(base, codex.fingerprint_of(&high, &a, "chatgpt:a"));
    let mut b = a.clone();
    b.bot_name = "Different".into();
    assert_ne!(base, codex.fingerprint_of(&profile, &b, "chatgpt:a"));
    assert_eq!(
        base,
        codex.fingerprint_of(&profile, &a.clone(), "chatgpt:a")
    );
    let dir = tempfile::tempdir().unwrap();
    let version = async |v: &str| {
        detect_with(dir.path(), &format!("echo '{v}'"), true)
            .await
            .fingerprint_of(&profile, &a, "chatgpt:a")
    };
    assert_ne!(
        version("codex-cli 9.0.0").await,
        version("codex-cli 9.0.1").await
    );
}

async fn detect_with(dir: &std::path::Path, script: &str, logged_in: bool) -> Codex {
    use std::os::unix::fs::PermissionsExt;
    let program = dir.join("codex");
    std::fs::write(&program, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let auth = dir.join("auth.json");
    if logged_in {
        std::fs::write(&auth, "{}").unwrap();
    } else {
        let _ = std::fs::remove_file(&auth);
    }
    Codex::detect(Some(Host::new(dir, dir)), Some(program), Some(auth)).await
}

async fn fake_codex(dir: &std::path::Path) -> (Codex, std::path::PathBuf) {
    fake_codex_with(dir, &json!([]), &[]).await
}

async fn fake_codex_with(
    dir: &std::path::Path,
    servers: &Value,
    notifications: &[Value],
) -> (Codex, std::path::PathBuf) {
    let received = dir.join("received.jsonl");
    let inputs = dir.join("inputs.jsonl");
    let threads = dir.join("threads.jsonl");
    let launched = dir.join("launched-config.toml");
    let launches = dir.join("launches.jsonl");
    let loaded = dir.join("loaded-catalog.json");
    let export_fails = dir.join("export-fails");
    let ignore_catalog = dir.join("ignore-catalog");
    let one_model = dir.join("one-model");
    let long_history = dir.join("long-history");
    let account_failures = dir.join("account-failures");
    let mcp_failures = dir.join("mcp-failures");
    let catalog = json!({
        "models": DEFINITION.fallback.iter().chain(&["hidden-model"]).map(|slug| json!({
            "slug": slug,
            "context_window": 272000,
            "base_instructions": "kept",
            "apply_patch_tool_type": "freeform",
            "shell_type": "unified_exec",
            "experimental_supported_tools": ["clock"],
        })).collect::<Vec<_>>(),
    });
    let catalog = format!("{:?}", catalog.to_string());
    let notifications = format!("{:?}", Value::from(notifications).to_string());
    let servers = format!("{:?}", servers.to_string());
    let script = format!(
        r#"python3 -c 'import json, sys; print(json.dumps(sys.argv[1:]))' "$@" >> {launches:?}
if [ "$1" = debug ]; then
    [ -e {export_fails:?} ] && exit 1
    exec python3 -c 'import json,os; data=json.loads({catalog}); data["models"]=data["models"][:1] if os.path.exists({one_model:?}) else data["models"]; print(json.dumps(data))'
fi
if [ "$1" = -c ] && [ "$3" = debug ]; then
    exec python3 -c 'import json,os,sys; data=json.loads({catalog}) if os.path.exists({ignore_catalog:?}) else json.load(open(json.loads(sys.argv[1].split("=",1)[1]))); data["models"]=data["models"][:1] if os.path.exists({one_model:?}) else data["models"]; print(json.dumps(data))' "$2"
fi
cat "$CODEX_HOME/config.toml" > {launched:?}
exec python3 -c '
import json, os, sys
def out(m): print(json.dumps(m), flush=True)
notifications = json.loads({notifications})
models = [m["slug"] for m in json.loads({catalog})["models"]]
if os.path.exists({one_model:?}): models = models[:1]
for flag, value in zip(sys.argv, sys.argv[1:]):
    if flag == "-c" and value.startswith("model_catalog_json="):
        catalog = open(json.loads(value.split("=", 1)[1])).read()
        open({loaded:?}, "w").write(catalog)
        if not os.path.exists({ignore_catalog:?}):
            models = [m["slug"] for m in json.loads(catalog)["models"]]
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    method, p = m["method"], m.get("params") or {{}}
    if method == "account/read" and os.path.exists({account_failures:?}):
        left = int(open({account_failures:?}).read())
        open({account_failures:?}, "w").write(str(left - 1))
        if left > 0:
            out({{"id": m["id"], "error": {{"code": -32603, "message": "workspace routing discovery timed out"}}}})
            continue
    if method == "mcpServerStatus/list" and os.path.exists({mcp_failures:?}):
        left = int(open({mcp_failures:?}).read())
        open({mcp_failures:?}, "w").write(str(left - 1))
        if left > 0:
            out({{"id": m["id"], "result": {{"data": [{{"name": "elsebot", "tools": {{}}, "toolsError": "MCP startup failed: connection refused"}}]}}}})
            continue
    if method == "mcpServerStatus/list": r = {{"data": json.loads({servers})}}
    elif method == "skills/list": r = {{"data": [{{"skills": []}}]}}
    elif method == "account/read": r = {{"account": {{"type": "chatgpt", "email": "o@example.test"}}}}
    elif method == "model/list": r = {{"data": [{{"id": s, "model": s}} for s in models], "nextCursor": None}}
    elif method in ("thread/start", "thread/resume"):
        with open({threads:?}, "a") as f: f.write(json.dumps(p) + "\n")
        turns = [{{"id": "old", "items": [{{"type": "agentMessage", "text": "x" * (9 << 20)}}]}}] if method == "thread/resume" and os.path.exists({long_history:?}) and not p.get("excludeTurns") else []
        r = {{"thread": {{"id": p.get("threadId", "fresh-thread"), "turns": turns}}}}
    elif method == "turn/start":
        with open({received:?}, "a") as f: f.write(json.dumps(p["input"][0]["text"]) + "\n")
        with open({inputs:?}, "a") as f: f.write(json.dumps(p["input"]) + "\n")
        r = {{"turn": {{"id": "native-turn"}}}}
    else: r = {{}}
    out({{"id": m["id"], "result": r}})
    if method == "turn/start":
        for n in notifications:
            out({{"method": n["method"], "params": {{"threadId": p["threadId"],
                 "turnId": "native-turn", **n["params"]}}}})
        out({{"method": "turn/completed", "params": {{"threadId": p["threadId"],
             "turn": {{"id": "native-turn", "status": "completed"}}}}}})
' "$@""#
    );
    let codex = detect_with(
        dir,
        &format!("[ \"$1\" = --version ] && echo 'codex-cli 9.0.0' && exit\n{script}"),
        true,
    )
    .await;
    let availability = codex.availability();
    assert!(
        matches!(availability, Availability::Available),
        "{availability:?}"
    );
    (codex, received)
}

const TOOL_URL: &str = "http://127.0.0.1:1/tools/v1/mcp";

fn elsebot_server() -> Value {
    let tools: serde_json::Map<String, Value> = crate::app::tools::TOOL_NAMES
        .iter()
        .map(|t| ((*t).to_owned(), json!({})))
        .collect();
    json!([{ "name": crate::app::tools::SERVER_NAME, "tools": tools }])
}

fn turn_from(origin: crate::app::providers::Origin) -> TurnInput {
    let mut turn = TurnInput {
        profile: Some(Profile {
            model: DEFINITION.fallback[0].into(),
            reasoning_effort: "medium".into(),
            service_tier: "default".into(),
        }),
        tool_grant: Some("tg_test".into()),
        ..input(Vec::new())
    };
    turn.delegation.origin = origin;
    turn
}

#[tokio::test]
async fn a_connector_event_launches_without_host_tools_but_keeps_elsebot_tools() {
    use crate::app::providers::Origin;
    for (origin, host_tools) in [
        (Origin::ConnectorEvent, false),
        (Origin::OwnerRequest, true),
        (Origin::RoutineRun, true),
        (Origin::Other, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (codex, _) = fake_codex_with(dir.path(), &elsebot_server(), &[]).await;
        let codex = codex.with_tools(TOOL_URL.into());
        let sink = super::super::recording::Recorded::default();
        let outcome = codex.run(turn_from(origin), &sink, Stop::default()).await;
        assert_eq!(outcome, TurnOutcome::Completed, "{origin:?}");
        let launched = std::fs::read_to_string(dir.path().join("launched-config.toml")).unwrap();
        assert_eq!(
            launched,
            codex_home::config_toml(Some(TOOL_URL), host_tools),
            "{origin:?}"
        );
        assert!(
            launched.contains(&format!("shell_tool = {host_tools}\n")),
            "{origin:?}"
        );
        assert!(launched.contains("code_mode_host = true\n"), "{origin:?}");
        assert!(launched.contains("[mcp_servers.elsebot]"), "{origin:?}");
        let thread: Value = serde_json::from_str(
            std::fs::read_to_string(dir.path().join("threads.jsonl"))
                .unwrap()
                .lines()
                .last()
                .unwrap(),
        )
        .unwrap();
        let sandbox = if host_tools {
            "danger-full-access"
        } else {
            "read-only"
        };
        assert_eq!(thread["sandbox"], sandbox, "{origin:?}");
        assert_eq!(thread["approvalPolicy"], "never", "{origin:?}");
        // Only the full catalog defers else.bot's tools behind ALL_TOOLS.
        let instructions = thread["developerInstructions"].as_str().unwrap();
        assert_eq!(
            instructions.contains("ALL_TOOLS.filter(t =>"),
            host_tools,
            "{origin:?}"
        );
        let launches = launches(dir.path());
        if host_tools {
            assert_eq!(launches, vec![vec!["app-server".to_owned()]], "{origin:?}");
            assert!(!dir.path().join("loaded-catalog.json").exists());
        } else {
            assert_no_native_catalog(dir.path(), &launches, DEFINITION.fallback[0]);
        }
    }
}

fn launches(dir: &std::path::Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(dir.join("launches.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn assert_no_native_catalog(dir: &std::path::Path, launches: &[Vec<String>], model: &str) {
    let [export, verified, launch] = launches else {
        panic!("{launches:?}");
    };
    assert_eq!(export, &["debug", "models"]);
    assert_eq!(&verified[..2], &launch[..2]);
    assert_eq!(&verified[2..], ["debug", "models"]);
    assert_eq!(launch.len(), 3, "{launch:?}");
    assert_eq!(launch[0], "-c");
    assert!(launch[1].starts_with("model_catalog_json=\""), "{launch:?}");
    assert_eq!(launch[2], "app-server");
    let loaded: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("loaded-catalog.json")).unwrap())
            .unwrap();
    assert_eq!(
        loaded,
        json!({ "models": [{
            "slug": model,
            "context_window": 272000,
            "base_instructions": "kept",
            "apply_patch_tool_type": null,
            "shell_type": "disabled",
            "experimental_supported_tools": [],
            "supports_search_tool":false,
        }] })
    );
}

#[tokio::test]
async fn a_connector_event_fails_before_its_turn_without_the_no_native_catalog() {
    use crate::app::providers::Origin;
    for (marker, one) in [
        ("export-fails", false),
        ("ignore-catalog", false),
        ("ignore-catalog", true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (codex, received) = fake_codex_with(dir.path(), &elsebot_server(), &[]).await;
        let codex = codex.with_tools(TOOL_URL.into());
        std::fs::write(dir.path().join(marker), "").unwrap();
        if one {
            std::fs::write(dir.path().join("one-model"), "").unwrap();
        }
        let sink = super::super::recording::Recorded::default();
        let outcome = codex
            .run(turn_from(Origin::ConnectorEvent), &sink, Stop::default())
            .await;
        let TurnOutcome::Failed { code, .. } = &outcome else {
            panic!("{marker}: {outcome:?}");
        };
        assert_eq!(
            *code,
            FailureCode::ProviderUnavailable,
            "{marker}: {outcome:?}"
        );
        assert!(!dir.path().join("threads.jsonl").exists(), "{marker}");
        assert!(!received.exists(), "{marker}");
        if marker == "export-fails" {
            assert_eq!(launches(dir.path()), vec![vec!["debug", "models"]]);
        }
    }
}

#[tokio::test]
async fn summaries_and_checkpoints_run_on_the_no_native_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let (codex, _) = fake_codex(dir.path()).await;
    let profile = turn_from(crate::app::providers::Origin::Other)
        .profile
        .unwrap();
    let result = codex
        .summarize_once(
            Uuid::new_v4(),
            &profile,
            "summarize",
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(result, Err("Codex returned an empty summary".into()));
    assert_no_native_catalog(dir.path(), &launches(dir.path()), &profile.model);

    let dir = tempfile::tempdir().unwrap();
    let (codex, _) = fake_codex(dir.path()).await;
    std::fs::write(dir.path().join("ignore-catalog"), "").unwrap();
    let result = codex
        .summarize_once(
            Uuid::new_v4(),
            &profile,
            "summarize",
            &CancellationToken::new(),
        )
        .await;
    assert!(
        result
            .as_ref()
            .is_err_and(|e| e.contains("catalog without native tools")),
        "{result:?}"
    );
    assert!(!dir.path().join("threads.jsonl").exists());
}

/// A notification about `item` for the fake Codex's current turn.
fn item(method: &str, item: Value) -> Value {
    json!({ "method": method, "params": { "item": item } })
}

/// `item` started and completed.
fn whole(item: Value) -> [Value; 2] {
    [
        self::item("item/started", item.clone()),
        self::item("item/completed", item),
    ]
}

#[tokio::test]
async fn a_summary_is_its_final_messages_and_refuses_any_other_item() {
    let profile = turn_from(crate::app::providers::Origin::Other)
        .profile
        .unwrap();
    let summarize = |notifications: Vec<Value>| {
        let profile = profile.clone();
        async move {
            let dir = tempfile::tempdir().unwrap();
            let (codex, _) = fake_codex_with(dir.path(), &json!([]), &notifications).await;
            codex
                .summarize_once(
                    Uuid::new_v4(),
                    &profile,
                    "summarize",
                    &CancellationToken::new(),
                )
                .await
        }
    };
    let message = |id: &str, phase: &str, text: &str| {
        whole(json!({ "type": "agentMessage", "id": id, "phase": phase, "text": text }))
    };
    let result = summarize(
        [
            message("c", "commentary", "Reading."),
            whole(json!({ "type": "reasoning", "id": "r" })),
            message("f1", "final_answer", " First."),
            message("f2", "final_answer", "Second. "),
        ]
        .concat(),
    )
    .await;
    assert_eq!(result, Ok("First.\n\nSecond.".into()));
    let plan = json!({ "type": "plan", "id": "p" });
    let forbidden = [
        // A kind a turn would allow, such as a new one, is refused here.
        whole(plan.clone()).to_vec(),
        whole(json!({ "type": "commandExecution", "id": "x" })).to_vec(),
        vec![item(
            "item/completed",
            json!({ "type": "mcpToolCall", "id": "m" }),
        )],
        vec![json!({ "method": "item/started",
                     "params": { "threadId": "other-thread", "turnId": "other", "item": plan } })],
    ];
    for notifications in forbidden {
        let result = summarize(notifications.clone()).await;
        assert!(
            result
                .as_ref()
                .is_err_and(|e| e.contains("Codex produced a")),
            "{notifications:?}: {result:?}"
        );
    }
}

#[tokio::test]
async fn a_failed_account_read_is_asked_once_more_before_the_turn_fails() {
    for (failures, delivered) in [(1, true), (2, false)] {
        let dir = tempfile::tempdir().unwrap();
        let (codex, received) = fake_codex_with(dir.path(), &elsebot_server(), &[]).await;
        let codex = codex.with_tools(TOOL_URL.into());
        std::fs::write(dir.path().join("account-failures"), failures.to_string()).unwrap();
        let sink = super::super::recording::Recorded::default();
        let outcome = codex
            .run(
                turn_from(crate::app::providers::Origin::OwnerRequest),
                &sink,
                Stop::default(),
            )
            .await;
        let asked = std::fs::read_to_string(dir.path().join("account-failures")).unwrap();
        assert_eq!(asked, (failures - 2).to_string(), "asked twice: {failures}");
        assert_eq!(received.exists(), delivered, "{failures}: {outcome:?}");
        if delivered {
            assert_eq!(outcome, TurnOutcome::Completed);
        } else {
            assert_eq!(
                outcome,
                TurnOutcome::Failed {
                    code: FailureCode::ProviderUnavailable,
                    message: "account/read failed: workspace routing discovery timed out (-32603)"
                        .into(),
                }
            );
        }
    }
}

#[tokio::test]
async fn an_empty_tool_inventory_is_listed_once_more_before_the_turn_fails() {
    for (failures, delivered) in [(1, true), (2, false)] {
        let dir = tempfile::tempdir().unwrap();
        let (codex, received) = fake_codex_with(dir.path(), &elsebot_server(), &[]).await;
        let codex = codex.with_tools(TOOL_URL.into());
        std::fs::write(dir.path().join("mcp-failures"), failures.to_string()).unwrap();
        let sink = super::super::recording::Recorded::default();
        let outcome = codex
            .run(
                turn_from(crate::app::providers::Origin::OwnerRequest),
                &sink,
                Stop::default(),
            )
            .await;
        let asked = std::fs::read_to_string(dir.path().join("mcp-failures")).unwrap();
        assert_eq!(
            asked,
            (failures - 2).to_string(),
            "listed twice: {failures}"
        );
        assert_eq!(received.exists(), delivered, "{failures}: {outcome:?}");
        if delivered {
            assert_eq!(outcome, TurnOutcome::Completed);
        } else {
            assert_eq!(
                outcome,
                TurnOutcome::failed(
                    FailureCode::ProviderUnavailable,
                    "else.bot's MCP server reported no tools: MCP startup failed: connection refused",
                )
            );
        }
    }
}

#[tokio::test]
async fn an_unexpected_tool_inventory_is_a_policy_violation() {
    let dir = tempfile::tempdir().unwrap();
    let servers = json!([{ "name": crate::app::tools::SERVER_NAME, "tools": { "shell": {} } }]);
    let (codex, received) = fake_codex_with(dir.path(), &servers, &[]).await;
    let codex = codex.with_tools(TOOL_URL.into());
    let sink = super::super::recording::Recorded::default();
    let outcome = codex
        .run(
            turn_from(crate::app::providers::Origin::OwnerRequest),
            &sink,
            Stop::default(),
        )
        .await;
    assert_eq!(
        outcome,
        TurnOutcome::failed(
            FailureCode::PolicyViolation,
            "else.bot's MCP server offered unexpected tools: [\"shell\"]",
        )
    );
    assert!(!received.exists());
}

#[tokio::test]
async fn a_codex_that_exits_reports_the_end_of_its_stderr_without_the_grant() {
    for leftover in ["", "sleep 30 >/dev/null &"] {
        let dir = tempfile::tempdir().unwrap();
        let codex = detect_with(
            dir.path(),
            &format!(
                "[ \"$1\" = --version ] && echo 'codex-cli 9.0.0' && exit\n\
                 {leftover}\n\
                 printf 'starting\\nthread main panicked: bad grant %s\\nnote: run with RUST_BACKTRACE=1\\n' \"$ELSE_TASK_GRANT\" >&2\n\
                 exit 101"
            ),
            true,
        ).await
        .with_tools(TOOL_URL.into());
        let sink = super::super::recording::Recorded::default();
        let outcome = codex
            .run(
                turn_from(crate::app::providers::Origin::OwnerRequest),
                &sink,
                Stop::default(),
            )
            .await;
        assert_eq!(
            outcome,
            TurnOutcome::failed(
                FailureCode::ProviderUnavailable,
                "the provider process exited: starting / thread main panicked: bad grant [tool grant] / note: run with RUST_BACKTRACE=1",
            ),
            "{leftover}"
        );
    }
}

#[tokio::test]
async fn a_connector_event_fails_on_any_command_or_file_change() {
    use crate::app::providers::Origin;
    for kind in ["commandExecution", "fileChange", "webSearch"] {
        let dir = tempfile::tempdir().unwrap();
        let (codex, _) = fake_codex_with(
            dir.path(),
            &elsebot_server(),
            &[item(
                "item/started",
                json!({ "type": kind, "id": "item-1" }),
            )],
        )
        .await;
        let codex = codex.with_tools(TOOL_URL.into());
        let sink = super::super::recording::Recorded::default();
        let outcome = codex
            .run(turn_from(Origin::ConnectorEvent), &sink, Stop::default())
            .await;
        assert_eq!(
            outcome,
            TurnOutcome::failed(
                FailureCode::PolicyViolation,
                format!("Codex attempted a {kind} item")
            ),
        );
    }
    for kind in ["commandExecution", "fileChange"] {
        let dir = tempfile::tempdir().unwrap();
        let (codex, _) = fake_codex_with(
            dir.path(),
            &elsebot_server(),
            &[item(
                "item/started",
                json!({ "type": kind, "id": "item-1" }),
            )],
        )
        .await;
        let codex = codex.with_tools(TOOL_URL.into());
        let sink = super::super::recording::Recorded::default();
        let outcome = codex
            .run(turn_from(Origin::OwnerRequest), &sink, Stop::default())
            .await;
        assert_eq!(outcome, TurnOutcome::Completed, "{kind}");
    }
}

#[test]
fn a_connector_event_never_resumes_a_thread_with_host_tools() {
    use crate::app::providers::Origin;
    let codex = Codex::offline(Some(TOOL_URL));
    let owner = turn_from(Origin::OwnerRequest);
    let event = TurnInput {
        delegation: crate::app::providers::DelegationContext {
            origin: Origin::ConnectorEvent,
            ..owner.delegation.clone()
        },
        ..owner.clone()
    };
    let profile = owner.profile.clone().unwrap();
    assert_ne!(
        codex.fingerprint_of(&profile, &owner, "a"),
        codex.fingerprint_of(&profile, &event, "a")
    );
}

#[tokio::test]
async fn a_warm_thread_receives_only_changed_memory_and_default_skills() {
    let dir = tempfile::tempdir().unwrap();
    let (codex, received) = fake_codex(dir.path()).await;
    let turn = |session| TurnInput {
        turn_no: 3,
        bot_name: "GitHub".into(),
        text: "Clean up the stale branches.".into(),
        profile: Some(Profile {
            model: DEFINITION.fallback[0].into(),
            reasoning_effort: "medium".into(),
            service_tier: "default".into(),
        }),
        session,
        memory: Some(MemoryContext {
            revision: 7,
            text: "[else.bot memory] Owner correction: rebase branches, never merge them.\n\n"
                .into(),
            lines: Vec::new(),
        }),
        bot_id: Uuid::from_u128(7),
        ..input(Vec::new())
    };
    let base = turn(None);
    let fingerprint = codex.fingerprint_of(
        base.profile.as_ref().unwrap(),
        &base,
        "chatgpt:o@example.test",
    );
    let warm = |memory_revision| SessionState {
        native_id: "warm-thread".into(),
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

    for (session, native, refreshed, skills_changed) in [
        (None, "fresh-thread", true, true),
        (Some(warm(Some(6))), "warm-thread", true, true),
        (Some(warm(Some(7))), "warm-thread", false, true),
        (Some(stale), "warm-thread", false, true),
        (Some(up_to_date), "warm-thread", false, false),
        (Some(incompatible), "fresh-thread", true, true),
    ] {
        let sink = super::super::recording::Recorded::default();
        let outcome = codex.run(turn(session), &sink, Stop::default()).await;
        assert_eq!(outcome, TurnOutcome::Completed);
        let text = super::super::recording::received(&received).pop().unwrap();
        assert_eq!(text.contains(memory), refreshed, "{native}: {text}");
        assert_eq!(text.contains("Avoid em dashes entirely."), skills_changed);
        assert!(text.ends_with("Clean up the stale branches."));
        let state = sink.last_session();
        let before_send = sink.sessions.lock().unwrap().first().unwrap().clone();
        if skills_changed {
            assert_ne!(before_send.default_skills, current);
        }
        assert_eq!(state.default_skills, current);
        assert_eq!(state.native_id, native);
        assert_eq!(state.memory_revision, Some(7));
    }
}

#[tokio::test]
async fn a_thread_whose_history_outgrew_the_line_limit_still_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let (codex, _) = fake_codex(dir.path()).await;
    std::fs::write(dir.path().join("long-history"), "").unwrap();
    let mut turn = turn_from(crate::app::providers::Origin::RoutineRun);
    let fingerprint = codex.fingerprint_of(
        turn.profile.as_ref().unwrap(),
        &turn,
        "chatgpt:o@example.test",
    );
    turn.session = Some(SessionState {
        native_id: "warm-thread".into(),
        fingerprint,
        considered_through_turn: 2,
        memory_revision: None,
        default_skills: Default::default(),
        context: Default::default(),
    });
    let sink = super::super::recording::Recorded::default();
    let outcome = codex.run(turn, &sink, Stop::default()).await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(sink.last_session().native_id, "warm-thread");
}

#[tokio::test]
async fn files_reach_codex_in_order_with_images_as_local_images() {
    use crate::app::messages::{Attachment, MessagePart};
    use crate::app::providers::InputFile;
    let dir = tempfile::tempdir().unwrap();
    let (codex, _) = fake_codex(dir.path()).await;
    let file = |name: &str, media_type: &str, data: &[u8]| InputFile {
        attachment: Attachment {
            id: Uuid::new_v4(),
            name: name.into(),
            media_type: media_type.into(),
            size: data.len() as u64,
        },
        data: data.into(),
    };
    let image = file("shot.png", "image/png", b"\x89PNG fake");
    let notes = file("notes.txt", "text/plain", b"line one\n");
    let text = |t: &str| MessagePart::Text { text: t.into() };
    let parts = vec![
        text("Compare "),
        MessagePart::File {
            file: image.attachment.clone(),
        },
        text(" with\n  "),
        MessagePart::File {
            file: notes.attachment.clone(),
        },
    ];
    let input = TurnInput {
        text: crate::app::messages::flatten(&parts),
        parts,
        files: vec![image.clone(), notes.clone()],
        profile: Some(Profile {
            model: DEFINITION.fallback[0].into(),
            reasoning_effort: "medium".into(),
            service_tier: "default".into(),
        }),
        ..input(Vec::new())
    };
    let bot_id = input.bot_id;
    let sink = super::super::recording::Recorded::default();
    let outcome = codex.run(input, &sink, Stop::default()).await;
    assert_eq!(outcome, TurnOutcome::Completed);

    let project = dir.path().join(bot_id.to_string());
    let staged = |f: &InputFile| {
        project
            .join(".attachments")
            .join(f.attachment.id.to_string())
            .join(&f.attachment.name)
    };
    assert_eq!(std::fs::read(staged(&image)).unwrap(), b"\x89PNG fake");
    assert_eq!(std::fs::read(staged(&notes)).unwrap(), b"line one\n");
    let sent: Value = serde_json::from_str(
        std::fs::read_to_string(dir.path().join("inputs.jsonl"))
            .unwrap()
            .lines()
            .last()
            .unwrap(),
    )
    .unwrap();
    let canonical = |p: std::path::PathBuf| std::fs::canonicalize(p).unwrap();
    assert_eq!(sent[0]["type"], "text");
    let (skills, message) = sent[0]["text"]
        .as_str()
        .unwrap()
        .split_once("[end else.bot default skills]\n\n")
        .unwrap();
    assert!(skills.contains("Default skill `unslop`"));
    let (context, message) = message.rsplit_once("\n\n").unwrap();
    assert!(context.starts_with("[else.bot bot registry]"));
    assert_eq!(
        message,
        format!(
            "Compare [File: shot.png (image/png, 9 bytes), saved at .attachments/{}/shot.png]",
            image.attachment.id
        )
    );
    assert_eq!(sent[1]["type"], "localImage");
    assert_eq!(
        canonical(sent[1]["path"].as_str().unwrap().into()),
        canonical(staged(&image))
    );
    assert_eq!(
        sent[2],
        json!({ "type": "text", "text": format!(
            " with\n  [File: notes.txt (text/plain, 9 bytes), saved at .attachments/{}/notes.txt]",
            notes.attachment.id
        ) })
    );
    assert_eq!(sent.as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn any_running_logged_in_version_is_available() {
    let dir = tempfile::tempdir().unwrap();
    let codex = detect_with(dir.path(), "echo 'codex-cli 9.0.0'", true).await;
    let availability = codex.availability();
    assert!(
        matches!(availability, Availability::Available),
        "{availability:?}"
    );
    assert_eq!(codex.ready.as_ref().unwrap().version, "codex-cli 9.0.0");
    for (script, logged_in) in [
        ("exit 1", true),
        ("true", true),
        ("echo 'codex-cli 9.0.0'", false),
    ] {
        assert!(
            matches!(
                detect_with(dir.path(), script, logged_in)
                    .await
                    .availability(),
                Availability::Unavailable(_)
            ),
            "{script} {logged_in}"
        );
    }
}

pub(crate) fn pinned_fingerprints() -> [(crate::app::providers::Origin, &'static str, String); 2] {
    use crate::app::providers::Origin;

    let codex = Codex::offline(Some("http://127.0.0.1:1/tools/v1/mcp"));
    let mut input = input(vec![]);
    input.bot_id = Uuid::nil();
    input.bot_name = "Golden".into();
    input.delegation.registry = vec![(Uuid::from_u128(7), "Other".into())];
    let profile = Profile {
        model: "gpt-6.1-sol".into(),
        reasoning_effort: "high".into(),
        service_tier: "priority".into(),
    };
    [
        (Origin::OwnerRequest, "b1bcc186dcb9c2ff60ba4b75928ce0cc"),
        (Origin::ConnectorEvent, "60a3bb40f58a035ad24a01584c79c961"),
    ]
    .map(|(origin, expected)| {
        input.delegation.origin = origin;
        let actual = codex.fingerprint_of(&profile, &input, "chatgpt:me@example.com");
        (origin, expected, actual)
    })
}

#[tokio::test]
async fn an_escalated_stop_ends_a_codex_that_never_answers_its_preflight() {
    let dir = tempfile::tempdir().unwrap();
    let codex = detect_with(
        dir.path(),
        r#"[ "$1" = --version ] && { echo codex-cli 9.0.0; exit 0; }
exec python3 -c '
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    if m.get("method") == "initialize":
        print(json.dumps({"id": m["id"], "result": {}}), flush=True)
'"#,
        true,
    )
    .await;
    let input = turn_from(crate::app::providers::Origin::OwnerRequest);
    let turn_id = input.turn_id;
    let stop = Stop::default();
    let owner = {
        let stop = stop.clone();
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            stop.request();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            stop.escalate();
        }
    };
    let sink = crate::providers::recording::Recorded::default();
    let (outcome, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(codex.run(input, &sink, stop), owner)
    })
    .await
    .expect("the escalation preempted the unanswered preflight");
    assert_eq!(outcome, TurnOutcome::Interrupted);
    assert!(
        sink.sessions.lock().unwrap().is_empty(),
        "nothing was delivered"
    );
    let host = &codex.ready.as_ref().unwrap().host;
    assert!(host.live_processes(turn_id).is_empty());
}

#[test]
fn account_rate_limit_updates_report_both_windows() {
    use crate::app::providers::LimitWindow;
    use crate::providers::cli::attempt::{Event, Protocol};
    let profile = Profile {
        model: "gpt-6.1-sol".into(),
        reasoning_effort: "medium".into(),
        service_tier: "priority".into(),
    };
    let mut events = super::policy::TurnEvents {
        thread_id: "th",
        native_turn: "tu",
        profile: &profile,
        allowed: super::policy::Allowed::Tools(ToolSet::HostAndElsebot),
        current_message: None,
        project: std::path::Path::new("/p"),
        steers: Default::default(),
    };
    let update =
        |snapshot: serde_json::Value| crate::providers::codex_rpc::Incoming::Notification {
            method: "account/rateLimits/updated".into(),
            params: json!({ "rateLimits": snapshot }),
        };
    let reported = events
        .on(&update(json!({ "limitId": "codex",
            "primary": { "usedPercent": 42, "windowDurationMins": 300, "resetsAt": 1794837600 },
            "secondary": { "usedPercent": 7, "resetsAt": 1795233600000_i64 } })))
        .unwrap();
    let expected = vec![
        LimitWindow {
            minutes: 300,
            used_basis_points: 4200,
            resets_at: chrono::DateTime::from_timestamp(1794837600, 0).unwrap(),
        },
        LimitWindow {
            minutes: 10080,
            used_basis_points: 700,
            resets_at: chrono::DateTime::from_timestamp(1795233600, 0).unwrap(),
        },
    ];
    assert!(matches!(reported.as_slice(), [Event::Limits(w)] if *w == expected));
    let other_quota = json!({ "limitId": "codex_other", "primary": { "usedPercent": 90 } });
    assert!(events.on(&update(other_quota)).unwrap().is_empty());
}

#[test]
fn token_usage_of_this_turn_is_reported_per_model_request() {
    use crate::app::providers::Usage;
    use crate::providers::cli::attempt::{Event, Protocol};
    let profile = Profile {
        model: "gpt-6.1-sol".into(),
        reasoning_effort: "medium".into(),
        service_tier: "priority".into(),
    };
    let mut events = super::policy::TurnEvents {
        thread_id: "th",
        native_turn: "tu",
        profile: &profile,
        allowed: super::policy::Allowed::Tools(ToolSet::HostAndElsebot),
        current_message: None,
        project: std::path::Path::new("/p"),
        steers: Default::default(),
    };
    let update = |turn: &str| crate::providers::codex_rpc::Incoming::Notification {
        method: "thread/tokenUsage/updated".into(),
        params: json!({ "threadId": "th", "turnId": turn, "tokenUsage": {
            "total": { "inputTokens": 900, "cachedInputTokens": 800, "outputTokens": 90,
                       "reasoningOutputTokens": 9, "totalTokens": 990 },
            "last": { "inputTokens": 300, "cachedInputTokens": 200, "outputTokens": 30,
                      "reasoningOutputTokens": 3, "totalTokens": 330 } } }),
    };
    let usage = Usage {
        input: 300,
        cached_input: 200,
        cache_write: 0,
        output: 30,
    };
    let reported = events.on(&update("tu")).unwrap();
    assert!(matches!(reported.as_slice(), [Event::Usage(u)] if *u == usage));
    assert!(events.on(&update("other")).unwrap().is_empty());
}

#[test]
fn context_compaction_is_reported_once_per_signal() {
    use crate::providers::cli::attempt::{Event, Protocol};
    let profile = Profile {
        model: "gpt-6.1-sol".into(),
        reasoning_effort: "medium".into(),
        service_tier: "priority".into(),
    };
    let mut events = super::policy::TurnEvents {
        thread_id: "th",
        native_turn: "tu",
        profile: &profile,
        allowed: super::policy::Allowed::Tools(ToolSet::HostAndElsebot),
        current_message: None,
        project: std::path::Path::new("/p"),
        steers: Default::default(),
    };
    let note = |method: &str, params: Value| crate::providers::codex_rpc::Incoming::Notification {
        method: method.into(),
        params,
    };
    let item = note(
        "item/completed",
        json!({ "threadId": "th", "turnId": "tu",
        "item": { "id": "c1", "type": "contextCompaction" } }),
    );
    assert!(matches!(
        events.on(&item).unwrap().as_slice(),
        [Event::Compacted]
    ));
    let thread = note(
        "thread/compacted",
        json!({ "threadId": "th", "turnId": "tu" }),
    );
    assert!(matches!(
        events.on(&thread).unwrap().as_slice(),
        [Event::Compacted]
    ));
    let other = note("thread/compacted", json!({ "threadId": "elsewhere" }));
    assert!(events.on(&other).unwrap().is_empty());
}

#[tokio::test]
async fn debug_models_lists_shown_models_in_priority_order() {
    let exported = json!({ "models": [
        { "slug": "gpt-6.1-sol", "visibility": "list", "priority": 3,
          "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "ultra" }],
          "service_tiers": [{ "id": "priority" }] },
        { "slug": "gpt-reserve", "visibility": "hide", "priority": 1,
          "supported_reasoning_levels": [{ "effort": "low" }], "service_tiers": [] },
        { "slug": "gpt-6.2-sol", "visibility": "list", "priority": 2,
          "supported_reasoning_levels": [{ "effort": "high" }], "service_tiers": [] },
    ] })
    .to_string();
    let listed = codex_home::listed_models(exported.as_bytes()).unwrap();
    let ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["gpt-6.2-sol", "gpt-6.1-sol"]);
    assert_eq!(
        listed[1].tiers.as_deref(),
        Some(&["default".to_owned(), "priority".to_owned()][..])
    );
    let codex = Codex::detect(None, None, None).await;
    codex.models.listed(listed, chrono::Utc::now()).unwrap();
    let sol = codex.catalog().model("gpt-6.1-sol").cloned().unwrap();
    assert_eq!(sol.efforts, ["low"], "ultra is not offered");
    assert_eq!(sol.upgrade.as_deref(), Some("gpt-6.2-sol"));
    assert!(codex_home::listed_models(b"[]").is_err());
}

/// `turn/steer` names the running turn and our message ID; Codex echoes the
/// ID on a user message item (measured on codex-cli 0.169.0), which the
/// attempt takes once.
#[test]
fn a_steered_message_names_the_turn_and_its_echo_is_taken_once() {
    use crate::providers::cli::attempt::{Event, Protocol};
    let profile = Profile {
        model: "gpt-6.1-sol".into(),
        reasoning_effort: "medium".into(),
        service_tier: "priority".into(),
    };
    let mut events = super::policy::TurnEvents {
        thread_id: "th",
        native_turn: "tu",
        profile: &profile,
        allowed: super::policy::Allowed::Tools(ToolSet::HostAndElsebot),
        current_message: None,
        project: std::path::Path::new("/p"),
        steers: Default::default(),
    };
    let steer = crate::app::providers::Steer {
        turn_id: Uuid::new_v4(),
        text: "Also say BANANA.".into(),
        parts: Vec::new(),
        files: Vec::new(),
    };
    let input = events.steer_input(&steer).unwrap();
    assert_eq!(
        (
            &input["threadId"],
            &input["expectedTurnId"],
            &input["clientUserMessageId"]
        ),
        (&json!("th"), &json!("tu"), &json!(steer.turn_id))
    );
    assert!(
        input["input"][0]["text"]
            .as_str()
            .unwrap()
            .ends_with("Also say BANANA.")
    );
    let item = |method: &str, client: Value| crate::providers::codex_rpc::Incoming::Notification {
        method: method.into(),
        params: json!({ "threadId": "th", "turnId": "tu", "item": {
            "type": "userMessage", "id": "i", "clientId": client, "content": [] } }),
    };
    let echo = item("item/started", json!(steer.turn_id));
    assert!(
        events.on(&echo).unwrap().is_empty(),
        "nothing is owed before it is sent"
    );
    events.steer_sent(&steer);
    assert!(
        events
            .on(&item("item/started", Value::Null))
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        events.on(&echo).unwrap().as_slice(),
        [Event::Steered(turn)] if *turn == steer.turn_id
    ));
    assert!(
        events
            .on(&item("item/completed", json!(steer.turn_id)))
            .unwrap()
            .is_empty(),
        "its completion is the same echo"
    );
}
