//! Email drafts a bot proposes and the owner approves (see
//! docs/decisions/0003-email-drafts.md): the typed draft, its lifecycle
//! states, and parsing of the bot's and the owner's input.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const MAX_ADDRESS: usize = 320;
pub const MAX_RECIPIENTS: usize = 50;
pub const MAX_SUBJECT: usize = 998;
pub const MAX_BODY: usize = 100_000;
pub const MAX_DETAIL: usize = 2000;

/// Where a draft is in its life. `pending` is the only state the owner can
/// edit. `approved` allows exactly one send by its send request; the claim
/// moves it to `sending`, and only the bot's report for that claim settles
/// it as `sent`, `failed`, or `uncertain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Status {
    Pending,
    Approved,
    Sending,
    Sent,
    Failed,
    Uncertain,
    Discarded,
    Superseded,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
            Self::Discarded => "discarded",
            Self::Superseded => "superseded",
        }
    }

    /// The owner may still decide to send it: as drafted, or again after a
    /// failed or uncertain attempt (a chat approval only).
    pub fn approvable_in_chat(self) -> bool {
        matches!(self, Self::Pending | Self::Failed | Self::Uncertain)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum ApprovalSource {
    /// The Approve button on the card.
    Button,
    /// An owner message sent while the draft was open, which the bot read
    /// as approval of that exact version.
    Message,
}

/// How the bot reports a claimed send ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    Failed,
    Uncertain,
}

impl Outcome {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "sent" => Some(Self::Sent),
            "failed" => Some(Self::Failed),
            "uncertain" => Some(Self::Uncertain),
            _ => None,
        }
    }

    pub fn status(self) -> Status {
        match self {
            Self::Sent => Status::Sent,
            Self::Failed => Status::Failed,
            Self::Uncertain => Status::Uncertain,
        }
    }
}

/// The one call that sent (or is sending) an approved draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "via", rename_all = "lowercase")]
pub enum Delivery {
    /// A computer tool call declared external, with its loggable arguments.
    Tool { tool: String, args: Value },
    /// The bot's host tools, which the API cannot see.
    Host,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipients {
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
}

/// What the bot drafts. The owner can change only `subject` and `body`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub from: String,
    pub recipients: Recipients,
    pub subject: String,
    pub body: String,
}

fn address(value: &Value) -> Result<String, &'static str> {
    let text = value
        .as_str()
        .map(str::trim)
        .ok_or("each address is text")?;
    if text.len() < 3
        || text.len() > MAX_ADDRESS
        || !text.contains('@')
        || text.chars().any(char::is_control)
    {
        return Err("each address is 3 to 320 bytes with an @ and no line breaks");
    }
    Ok(text.to_owned())
}

fn addresses(args: &Value, key: &str, required: bool) -> Result<Vec<String>, &'static str> {
    let list = match args.get(key) {
        None | Some(Value::Null) if !required => return Ok(Vec::new()),
        Some(Value::Array(list)) => list,
        _ => return Err("to is a list of 1 to 50 addresses; cc and bcc are optional lists"),
    };
    if (required && list.is_empty()) || list.len() > MAX_RECIPIENTS {
        return Err("to is a list of 1 to 50 addresses; cc and bcc are optional lists");
    }
    list.iter().map(address).collect()
}

/// A subject: one line, possibly empty.
pub fn subject(text: &str) -> Result<String, &'static str> {
    let text = text.trim();
    if text.len() > MAX_SUBJECT || text.chars().any(char::is_control) {
        return Err("subject is one line of at most 998 bytes");
    }
    Ok(text.to_owned())
}

/// A body: some text, at most 100 KB, no NUL.
pub fn body(text: &str) -> Result<String, &'static str> {
    if text.trim().is_empty() || text.len() > MAX_BODY || text.contains('\0') {
        return Err("body is 1 to 100000 bytes of text");
    }
    Ok(text.to_owned())
}

/// A short receipt or error the bot reports for a send.
pub fn detail(value: Option<&Value>) -> Result<Option<String>, &'static str> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if text.len() <= MAX_DETAIL && !text.contains('\0') => {
            Ok(Some(text.trim().to_owned()).filter(|t| !t.is_empty()))
        }
        Some(_) => Err("detail is at most 2000 bytes of text"),
    }
}

impl Draft {
    /// Parse `draft_email` arguments (`replaces` is read by the caller).
    pub fn parse(args: &Value) -> Result<Self, &'static str> {
        let object = args
            .as_object()
            .ok_or("use from, to, subject, body, and optionally cc, bcc, replaces")?;
        let allowed = ["from", "to", "cc", "bcc", "subject", "body", "replaces"];
        if !object.keys().all(|k| allowed.contains(&k.as_str())) {
            return Err("use from, to, subject, body, and optionally cc, bcc, replaces");
        }
        let from = address(args.get("from").unwrap_or(&Value::Null))
            .map_err(|_| "from is the sending address, 3 to 320 bytes with an @")?;
        let recipients = Recipients {
            to: addresses(args, "to", true)?,
            cc: addresses(args, "cc", false)?,
            bcc: addresses(args, "bcc", false)?,
        };
        let subject = subject(args["subject"].as_str().ok_or("subject is text")?)?;
        let body = body(args["body"].as_str().ok_or("body is text")?)?;
        Ok(Self {
            from,
            recipients,
            subject,
            body,
        })
    }
}

/// A draft as a turn's attempt is told about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: Uuid,
    pub email_id: Uuid,
    pub revision: i64,
    pub draft: Draft,
    pub status: Status,
    pub version: i64,
    /// This request's owner message was sent with the draft open at its
    /// current version: the owner's words may approve exactly it.
    pub in_context: bool,
    /// The owner approved or discarded it on its card, and this is the
    /// first attempt to hear it.
    pub decided: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_draft_parses_only_its_fields_and_refuses_header_injection() {
        let draft = Draft::parse(&json!({
            "from": "bot@example.com", "to": [" ada@example.com "], "cc": ["Bob <bob@example.com>"],
            "subject": " Lunch on Friday ", "body": "Hi Ada,\n\nLunch on Friday?\n",
        }))
        .unwrap();
        assert_eq!(draft.recipients.to, ["ada@example.com"]);
        assert_eq!(draft.recipients.cc, ["Bob <bob@example.com>"]);
        assert!(draft.recipients.bcc.is_empty());
        assert_eq!(draft.subject, "Lunch on Friday");
        assert_eq!(draft.body, "Hi Ada,\n\nLunch on Friday?\n");
        let ok = json!({ "from": "b@x.io", "to": ["a@x.io"], "subject": "", "body": "x" });
        assert!(Draft::parse(&ok).is_ok(), "an empty subject is allowed");
        for (key, bad) in [
            ("to", json!([])),
            ("to", json!(["nobody"])),
            ("to", json!(["a@x.io\r\nBcc: evil@x.io"])),
            ("subject", json!("Hi\r\nBcc: evil@x.io")),
            ("body", json!("  ")),
            ("from", json!("not an address")),
            ("extra", json!(1)),
        ] {
            let mut args = ok.clone();
            args[key] = bad;
            assert!(Draft::parse(&args).is_err(), "{args}");
        }
    }
}
