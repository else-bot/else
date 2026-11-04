use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// One option of an owner question, by its stable id and shown label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choice {
    pub id: String,
    pub label: String,
}

/// The owner's answer to a question or approval request: which question
/// (its kind, title, and bound action) and the choice the owner made, or
/// that it ended unanswered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub id: Uuid,
    /// `choice`, `destructive`, or `external`.
    pub kind: Kind,
    pub title: String,
    /// For an approval: the tool call or described host action.
    pub action: Option<Action>,
    /// `answered`, `skipped`, or `cancelled`.
    pub status: Status,
    pub answer: Option<Choice>,
}

/// Questions per owner request (turn).
pub const MAX_PER_TURN: i64 = 8;
pub const MAX_TITLE: usize = 500;
pub const MAX_CONTEXT: usize = 4000;
pub const MAX_CHOICES: usize = 4;
pub const MIN_CHOICES: usize = 2;
pub const MAX_CHOICE_LABEL: usize = 120;

pub const APPROVE: &str = "approve";
pub const DENY: &str = "deny";
pub const APPROVE_FOREVER: &str = "approve_forever";
/// The one standing grant a bot can hold.
pub const DESTRUCTIVE_SCOPE: &str = "destructive";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Kind {
    Choice,
    Destructive,
    External,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Choice => "choice",
            Self::Destructive => "destructive",
            Self::External => "external",
        }
    }

    /// The effect a bot may declare for an approval: never `choice`.
    pub fn approval(text: &str) -> Option<Self> {
        match text {
            "destructive" => Some(Self::Destructive),
            "external" => Some(Self::External),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Status {
    Pending,
    Answered,
    Skipped,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Answered => "answered",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The options of an approval card. Only a destructive action offers a
/// standing grant.
pub fn approval_choices(kind: Kind) -> Vec<Choice> {
    let mut choices = vec![
        Choice {
            id: APPROVE.into(),
            label: "Approve once".into(),
        },
        Choice {
            id: DENY.into(),
            label: "Deny".into(),
        },
    ];
    if kind == Kind::Destructive {
        choices.push(Choice {
            id: APPROVE_FOREVER.into(),
            label: "Always allow destructive actions".into(),
        });
    }
    choices
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Action {
    Tool { tool: String, args: Value },
    Host { host: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub action: Action,
    pub hash: [u8; 32],
}

/// What the bot asks: a question with options, or an action to approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub kind: Kind,
    pub title: String,
    pub context: Option<String>,
    pub choices: Vec<Choice>,
    pub approval: Option<Approval>,
}

fn text_arg<'a>(args: &'a Value, key: &str, max: usize) -> Result<Option<&'a str>, ()> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() || text.len() > max || text.contains('\0') {
                Err(())
            } else {
                Ok(Some(text))
            }
        }
        Some(_) => Err(()),
    }
}

/// Keys of `args` outside `allowed` are refused (no value rides along).
fn only_keys(args: &Value, allowed: &[&str]) -> Result<(), ()> {
    let object = args.as_object().ok_or(())?;
    if object.keys().all(|k| allowed.contains(&k.as_str())) {
        Ok(())
    } else {
        Err(())
    }
}

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
        hasher.update(b"\0");
    }
    hasher.finalize().into()
}

impl Spec {
    /// Omit choices for a question answered through the chat composer.
    pub fn choice(args: &Value) -> Result<Self, &'static str> {
        only_keys(args, &["question", "choices", "context"])
            .map_err(|_| "use question, choices, and optionally context")?;
        let title = text_arg(args, "question", MAX_TITLE)
            .ok()
            .flatten()
            .ok_or("question is 1 to 500 bytes of text")?;
        let context = text_arg(args, "context", MAX_CONTEXT)
            .map_err(|_| "context is at most 4000 bytes of text")?;
        let list = match args.get("choices") {
            None => &[][..],
            Some(Value::Array(list))
                if list.is_empty() || (MIN_CHOICES..=MAX_CHOICES).contains(&list.len()) =>
            {
                list.as_slice()
            }
            _ => return Err("choices lists 2 to 4 short labels, or omit it for a chat answer"),
        };
        let mut choices = Vec::new();
        for (index, label) in list.iter().enumerate() {
            let label = label
                .as_str()
                .map(str::trim)
                .filter(|l| !l.is_empty() && l.len() <= MAX_CHOICE_LABEL && !l.contains('\0'))
                .ok_or("each choice is 1 to 120 bytes of text")?;
            if choices.iter().any(|c: &Choice| c.label == label) {
                return Err("choices must be distinct");
            }
            choices.push(Choice {
                id: ((b'a' + index as u8) as char).to_string(),
                label: label.to_owned(),
            });
        }
        Ok(Self {
            kind: Kind::Choice,
            title: title.to_owned(),
            context: context.map(str::to_owned),
            choices,
            approval: None,
        })
    }

    /// Parse `ask_permission` arguments: the kind, the exact action the bot
    /// will take on the host, and why.
    pub fn permission(args: &Value) -> Result<Self, &'static str> {
        only_keys(args, &["kind", "action", "reason"])
            .map_err(|_| "use kind, action, and optionally reason")?;
        let kind = args["kind"]
            .as_str()
            .and_then(Kind::approval)
            .ok_or("kind is destructive or external")?;
        let action = text_arg(args, "action", MAX_CONTEXT)
            .ok()
            .flatten()
            .ok_or("action is 1 to 4000 bytes: exactly what will be done")?;
        let reason = text_arg(args, "reason", MAX_TITLE)
            .map_err(|_| "reason is at most 500 bytes of text")?;
        let title = reason.map(str::to_owned).unwrap_or_else(|| {
            match kind {
                Kind::External => "Allow this action that reaches outside your machine?",
                _ => "Allow this action on your machine?",
            }
            .to_owned()
        });
        Ok(Self {
            kind,
            title,
            context: Some(action.to_owned()),
            choices: approval_choices(kind),
            approval: Some(Approval {
                action: Action::Host {
                    host: action.to_owned(),
                },
                hash: hash(&[b"host", kind.as_str().as_bytes(), action.as_bytes()]),
            }),
        })
    }

    /// An approval bound to one computer tool call: the tool and its exact
    /// arguments. `title` and `context` are what the card shows.
    pub fn tool_approval(
        kind: Kind,
        tool: &str,
        args: &Value,
        title: String,
        context: String,
    ) -> Self {
        let mut canonical = args.clone();
        canonical.sort_all_objects();
        let canonical = canonical.to_string();
        Self {
            kind,
            title,
            context: Some(context),
            choices: approval_choices(kind),
            approval: Some(Approval {
                action: Action::Tool {
                    tool: tool.to_owned(),
                    args: crate::app::tools::loggable_args(tool, args),
                },
                hash: hash(&[b"tool", tool.as_bytes(), canonical.as_bytes()]),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_choice_question_parses_only_its_fields_and_letters_its_options() {
        let spec = Spec::choice(&json!({
            "question": " Which newsletter template? ",
            "choices": ["Plain text", "HTML digest", "Both"],
            "context": "Draft: Hello everyone...",
        }))
        .unwrap();
        assert_eq!(spec.kind, Kind::Choice);
        assert_eq!(spec.title, "Which newsletter template?");
        let ids: Vec<&str> = spec.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(spec.approval, None);
        for bad in [
            json!({ "question": "q", "choices": ["only one"] }),
            json!({ "question": "q", "choices": ["a", "a"] }),
            json!({ "question": "q", "choices": ["a", "b", "c", "d", "e"] }),
            json!({ "question": " ", "choices": ["a", "b"] }),
            json!({ "question": "q", "choices": ["a", "b"], "value": "x" }),
            json!({ "question": "q", "choices": [1, 2] }),
            json!(["q"]),
        ] {
            assert!(Spec::choice(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_permission_binds_the_described_action_and_offers_forever_only_for_destructive() {
        let delete = Spec::permission(&json!({
            "kind": "destructive", "action": "rm -rf ~/old-site", "reason": "Remove the old site",
        }))
        .unwrap();
        assert_eq!(delete.kind, Kind::Destructive);
        assert_eq!(delete.title, "Remove the old site");
        assert_eq!(delete.context.as_deref(), Some("rm -rf ~/old-site"));
        let ids: Vec<&str> = delete.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [APPROVE, DENY, APPROVE_FOREVER]);

        let send =
            Spec::permission(&json!({ "kind": "external", "action": "mail -s hi bob@x" })).unwrap();
        let ids: Vec<&str> = send.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [APPROVE, DENY]);
        assert_ne!(
            send.approval.as_ref().map(|a| a.hash),
            delete.approval.as_ref().map(|a| a.hash)
        );
        assert!(Spec::permission(&json!({ "kind": "local", "action": "ls" })).is_err());
        assert!(Spec::permission(&json!({ "kind": "external" })).is_err());
    }

    #[test]
    fn a_tool_approval_hashes_the_exact_arguments_regardless_of_key_order() {
        let a = Spec::tool_approval(
            Kind::External,
            "computer_run",
            &json!({ "command": "./send.sh", "effect": "external" }),
            "t".into(),
            "c".into(),
        );
        let b = Spec::tool_approval(
            Kind::External,
            "computer_run",
            &json!({ "effect": "external", "command": "./send.sh" }),
            "t".into(),
            "c".into(),
        );
        let c = Spec::tool_approval(
            Kind::External,
            "computer_run",
            &json!({ "command": "./send.sh --all", "effect": "external" }),
            "t".into(),
            "c".into(),
        );
        assert_eq!(
            a.approval.as_ref().map(|a| a.hash),
            b.approval.as_ref().map(|a| a.hash)
        );
        assert_ne!(
            a.approval.as_ref().map(|a| a.hash),
            c.approval.as_ref().map(|a| a.hash)
        );
        assert_ne!(
            a.approval.as_ref().map(|a| a.hash),
            Spec::tool_approval(
                Kind::External,
                "computer_job_start",
                &json!({ "command": "./send.sh", "effect": "external" }),
                "t".into(),
                "c".into()
            )
            .approval
            .as_ref()
            .map(|a| a.hash)
        );
    }
}
