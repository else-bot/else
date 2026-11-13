use serde_json::Value;

use super::{AppState, Grant, Outcome, slice, summarize_input, tool_error};
use crate::app::questions::{Kind, TaskScope};
use crate::computers::browser_refs::RefLabel;

pub(super) const MAX_PURPOSE: usize = 300;
const MAX_PREVIEW: usize = 2000;

/// The advertised `effect` description. The instructions define the three
/// effects; the full definition comes back only with an EFFECT_REQUIRED error.
pub(super) const EFFECT_ADVERTISED: &str = "local, destructive, or external, as your \
    instructions define them. Required; a missing effect is refused, never assumed local.";
pub(super) const EFFECT_DESCRIPTION: &str = "What this does: local (reads and routine reversible work, \
    including creating or updating tickets, notes, and documents locally or in connected services); destructive (deletes, overwrites, \
    or changes something hard to undo, here or on a remote service: deleting a remote git \
    branch, repository, file, or record is destructive, not external); external (sends a \
    message or email, publishes publicly, pays, changes access or security, or makes another \
    sensitive commitment). Routine work requested by the owner needs no approval merely because \
    it uses a remote service. Required; a missing effect is refused, never assumed local.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effect {
    Local,
    Destructive,
    External,
}

impl Effect {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "local" => Some(Self::Local),
            "destructive" => Some(Self::Destructive),
            "external" => Some(Self::External),
            _ => None,
        }
    }

    fn approval(self) -> Option<Kind> {
        match self {
            Self::Local => None,
            Self::Destructive => Some(Kind::Destructive),
            Self::External => Some(Kind::External),
        }
    }
}

fn needs_effect(name: &str, args: &Value) -> bool {
    match name {
        "computer_run" | "computer_job_start" | "computer_input" | "computer_write_file" => true,
        "computer_browser" => {
            matches!(args["action"].as_str(), Some("click") | Some("type"))
                || args["dialog"] == "accept"
        }
        _ => false,
    }
}

/// The computer tools whose schema declares `effect`, all of them parking
/// tools. Any other tool given an effect is refused before approval: its
/// card would park the turn under a name no provider treats as parking.
fn declares_effect(name: &str) -> bool {
    matches!(
        name,
        "computer_run"
            | "computer_job_start"
            | "computer_input"
            | "computer_write_file"
            | "computer_browser"
    )
}

fn declared_effect(name: &str, args: &Value) -> Result<Option<Effect>, String> {
    if !declares_effect(name) && args.get("effect").is_some_and(|e| !e.is_null()) {
        return Err(format!("{name} takes no effect. Nothing was done."));
    }
    let needs = needs_effect(name, args);
    let effect = match args.get("effect") {
        None | Some(Value::Null) if needs => {
            return Err(format!(
                "EFFECT_REQUIRED: {name} needs effect. {EFFECT_DESCRIPTION} Nothing was done."
            ));
        }
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(text)) => Effect::parse(text).ok_or_else(|| {
            "effect is local, destructive, or external. Nothing was done.".to_owned()
        })?,
        Some(_) => {
            return Err("effect is local, destructive, or external. Nothing was done.".into());
        }
    };
    match args.get("purpose") {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) if text.len() <= MAX_PURPOSE && !text.contains('\0') => {}
        Some(_) => return Err(format!("purpose is at most {MAX_PURPOSE} bytes of text.")),
    }
    match args.get("context") {
        None | Some(Value::Null) => {}
        Some(Value::String(text))
            if text.len() <= crate::app::questions::MAX_CONTEXT && !text.contains('\0') => {}
        Some(_) => return Err("context is at most 4000 bytes of text.".into()),
    }
    TaskScope::from_args(args).map_err(|message| format!("{message}. Nothing was done."))?;
    Ok(Some(effect))
}

/// Page text for a card: control and invisible formatting characters
/// removed, cut to `max` characters.
fn page_text(text: &str, max: usize) -> String {
    let invisible = |c: &char| {
        c.is_control()
            || matches!(
                c,
                '\u{200b}'..='\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{feff}'
            )
    };
    let mut kept: String = text.chars().filter(|c| !invisible(c)).collect();
    if let Some((cut, _)) = kept.char_indices().nth(max) {
        kept.truncate(cut);
        kept.push('…');
    }
    kept
}

/// The control a snapshot ref names, as the page labels it: `button
/// "Delete project" (@kqvzb5)`. A selector the API has no label for
/// is shown as given.
fn browser_target(selector: &str, label: Option<&RefLabel>) -> String {
    let selector = slice(selector, 0, 300).1;
    let Some(label) = label else {
        return selector.to_owned();
    };
    let role: String = label
        .role
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(30)
        .collect();
    let name = serde_json::to_string(&page_text(&label.name, 80)).unwrap_or_default();
    match &label.url {
        Some(url) => format!("{role} {name} to {} ({selector})", page_text(url, 120)),
        None => format!("{role} {name} ({selector})"),
    }
}

fn approval_card(name: &str, args: &Value, label: Option<&RefLabel>) -> (String, String) {
    let text = |key: &str| args[key].as_str().unwrap_or_default();
    let purpose = text("purpose").trim();
    let default = match name {
        "mcp_request" => "Use a shared MCP",
        "computer_run" => "Run a command in the bot's computer",
        "computer_job_start" => "Start a background job in the bot's computer",
        "computer_input" => "Use the bot's desktop",
        "computer_write_file" => "Write a file in the bot's computer",
        _ => "Act in the bot's browser",
    };
    let title = if purpose.is_empty() {
        default.to_owned()
    } else {
        purpose.to_owned()
    };
    if name == "mcp_request" {
        let header = format!(
            "MCP: {} / {} ({})\n",
            text("id"),
            args["params"]["name"].as_str().unwrap_or_default(),
            text("effect")
        );
        let body = if text("context").trim().is_empty() {
            args["params"].to_string()
        } else {
            text("context").to_owned()
        };
        let available = crate::app::questions::MAX_CONTEXT.saturating_sub(header.len());
        return (title, format!("{header}{}", slice(&body, 0, available).1));
    }
    if !text("context").trim().is_empty() && name != "computer_browser" {
        return (title, text("context").to_owned());
    }
    let context = match name {
        "computer_run" | "computer_job_start" => {
            let mut preview = slice(text("command"), 0, MAX_PREVIEW).1.to_owned();
            if preview.len() < text("command").len() {
                preview.push('…');
            }
            if let Some(secrets) = args["secrets"].as_array().filter(|s| !s.is_empty()) {
                let names: Vec<&str> = secrets.iter().filter_map(Value::as_str).collect();
                preview.push_str(&format!("\n(uses the secrets: {})", names.join(", ")));
            }
            preview
        }
        "computer_write_file" => format!(
            "Write {} bytes to {}",
            text("content").len(),
            slice(text("path"), 0, 1024).1
        ),
        "computer_input" => format!(
            "Desktop input: {}",
            summarize_input(
                args["actions"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
            )
        ),
        _ => {
            let action = text("action");
            let selector = browser_target(text("selector"), label);
            let step = match action {
                "type" => format!(
                    "Browser: type {} characters into {selector}",
                    text("text").chars().count()
                ),
                "open" => format!("Browser: open {}", slice(text("url"), 0, 300).1),
                _ => format!("Browser: {action} {selector}"),
            };
            let step = if args["dialog"] == "accept" {
                format!("{step}, accepting any dialog it opens")
            } else {
                step
            };
            // First, so a long description from the bot cannot push it
            // out of the card's collapsed preview.
            let given = text("context").trim();
            if given.is_empty() {
                step
            } else {
                let room = crate::app::questions::MAX_CONTEXT.saturating_sub(step.len() + 2);
                format!("{step}\n\n{}", slice(given, 0, room).1)
            }
        }
    };
    (title, context)
}

pub(crate) async fn approve(
    state: &AppState,
    grant: &Grant,
    name: &str,
    args: &Value,
    kind: Kind,
) -> Result<Option<&'static str>, Outcome> {
    let label = match (name, args["selector"].as_str(), &state.computers) {
        ("computer_browser", Some(selector), Some(computers)) => {
            computers.browser_ref(grant.bot_id, selector).await
        }
        _ => None,
    };
    let (title, context) = approval_card(name, args, label.as_ref());
    approve_card(state, grant, name, args, kind, title, context).await
}

/// Like [`approve`], with a card the caller describes.
pub(crate) async fn approve_card(
    state: &AppState,
    grant: &Grant,
    name: &str,
    args: &Value,
    kind: Kind,
    title: String,
    context: String,
) -> Result<Option<&'static str>, Outcome> {
    use crate::app::questions::{Spec, Standing};
    use crate::db::owner_questions::{self, Gate};
    let task_scope = declares_effect(name)
        .then(|| TaskScope::from_args(args).ok().flatten())
        .flatten();
    let spec = Spec::tool_approval(kind, name, args, task_scope, title, context);
    match owner_questions::gate(&state.db, grant.turn_id, &spec).await {
        Ok(Gate::Run {
            standing: Some(Standing::Bot),
        }) => Ok(Some("standing destructive permission")),
        Ok(Gate::Run {
            standing: Some(Standing::Routine),
        }) => Ok(Some("standing destructive permission for this routine")),
        Ok(Gate::Run {
            standing: Some(Standing::Tool),
        }) => Ok(Some("standing permission for this MCP tool and effect")),
        Ok(Gate::Run {
            standing: Some(Standing::Task),
        }) => Ok(Some("standing permission for this task and target")),
        Ok(Gate::Run { standing: None }) => Ok(Some("approved by the owner")),
        Ok(Gate::Pending(question)) => Err(crate::http::owner_tools::pending_approval(
            state,
            grant,
            &question,
            &format!(
                "Nothing was done. The owner sees an approval card for exactly this call. \
                 Tell them briefly what it will do and why, then end this attempt now; do \
                 not wait, poll, or call more tools. If they approve, you continue \
                 automatically: call {name} again with exactly the same arguments, once. \
                 If they decline, do not do it or an equivalent."
            ),
        )
        .await),
        Ok(Gate::Denied(question)) => Err((
            tool_error(&format!(
                "DENIED: the owner declined this {} action for this request. Nothing was done. \
                 Do not run it or an equivalent; continue without it or ask what they want.",
                kind.as_str()
            )),
            Some(format!("denied {} by the owner", question.id)),
        )),
        Err(owner_questions::RequestError::NotAllowed) => Err((
            tool_error(&format!(
                "NOT_ALLOWED: this task cannot ask the owner, so this {} action cannot \
                 be approved here. Nothing was done. Report what needs approval in your result.",
                kind.as_str()
            )),
            Some("rejected NOT_ALLOWED".into()),
        )),
        Err(err) => Err(crate::http::owner_tools::question_refused(err)),
    }
}

/// Run a computer tool once its declared effect allows it: a local call
/// runs at once, a destructive or external one only with the owner's
/// approval for exactly this call. The approval is noted in the activity log.
pub async fn call(state: &AppState, grant: &Grant, name: &str, args: &Value) -> Outcome {
    let effect = match declared_effect(name, args) {
        Ok(effect) => effect,
        Err(message) => return (tool_error(&message), None),
    };
    let note = match (effect, args.get("approved_draft").filter(|v| !v.is_null())) {
        (Some(Effect::External), Some(_)) => {
            match crate::http::outgoing_tools::claim_for_tool(state, grant, name, args).await {
                Ok(note) => Some(note),
                Err(outcome) => return outcome,
            }
        }
        (_, Some(_)) => {
            return (
                tool_error(
                    "approved_draft goes only with effect external: it names the approved \
                     draft this call sends. Nothing was done.",
                ),
                None,
            );
        }
        (effect, None) => match effect.and_then(Effect::approval) {
            Some(kind) => match approve(state, grant, name, args, kind).await {
                Ok(note) => note.map(str::to_owned),
                Err(outcome) => return outcome,
            },
            None => None,
        },
    };
    let (result, log) = super::execute(state, grant, name, args).await;
    match note {
        Some(note) => (result, log.map(|log| format!("{log} [{note}]"))),
        None => (result, log),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_tool_without_an_effect_in_its_schema_refuses_one() {
        let read = json!({ "path": "notes.txt", "effect": "destructive" });
        assert!(declared_effect("computer_read_file", &read).is_err());
        assert!(declared_effect("computer_job_cancel", &json!({ "effect": "external" })).is_err());
        assert_eq!(
            declared_effect("computer_read_file", &json!({ "effect": null })),
            Ok(None)
        );
        let schemas = crate::http::guest_tools::tool_list();
        for tool in &schemas {
            let name = tool["name"].as_str().unwrap();
            let declared = tool["inputSchema"]["properties"].get("effect").is_some();
            assert_eq!(declares_effect(name), declared, "{name}");
        }
    }

    #[test]
    fn acting_tools_must_declare_an_effect_and_reads_need_none() {
        let run = json!({ "command": "ls" });
        assert!(
            declared_effect("computer_run", &run)
                .unwrap_err()
                .starts_with("EFFECT_REQUIRED")
        );
        assert!(declared_effect("computer_job_start", &run).is_err());
        assert!(
            declared_effect("computer_input", &json!({ "frame": [1, 1], "actions": [] })).is_err()
        );
        assert!(
            declared_effect(
                "computer_browser",
                &json!({ "action": "click", "selector": "a" })
            )
            .is_err()
        );
        assert_eq!(
            declared_effect(
                "computer_browser",
                &json!({ "action": "open", "url": "https://x" })
            )
            .unwrap(),
            None
        );
        assert!(
            declared_effect(
                "computer_browser",
                &json!({ "action": "open", "url": "https://x", "dialog": "accept" })
            )
            .is_err()
        );
        assert_eq!(
            declared_effect("computer_read_file", &json!({ "path": "a" })).unwrap(),
            None
        );
        assert_eq!(
            declared_effect(
                "computer_run",
                &json!({ "command": "ls", "effect": "local" })
            )
            .unwrap(),
            Some(Effect::Local)
        );
        assert_eq!(
            declared_effect(
                "computer_run",
                &json!({ "command": "rm -rf x", "effect": "destructive" })
            )
            .unwrap(),
            Some(Effect::Destructive)
        );
        assert!(
            declared_effect(
                "computer_run",
                &json!({ "command": "ls", "effect": "harmless" })
            )
            .is_err()
        );
        assert!(
            declared_effect(
                "computer_run",
                &json!({ "command": "ls", "effect": "local", "purpose": "x".repeat(301) })
            )
            .is_err()
        );
    }

    fn card(name: &str, args: &Value) -> (String, String) {
        approval_card(name, args, None)
    }

    fn label(role: &str, name: &str, url: Option<&str>) -> RefLabel {
        RefLabel {
            role: role.into(),
            name: name.into(),
            url: url.map(str::to_owned),
        }
    }

    #[test]
    fn a_browser_card_names_the_control_a_known_ref_points_at() {
        let click = json!({ "action": "click", "selector": "@kqvzb5", "effect": "destructive" });
        let delete = label("button", "Delete project", None);
        let (_, context) = approval_card("computer_browser", &click, Some(&delete));
        assert_eq!(
            context,
            "Browser: click button \"Delete project\" (@kqvzb5)"
        );

        let typing = json!({ "action": "type", "selector": "@kqvzb7", "text": "hunter2 here",
                             "effect": "external" });
        let email = label("textbox", "Email", None);
        let (_, context) = approval_card("computer_browser", &typing, Some(&email));
        assert_eq!(
            context,
            "Browser: type 12 characters into textbox \"Email\" (@kqvzb7)"
        );

        let docs = label("link", "Docs", Some("/docs"));
        let (_, context) = approval_card("computer_browser", &click, Some(&docs));
        assert_eq!(context, "Browser: click link \"Docs\" to /docs (@kqvzb5)");

        let (_, context) = card("computer_browser", &click);
        assert_eq!(
            context, "Browser: click @kqvzb5",
            "an unknown ref is shown as given"
        );
        let css = json!({ "action": "click", "selector": "text=Pay", "effect": "external" });
        assert_eq!(card("computer_browser", &css).1, "Browser: click text=Pay");
    }

    #[test]
    fn a_hostile_control_name_is_quoted_cut_and_stripped() {
        let click = json!({ "action": "click", "selector": "@kq1", "effect": "destructive",
                            "context": "Remove the old project." });
        let hostile = label(
            "button\n\"x",
            &format!("Cancel\" (@kq2)\n\u{202e}{}", "a".repeat(200)),
            None,
        );
        let (_, context) = approval_card("computer_browser", &click, Some(&hostile));
        let expected = format!(
            "Browser: click buttonx \"Cancel\\\" (@kq2){}…\" (@kq1)\n\nRemove the old project.",
            "a".repeat(66)
        );
        assert_eq!(context, expected);
    }

    #[test]
    fn approval_cards_name_the_action_but_never_typed_text() {
        let (title, context) = card(
            "computer_run",
            &json!({ "command": "./send.sh", "effect": "external", "secrets": ["smtp"], "purpose": "Send the newsletter" }),
        );
        assert_eq!(title, "Send the newsletter");
        assert_eq!(context, "./send.sh\n(uses the secrets: smtp)");
        let (title, context) = card(
            "computer_input",
            &json!({ "frame": [1, 1], "effect": "external",
                     "actions": [{ "click": [1, 2] }, { "type": "hunter2" }] }),
        );
        assert_eq!(title, "Use the bot's desktop");
        assert!(
            context.contains("type(7 chars)") && !context.contains("hunter2"),
            "{context}"
        );
        let (_, context) = card(
            "computer_browser",
            &json!({ "action": "type", "selector": "#msg", "text": "secret words", "effect": "external" }),
        );
        assert_eq!(context, "Browser: type 12 characters into #msg");
        let (_, context) = card(
            "computer_browser",
            &json!({ "action": "open", "url": "https://x.com/home", "dialog": "accept", "effect": "destructive" }),
        );
        assert_eq!(
            context,
            "Browser: open https://x.com/home, accepting any dialog it opens"
        );
    }
}
