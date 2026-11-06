use serde_json::Value;

use super::{AppState, Grant, Outcome, slice, summarize_input, tool_error};
use crate::app::questions::Kind;

pub(super) const MAX_PURPOSE: usize = 300;
const MAX_PREVIEW: usize = 2000;

pub(super) const EFFECT_DESCRIPTION: &str = "What this does: local (reads, or changes only inside your \
    computer or project directory that are easy to undo); destructive (deletes, overwrites, \
    or changes something hard to undo, here or on a remote service: deleting a remote git \
    branch, repository, file, or record is destructive, not external); external (sends a \
    message or email, posts, edits, or publishes anything others can see, pays, or otherwise \
    reaches people or services outside). Required; a missing effect is refused, never assumed local.";

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
        "computer_browser" => matches!(args["action"].as_str(), Some("click") | Some("type")),
        _ => false,
    }
}

fn declared_effect(name: &str, args: &Value) -> Result<Option<Effect>, String> {
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
    Ok(Some(effect))
}

fn approval_card(name: &str, args: &Value) -> (String, String) {
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
    if !text("context").trim().is_empty() {
        return (title, text("context").to_owned());
    }
    let context = match name {
        "mcp_request" => format!(
            "MCP {}: {}\n{}",
            text("id"),
            text("method"),
            slice(&args["params"].to_string(), 0, MAX_PREVIEW).1
        ),
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
            let selector = slice(text("selector"), 0, 300).1;
            if action == "type" {
                format!(
                    "Browser: type {} characters into {selector}",
                    text("text").chars().count()
                )
            } else {
                format!("Browser: {action} {selector}")
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
    use crate::app::questions::{Spec, Standing};
    use crate::db::owner_questions::{self, Gate};
    let (title, context) = approval_card(name, args);
    let spec = Spec::tool_approval(kind, name, args, title, context);
    match owner_questions::gate(&state.db, grant.turn_id, &spec).await {
        Ok(Gate::Run {
            standing: Some(Standing::Bot),
        }) => Ok(Some("standing destructive permission")),
        Ok(Gate::Run {
            standing: Some(Standing::Routine),
        }) => Ok(Some("standing destructive permission for this routine")),
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
                "NOT_ALLOWED: a delegated task cannot ask the owner, so this {} action cannot \
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

    #[test]
    fn approval_cards_name_the_action_but_never_typed_text() {
        let (title, context) = approval_card(
            "computer_run",
            &json!({ "command": "./send.sh", "effect": "external", "secrets": ["smtp"], "purpose": "Send the newsletter" }),
        );
        assert_eq!(title, "Send the newsletter");
        assert_eq!(context, "./send.sh\n(uses the secrets: smtp)");
        let (title, context) = approval_card(
            "computer_input",
            &json!({ "frame": [1, 1], "effect": "external",
                     "actions": [{ "click": [1, 2] }, { "type": "hunter2" }] }),
        );
        assert_eq!(title, "Use the bot's desktop");
        assert!(
            context.contains("type(7 chars)") && !context.contains("hunter2"),
            "{context}"
        );
        let (_, context) = approval_card(
            "computer_browser",
            &json!({ "action": "type", "selector": "#msg", "text": "secret words", "effect": "external" }),
        );
        assert_eq!(context, "Browser: type 12 characters into #msg");
    }
}
