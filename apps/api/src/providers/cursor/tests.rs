use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::app::providers::{Fenced, Origin, Selection, SessionState, Steer};
use crate::providers::cli::attempt::{Event, Protocol, StopCause};
use protocol::Terminal;

const MODEL: &str = "composer-2";

/// Cursor CLI 2026.08.11's print mode, reduced to what the adapter reads.
/// The prompt's markers pick a scenario; every launch is logged beside it.
const FAKE_CURSOR: &str = r#"#!/usr/bin/env python3
import hashlib, json, os, signal, sys, time
here = os.path.dirname(os.path.realpath(__file__))
a = sys.argv[1:]
if a[:1] == ["--version"]:
    print("2026.08.11-test")
    sys.exit()
def refuse():
    print("Error: Authentication required. Run 'agent login' first.", file=sys.stderr)
    sys.exit(1)
if os.path.exists(os.path.join(here, "refuse-login")):
    refuse()
if a[:1] == ["models"]:
    print("Available models\n\ncomposer-2.5 - Composer 2.5 (default)\ngpt-5.5-high - GPT-5.5 High\n\nTip: use --model <id>")
    sys.exit()
home = os.environ["HOME"]
if not os.path.isfile(os.path.join(home, ".config/cursor/auth.json")):
    refuse()
arg = lambda flag: a[a.index(flag) + 1]
resumed = "--resume" in a
chat = arg("--resume") if resumed else arg("--new-session-id")
prompt = sys.stdin.read()
store = os.path.join(home, ".cursor/chats", hashlib.md5(os.getcwd().encode()).hexdigest(), chat)
if resumed and not os.path.exists(os.path.join(store, "store.db")):
    print(f'Error: Chat "{chat}" not found', file=sys.stderr)
    sys.exit(1)
os.makedirs(store, exist_ok=True)
open(os.path.join(store, "store.db"), "a").close()
with open(os.path.join(here, "launches.jsonl"), "a") as f:
    f.write(json.dumps({"chat": chat, "resumed": resumed, "args": a, "home": home, "prompt": prompt,
        "grant": os.environ.get("ELSE_TASK_GRANT"), "direnv": os.environ.get("DIRENV_DISABLE"),
        "mcp": json.load(open(os.path.join(home, ".cursor/mcp.json"))),
        "config": json.load(open(os.path.join(home, ".cursor/cli-config.json")))}) + "\n")
out = lambda e: print(json.dumps(dict(e, session_id=chat)), flush=True)
now = lambda: int(time.time() * 1000)
delta = lambda t: out({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": t}]}, "timestamp_ms": now()})
out({"type": "system", "subtype": "init", "apiKeySource": "login", "cwd": os.getcwd(), "model": "Composer 2", "permissionMode": "default"})
out({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}})
if "[shell]" in prompt:
    out({"type": "tool_call", "subtype": "started", "call_id": "c1", "tool_call": {"shellToolCall": {"args": {"command": "id"}}}})
    time.sleep(30)
if "[limit]" in prompt:
    print("Error: [resource_exhausted] You've hit your usage limit", file=sys.stderr)
    sys.exit(1)
if "[lost]" in prompt:
    delta("Partial")
    os.kill(os.getpid(), signal.SIGKILL)
if "[crash]" in prompt:
    delta("Partial")
    print("Error: socket hang up", file=sys.stderr)
    sys.exit(1)
if "[stop]" in prompt:
    def cancel(*_):
        print("\nAborting operation...\nOperation cancelled", file=sys.stderr)
        sys.exit(130)
    signal.signal(signal.SIGINT, cancel)
    delta("Working")
    while True:
        time.sleep(1)
delta("Hel")
delta("lo")
out({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "Hello"}]}, "model_call_id": "m1", "timestamp_ms": now()})
out({"type": "tool_call", "subtype": "started", "call_id": "c2", "model_call_id": "m1", "timestamp_ms": now(),
     "tool_call": {"mcpToolCall": {"args": {"name": "elsebot-find_tool", "providerIdentifier": "elsebot", "toolName": "find_tool", "args": {}}}}})
out({"type": "tool_call", "subtype": "completed", "call_id": "c2", "model_call_id": "m1", "timestamp_ms": now(),
     "tool_call": {"mcpToolCall": {"args": {"providerIdentifier": "elsebot", "toolName": "find_tool"}, "result": {"success": {"content": [{"text": {"text": "{}"}}], "isError": False}}}}})
delta(" there")
out({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": " there"}]}})
out({"type": "result", "subtype": "success", "is_error": False, "duration_ms": 5, "result": "Hello there",
     "usage": {"inputTokens": 100, "outputTokens": 20, "cacheReadTokens": 400, "cacheWriteTokens": 10}})
if "[unsaved]" in prompt:
    print("Error: database is locked", file=sys.stderr)
    sys.exit(1)
"#;

/// A host with the fake CLI and a host login file, both in a temporary
/// directory.
struct Server {
    dir: tempfile::TempDir,
    cursor: Cursor,
}

impl Server {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("bin/cursor-agent");
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::write(&program, FAKE_CURSOR).unwrap();
        let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
        std::fs::set_permissions(&program, mode).unwrap();
        let login = dir.path().join("host/.config/cursor/auth.json");
        std::fs::create_dir_all(login.parent().unwrap()).unwrap();
        std::fs::write(&login, r#"{"accessToken":"a.eyJzdWIiOiJ1c2VyXzEifQ.c"}"#).unwrap();
        let host = Host::new(&dir.path().join("data"), &dir.path().join("bots"));
        let login = Login {
            macos: false,
            store: None,
            file: Some(login),
        };
        let cursor = Cursor::detect(Some(host), Some(program), login)
            .with_tools("http://127.0.0.1:9/mcp".into());
        Self { dir, cursor }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn launches(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("bin/launches.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    async fn run(&self, input: TurnInput, sink: &Sink) -> TurnOutcome {
        self.cursor.run(input, sink, Stop::default()).await
    }
}

fn input(text: &str, origin: Origin) -> TurnInput {
    let mut input = crate::providers::cli::session::tests::input();
    input.text = text.into();
    input.delegation.origin = origin;
    input.tool_grant = Some("grant-1".into());
    input.profile = Some(Profile {
        model: MODEL.into(),
        reasoning_effort: "default".into(),
        service_tier: "default".into(),
    });
    input
}

#[derive(Default)]
struct Sink {
    sessions: Mutex<Vec<SessionState>>,
    text: Mutex<String>,
    usage: Mutex<Vec<crate::app::providers::Usage>>,
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
    async fn usage(&self, usage: crate::app::providers::Usage) {
        self.usage.lock().unwrap().push(usage);
    }
    async fn steers(&self) -> Vec<Steer> {
        Vec::new()
    }
    async fn steer_sending(&self, _: Uuid) -> Result<bool, Fenced> {
        Ok(true)
    }
    async fn steer_refused(&self, _: Uuid) {}
    async fn retire_session(&self) {
        self.retired
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    async fn steered(&self, _: Uuid) -> Result<(), Fenced> {
        Ok(())
    }
}

impl Sink {
    fn last_session(&self) -> SessionState {
        self.sessions.lock().unwrap().last().cloned().unwrap()
    }
    fn retired(&self) -> bool {
        self.retired.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn has(args: &Value, flag: &str) -> bool {
    args.as_array().unwrap().iter().any(|a| a == flag)
}

#[tokio::test]
async fn an_owner_turn_creates_a_chat_and_the_next_turn_resumes_it() {
    let server = Server::new();
    let sink = Sink::default();
    let outcome = server.run(input("hi", Origin::OwnerRequest), &sink).await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(*sink.text.lock().unwrap(), "Hello there", "each delta once");
    assert_eq!(
        *sink.usage.lock().unwrap(),
        [crate::app::providers::Usage {
            input: 510,
            cached_input: 400,
            cache_write: 10,
            output: 20,
        }]
    );
    let first = sink.last_session();

    let mut next = input("--again", Origin::OwnerRequest);
    next.session = Some(first.clone());
    next.turn_no = 4;
    let sink = Sink::default();
    assert_eq!(server.run(next, &sink).await, TurnOutcome::Completed);
    assert_eq!(sink.last_session().native_id, first.native_id);

    let [created, resumed] = &server.launches()[..] else {
        panic!("two launches: {:?}", server.launches());
    };
    assert_eq!(created["chat"], first.native_id.as_str());
    assert_eq!(created["resumed"], false);
    assert_eq!(resumed["chat"], first.native_id.as_str());
    assert_eq!(resumed["resumed"], true);
    for launch in [created, resumed] {
        assert!(
            has(&launch["args"], "--force"),
            "owner turns run host tools"
        );
        assert!(!has(&launch["args"], "--approve-mcps"));
        assert!(has(&launch["args"], "--disable-project-configs"));
        assert_eq!(launch["direnv"], "1");
        assert_eq!(launch["grant"], "grant-1");
        let home = server
            .path("data/bots")
            .join(Uuid::nil().to_string())
            .join("cursor-home");
        assert_eq!(launch["home"], home.to_str().unwrap());
        assert_eq!(
            launch["mcp"],
            json!({"mcpServers": {"elsebot": {"url": "http://127.0.0.1:9/mcp",
                "headers": {"Authorization": "Bearer ${ELSE_TASK_GRANT}"}}}}),
            "the grant stays out of the file"
        );
        assert_eq!(launch["config"]["permissions"]["deny"], json!([]));
    }
    let allowed = |launch: &Value| {
        let args = launch["args"].as_array().unwrap();
        let at = args.iter().position(|a| a == "--allowed-tools").unwrap();
        args[at + 1].clone()
    };
    assert_eq!(
        allowed(created),
        "update_todos_tool_call,read_todos_tool_call,shell_tool_call,read_tool_call,\
         edit_tool_call,delete_tool_call,grep_tool_call,glob_tool_call,ls_tool_call,\
         sem_search_tool_call,read_lints_tool_call,mcp_tool_call,get_mcp_tools_tool_call"
    );
    let prompt = |launch: &Value| launch["prompt"].as_str().unwrap().to_owned();
    assert!(prompt(created).starts_with("[else.bot instructions]"));
    assert!(
        !prompt(resumed).contains("[else.bot instructions]"),
        "the chat already holds them"
    );
    assert!(
        prompt(resumed).contains("--again"),
        "a prompt starting with dashes is not an option"
    );
}

#[tokio::test]
async fn a_result_the_cli_did_not_store_completes_without_resuming() {
    let server = Server::new();
    let sink = Sink::default();
    let outcome = server
        .run(input("[unsaved]", Origin::OwnerRequest), &sink)
        .await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert!(sink.retired(), "the next turn rebuilds the chat");

    let sink = Sink::default();
    let outcome = server.run(input("hi", Origin::OwnerRequest), &sink).await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert!(!sink.retired());
}

#[tokio::test]
async fn a_lost_chat_or_changed_profile_starts_a_new_chat() {
    let server = Server::new();
    let sink = Sink::default();
    server.run(input("hi", Origin::OwnerRequest), &sink).await;
    let first = sink.last_session();

    let mut changed = input("again", Origin::OwnerRequest);
    changed.session = Some(first.clone());
    changed.profile.as_mut().unwrap().model = "composer-2.5".into();
    let sink = Sink::default();
    assert_eq!(server.run(changed, &sink).await, TurnOutcome::Completed);
    assert_ne!(sink.last_session().native_id, first.native_id);

    std::fs::remove_dir_all(
        server
            .path("data/bots")
            .join(Uuid::nil().to_string())
            .join("cursor-home/.cursor/chats"),
    )
    .unwrap();
    let mut lost = input("again", Origin::OwnerRequest);
    lost.session = Some(first.clone());
    let sink = Sink::default();
    assert_eq!(server.run(lost, &sink).await, TurnOutcome::Completed);
    assert_ne!(sink.last_session().native_id, first.native_id);
    assert!(server.launches().iter().all(|l| l["resumed"] == false));
}

#[tokio::test]
async fn an_event_turn_offers_only_elsebot_tools_and_a_shell_call_fails_it() {
    let server = Server::new();
    let sink = Sink::default();
    let outcome = server
        .run(input("[shell]", Origin::ConnectorEvent), &sink)
        .await;
    assert_eq!(
        outcome,
        TurnOutcome::Failed {
            code: "policy_violation".into(),
            message: "Cursor called a tool this turn does not allow: shellToolCall".into(),
        }
    );
    assert!(sink.retired(), "the chat may hold the refused call");
    let [launch] = &server.launches()[..] else {
        panic!("one launch");
    };
    assert!(!has(&launch["args"], "--force"));
    assert!(has(
        &launch["args"],
        "update_todos_tool_call,read_todos_tool_call,mcp_tool_call,get_mcp_tools_tool_call"
    ));
    assert_eq!(
        launch["config"]["permissions"],
        json!({"allow": ["Mcp(elsebot:*)"], "deny": ["Shell(*)", "Read(**)", "Write(**)"]})
    );

    let sink = Sink::default();
    let outcome = server.run(input("hi", Origin::ConnectorEvent), &sink).await;
    assert_eq!(outcome, TurnOutcome::Completed, "else.bot tools still run");
}

#[tokio::test]
async fn stop_interrupts_the_running_cli() {
    let server = Server::new();
    let sink = Sink::default();
    let stop = Stop::default();
    let requester = {
        let stop = stop.clone();
        let sink = &sink;
        async move {
            tokio::time::timeout(Duration::from_secs(60), async {
                while sink.text.lock().unwrap().is_empty() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the CLI started replying");
            stop.request();
        }
    };
    let (outcome, ()) = tokio::join!(
        server
            .cursor
            .run(input("[stop]", Origin::OwnerRequest), &sink, stop.clone()),
        requester
    );
    assert_eq!(outcome, TurnOutcome::Interrupted);
    assert!(sink.retired(), "an interrupted chat is rebuilt from SQLite");
}

#[tokio::test]
async fn a_usage_limit_makes_cursor_unavailable_for_the_turn() {
    let server = Server::new();
    let outcome = server
        .run(input("[limit]", Origin::OwnerRequest), &Sink::default())
        .await;
    assert_eq!(
        outcome,
        TurnOutcome::Failed {
            code: "provider_unavailable".into(),
            message: "Cursor's usage limit is reached: Error: [resource_exhausted] You've hit your usage limit".into(),
        }
    );
}

#[tokio::test]
async fn a_lost_stream_ends_uncertain_and_retires_the_chat() {
    let server = Server::new();
    let sink = Sink::default();
    let outcome = server
        .run(input("[lost]", Origin::OwnerRequest), &sink)
        .await;
    assert_eq!(
        outcome,
        TurnOutcome::Uncertain {
            message: "Cursor CLI ended without a result (signal 9).".into()
        }
    );
    assert!(sink.retired());
}

#[tokio::test]
async fn a_cli_that_fails_after_taking_the_prompt_ends_uncertain() {
    let server = Server::new();
    let sink = Sink::default();
    let outcome = server
        .run(input("[crash]", Origin::OwnerRequest), &sink)
        .await;
    assert_eq!(
        outcome,
        TurnOutcome::Uncertain {
            message: "Cursor CLI ended without a result (exit 1). Error: socket hang up".into()
        }
    );
    assert!(sink.retired());
}

#[tokio::test]
async fn a_signed_out_host_says_how_to_sign_in() {
    let server = Server::new();
    std::fs::write(server.path("bin/refuse-login"), "").unwrap();
    let outcome = server
        .run(input("hi", Origin::OwnerRequest), &Sink::default())
        .await;
    let signed_out = TurnOutcome::Failed {
        code: "provider_unavailable".into(),
        message: cursor_home::SIGNED_OUT.into(),
    };
    assert_eq!(outcome, signed_out, "a login the CLI refuses");
    assert_eq!(
        server.cursor.list_models().await,
        Err(cursor_home::SIGNED_OUT.into())
    );

    std::fs::remove_file(server.path("host/.config/cursor/auth.json")).unwrap();
    assert_eq!(
        server.cursor.availability(),
        Availability::Unavailable(cursor_home::SIGNED_OUT.into())
    );
    let outcome = server
        .run(input("hi", Origin::OwnerRequest), &Sink::default())
        .await;
    assert_eq!(outcome, signed_out);
    assert_eq!(server.launches().len(), 0, "nothing ran");
}

#[tokio::test]
async fn the_cli_lists_the_accounts_models() {
    let server = Server::new();
    assert_eq!(
        server.cursor.list_models().await,
        Ok(vec![Listed::id("composer-2.5"), Listed::id("gpt-5.5-high")])
    );
}

#[test]
fn a_missing_or_old_cli_names_its_fix() {
    let dir = tempfile::tempdir().unwrap();
    let host = Host::new(&dir.path().join("data"), &dir.path().join("bots"));
    let login = Login {
        macos: false,
        store: None,
        file: None,
    };
    let missing = Cursor::detect(Some(host.clone()), None, login.clone());
    assert_eq!(
        missing.availability(),
        Availability::Unavailable(cursor_home::NOT_INSTALLED.into())
    );
    let old = dir.path().join("cursor-agent");
    std::fs::write(&old, "#!/bin/sh\necho 2026.07.30-abc\n").unwrap();
    let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&old, mode).unwrap();
    assert_eq!(
        Cursor::detect(Some(host), Some(old), login).availability(),
        Availability::Unavailable(
            "Cursor CLI 2026.07.30-abc is older than 2026.08.11. Run `cursor-agent update` on this server."
                .into()
        )
    );
}

#[test]
fn the_installers_link_is_used_and_kept_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let version = dir
        .path()
        .join(".local/share/cursor-agent/versions/2026.08.11-x/cursor-agent");
    std::fs::create_dir_all(version.parent().unwrap()).unwrap();
    std::fs::write(&version, "#!/usr/bin/env bash\n").unwrap();
    let mode = std::os::unix::fs::PermissionsExt::from_mode(0o755);
    std::fs::set_permissions(&version, mode).unwrap();
    let link = dir.path().join(cursor_home::INSTALLED);
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&version, &link).unwrap();
    let home = Some(dir.path());
    assert_eq!(lookup(None, home), Ok(Some(link.clone())));
    assert_eq!(lookup(Some(&link), None), Ok(Some(link)));
    let missing = dir.path().join("missing");
    let err = lookup(Some(&missing), home).unwrap_err();
    assert!(err.contains("ELSE_CURSOR_BIN"), "{err}");
}

#[tokio::test]
async fn hooks_in_the_bots_project_refuse_the_turn() {
    let server = Server::new();
    let project = server.path("bots").join(Uuid::nil().to_string());
    std::fs::create_dir_all(project.join(".claude")).unwrap();
    std::fs::write(
        project.join(".claude/settings.json"),
        r#"{"permissions":{}}"#,
    )
    .unwrap();
    let outcome = server
        .run(input("hi", Origin::OwnerRequest), &Sink::default())
        .await;
    assert_eq!(
        outcome,
        TurnOutcome::Completed,
        "settings without hooks are fine"
    );

    std::fs::create_dir_all(project.join(".cursor")).unwrap();
    std::fs::write(
        project.join(".cursor/hooks.json"),
        r#"{"version":1,"hooks":{"sessionStart":[{"command":"touch /tmp/x"}]}}"#,
    )
    .unwrap();
    let outcome = server
        .run(input("hi", Origin::OwnerRequest), &Sink::default())
        .await;
    let TurnOutcome::Failed { code, message } = outcome else {
        panic!("refused: {outcome:?}");
    };
    assert_eq!(code, "provider_refused");
    assert!(message.contains(".cursor/hooks.json"), "{message}");
    assert_eq!(
        server.launches().len(),
        1,
        "the refused turn never launched"
    );
}

#[test]
fn the_host_login_is_found_where_the_cli_keeps_it() {
    let path = |p: &str| Some(PathBuf::from(p));
    assert_eq!(
        cursor_home::host_login(false, Some("/h"), None, None),
        path("/h/.config/cursor/auth.json")
    );
    assert_eq!(
        cursor_home::host_login(false, Some("/h"), Some("/x"), None),
        path("/x/cursor/auth.json")
    );
    assert_eq!(
        cursor_home::host_login(true, Some("/h"), None, None),
        None,
        "keychain"
    );
    assert_eq!(
        cursor_home::host_login(true, Some("/h"), None, Some("file")),
        path("/h/.cursor/auth.json")
    );
}

#[test]
fn models_are_read_from_the_listing() {
    let printed = "Available models\n\nauto - Auto (current)\ncomposer-2.5 - Composer 2.5 (default)\n\nTip: use --model <id>";
    assert_eq!(
        listed_models(printed),
        vec![Listed::id("auto"), Listed::id("composer-2.5")]
    );
    assert_eq!(
        listed_models("No models available for this account."),
        vec![]
    );
}

fn tracker(host: bool) -> Tracker {
    let mut t = Tracker::new(MODEL, "chat", host, true);
    t.on(&json!({"type":"system","subtype":"init","session_id":"chat","model":"Composer 2"}))
        .unwrap();
    t.on(&json!({"type":"user","session_id":"chat","message":{"content":[]}}))
        .unwrap();
    t
}

fn said(text: &str, extra: Value) -> Value {
    let mut event = json!({"type":"assistant","session_id":"chat",
        "message":{"role":"assistant","content":[{"type":"text","text":text}]}});
    for (key, value) in extra.as_object().unwrap() {
        event[key] = value.clone();
    }
    event
}

#[test]
fn a_delta_that_repeats_the_text_so_far_is_held_until_the_next_event() {
    let ts = json!({"timestamp_ms": 1});
    let mut t = tracker(true);
    assert_eq!(
        t.on(&said("ab", ts.clone())),
        Ok(vec![Event::Text("ab".into())])
    );
    assert_eq!(
        t.on(&said("ab", ts.clone())),
        Ok(vec![]),
        "a flush or a delta"
    );
    assert_eq!(
        t.on(&said("c", ts.clone())),
        Ok(vec![Event::Text("ab".into()), Event::Text("c".into())]),
        "a delta"
    );
    assert_eq!(t.on(&said("ababc", ts.clone())), Ok(vec![]));
    assert_eq!(
        t.on(&json!({"type":"retry","subtype":"starting","session_id":"chat"})),
        Ok(vec![Event::EndMessage]),
        "a flush before a retry"
    );
    assert_eq!(
        t.on(&said("d", ts.clone())),
        Ok(vec![Event::Text("d".into())])
    );
    assert_eq!(
        t.on(&said("d", json!({}))),
        Ok(vec![Event::EndMessage]),
        "a flush without a timestamp"
    );
    t.on(&said("e", ts.clone())).unwrap();
    assert_eq!(t.on(&said("e", ts.clone())), Ok(vec![]));
    assert_eq!(
        t.on(&json!({"type":"interaction_query","session_id":"chat"})),
        Ok(vec![Event::EndMessage]),
        "a flush before a question"
    );
}

#[test]
fn a_tool_call_outside_the_turns_policy_violates_it() {
    let call = |kind: &str, args: Value| {
        json!({"type":"tool_call","subtype":"started","call_id":"c","session_id":"chat",
            "tool_call": {kind: {"args": args}}})
    };
    let mcp = |server: &str, tool: &str| {
        call(
            "mcpToolCall",
            json!({"providerIdentifier": server, "toolName": tool}),
        )
    };
    assert_eq!(tracker(false).on(&mcp("elsebot", "find_tool")), Ok(vec![]));
    assert_eq!(
        tracker(false).on(&call("updateTodosToolCall", json!({}))),
        Ok(vec![])
    );
    assert_eq!(
        tracker(true).on(&call("shellToolCall", json!({}))),
        Ok(vec![])
    );
    assert_eq!(
        tracker(false).on(&call("getMcpToolsToolCall", json!({"server": "elsebot"}))),
        Ok(vec![])
    );
    for (host, event) in [
        (
            false,
            call("getMcpToolsToolCall", json!({"server": "github"})),
        ),
        (false, call("readToolCall", json!({}))),
        (true, call("taskToolCall", json!({}))),
        (true, call("webSearchToolCall", json!({}))),
        (true, mcp("github", "find_tool")),
        (true, mcp("elsebot", "no_such_tool")),
        (
            true,
            json!({"type":"tool_call","subtype":"started","call_id":"c","session_id":"chat","tool_call":{}}),
        ),
    ] {
        let violation = tracker(host).on(&event).unwrap_err();
        assert_eq!(violation.code, "policy_violation", "{event}");
    }
    let mut early = Tracker::new(MODEL, "chat", true, true);
    assert!(
        early.on(&call("shellToolCall", json!({}))).is_err(),
        "before the chat opened"
    );
    let mut other = tracker(true);
    assert!(
        other
            .on(&said("x", json!({"session_id": "another"})))
            .is_err()
    );
}

#[test]
fn a_parked_elsebot_result_parks_the_turn() {
    let mut t = tracker(false);
    let tool = crate::app::tools::PARKING_TOOLS[0];
    t.on(
        &json!({"type":"tool_call","subtype":"started","call_id":"c","session_id":"chat",
        "tool_call":{"mcpToolCall":{"args":{"providerIdentifier":"elsebot","toolName":tool}}}}),
    )
    .unwrap();
    let completed = |payload: &str| {
        json!({"type":"tool_call","subtype":"completed","call_id":"c","session_id":"chat",
            "tool_call":{"mcpToolCall":{"args":{},"result":{"success":{"content":[{"text":{"text":payload}}]}}}}})
    };
    assert_eq!(t.on(&completed(r#"{"status":"done"}"#)), Ok(vec![]));
    assert_eq!(
        t.on(&completed(r#"{"status":"pending","parked":true}"#)),
        Ok(vec![Event::Parked])
    );
}

#[test]
fn an_exit_after_a_stop_interrupts_and_other_exits_name_their_cause() {
    let t = tracker(true);
    let exit = |code: Option<i32>, stderr: &str| Terminal::Exit {
        code,
        signal: None,
        stderr: stderr.into(),
    };
    assert_eq!(
        t.outcome(exit(Some(130), ""), Some(StopCause::Owner), true),
        TurnOutcome::Interrupted
    );
    assert!(
        matches!(
            t.outcome(exit(Some(1), "Error: x"), Some(StopCause::Owner), true),
            TurnOutcome::Uncertain { .. }
        ),
        "a failure the stop did not cause"
    );
    assert!(matches!(
        t.outcome(exit(Some(1), "Error: x"), None, true),
        TurnOutcome::Uncertain { .. }
    ));
    assert_eq!(
        t.outcome(
            exit(Some(1), "Error: Cannot use this model: x"),
            None,
            false
        ),
        TurnOutcome::Failed {
            code: "provider_failed".into(),
            message: "Cursor CLI exited with 1: Error: Cannot use this model: x".into(),
        }
    );
    assert!(matches!(
        t.outcome(exit(Some(0), ""), None, true),
        TurnOutcome::Uncertain { .. }
    ));
    let result = |success| Terminal::Result {
        success,
        text: String::new(),
    };
    assert!(matches!(
        t.outcome(result(true), None, false),
        TurnOutcome::Uncertain { .. }
    ));
    let ended = |exit_code: Value| {
        let mut t = tracker(true);
        t.on(
            &json!({"type":"result","subtype":"success","is_error":false,
            "session_id":"chat","exit_code": exit_code}),
        )
        .unwrap();
        t
    };
    assert!(ended(json!(0)).resumable(&TurnOutcome::Completed));
    assert!(!ended(json!(1)).resumable(&TurnOutcome::Completed));
    assert!(!ended(Value::Null).resumable(&TurnOutcome::Completed));
    assert!(!ended(json!(0)).resumable(&TurnOutcome::Interrupted));
}
