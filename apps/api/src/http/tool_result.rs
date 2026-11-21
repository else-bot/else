//! What a bot tool handler returns. A handler answers with [`Done`] or stops
//! early with a [`Refusal`], so `?` carries a refusal out of any helper;
//! [`render`] turns either into the MCP result and the memory log line once.

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub(crate) type ToolResult = Result<Done, Refusal>;

/// A tool's answer and the line the bot's memory keeps about it.
#[derive(Debug)]
pub(crate) struct Done(Reply);

/// How a tool ended early, in the wire dialect its file uses: a text error,
/// a JSON error envelope, a declined call, or a call parked on the owner.
/// Some effects may already have run, as the message says. A refusal writes
/// no memory line unless it says so.
#[derive(Debug)]
pub(crate) struct Refusal(Reply);

/// How a call ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Outcome {
    Done,
    /// The turn waits for the owner or other work; the result says
    /// `"parked": true`.
    Parked,
    /// Not run and not an error: the owner declined, or must act first.
    Declined,
    /// An error result, flagged `isError`.
    Failed,
}

#[derive(Debug)]
struct Reply {
    body: Body,
    log: Option<String>,
}

/// How a call with a result that is not flagged `isError` ended.
#[derive(Debug, Clone, Copy)]
enum Unflagged {
    Done,
    Parked,
    Declined,
}

#[derive(Debug)]
enum Body {
    Json(Value, Unflagged),
    FlaggedJson(Value),
    Text(String),
    Image { meta: Value, png: Vec<u8> },
}

impl Done {
    pub(crate) fn json(value: Value) -> Self {
        Self(Reply::new(Body::Json(value, Unflagged::Done)))
    }

    pub(crate) fn image(meta: Value, png: Vec<u8>) -> Self {
        Self(Reply::new(Body::Image { meta, png }))
    }

    /// Sets the result's `parked` field, which providers read, and the
    /// outcome. Give the field its place in the JSON: providers see its
    /// bytes, and a field set here for the first time goes last.
    pub(crate) fn parked(mut self, parked: bool) -> Self {
        self.0.park(parked);
        self
    }

    pub(crate) fn logged(mut self, log: impl Into<String>) -> Self {
        self.0.log = Some(log.into());
        self
    }
}

impl Refusal {
    /// A plain-text error: `isError` with `message` as its only text.
    pub(crate) fn text(message: impl Into<String>) -> Self {
        Self(Reply::new(Body::Text(message.into())))
    }

    /// A `CODE: message` text error, logged as `rejected CODE`.
    pub(crate) fn coded(code: &str, message: impl std::fmt::Display) -> Self {
        Self::text(format!("{code}: {message}")).logged(format!("rejected {code}"))
    }

    /// A call the owner declined or must act on first, as a result that is
    /// not an error.
    pub(crate) fn declined(value: Value) -> Self {
        Self(Reply::new(Body::Json(value, Unflagged::Declined)))
    }

    /// A call that parked the turn on the owner.
    pub(crate) fn parked(value: Value) -> Self {
        let mut reply = Reply::new(Body::Json(value, Unflagged::Done));
        reply.park(true);
        Self(reply)
    }

    /// A JSON result flagged `isError`.
    pub(crate) fn flagged(value: Value) -> Self {
        Self(Reply::new(Body::FlaggedJson(value)))
    }

    /// The JSON error envelope `{"error":{"code","message"}}`, flagged `isError`.
    pub(crate) fn envelope(code: &str, message: &str) -> Self {
        Self::flagged(json!({"error": {"code": code, "message": message}}))
    }

    pub(crate) fn logged(mut self, log: impl Into<String>) -> Self {
        self.0.log = Some(log.into());
        self
    }

    /// This refusal as the JSON error envelope, for tools whose callers parse
    /// every result as JSON. A `CODE: message` text keeps its code in lower
    /// case; other text gets the code `error`. JSON refusals stay as they are.
    pub(crate) fn enveloped(self) -> Self {
        let Body::Text(text) = &self.0.body else {
            return self;
        };
        if serde_json::from_str::<Value>(text).is_ok_and(|parsed| parsed.is_object()) {
            return self;
        }
        let (code, message) = match text.split_once(": ") {
            Some((code, message))
                if !code.is_empty()
                    && code.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') =>
            {
                (code.to_ascii_lowercase(), message)
            }
            _ => ("error".to_owned(), text.as_str()),
        };
        let log = self.0.log.clone();
        Self(Reply {
            log,
            ..Self::envelope(&code, message).0
        })
    }
}

impl Reply {
    fn new(body: Body) -> Self {
        Self { body, log: None }
    }

    fn park(&mut self, parked: bool) {
        let Body::Json(Value::Object(payload), ended) = &mut self.body else {
            unreachable!("only a JSON object result parks")
        };
        payload.insert("parked".into(), json!(parked));
        if parked {
            *ended = Unflagged::Parked;
        }
    }
}

/// How the call that returned `result` ended.
pub(crate) fn outcome(result: &ToolResult) -> Outcome {
    match result {
        Ok(Done(reply)) | Err(Refusal(reply)) => match reply.body {
            Body::Json(_, Unflagged::Done) | Body::Image { .. } => Outcome::Done,
            Body::Json(_, Unflagged::Parked) => Outcome::Parked,
            Body::Json(_, Unflagged::Declined) => Outcome::Declined,
            Body::FlaggedJson(_) | Body::Text(_) => Outcome::Failed,
        },
    }
}

/// Decodes a tool's arguments, refusing as `invalid` says when they do not fit.
pub(crate) fn args<T: DeserializeOwned>(
    args: &Value,
    invalid: impl FnOnce() -> Refusal,
) -> Result<T, Refusal> {
    serde_json::from_value(args.clone()).map_err(|_| invalid())
}

/// Edits a result's JSON payload, if it has one, and its log line, however
/// the tool ended.
pub(crate) fn amend(
    result: ToolResult,
    payload: impl FnOnce(&mut Value),
    log: impl FnOnce(Option<String>) -> Option<String>,
) -> ToolResult {
    let amend = |mut reply: Reply| {
        if let Body::Json(value, _) | Body::FlaggedJson(value) = &mut reply.body
            && value.is_object()
        {
            payload(value);
        }
        reply.log = log(reply.log);
        reply
    };
    match result {
        Ok(Done(reply)) => Ok(Done(amend(reply))),
        Err(Refusal(reply)) => Err(Refusal(amend(reply))),
    }
}

/// The text a bot reads first in a refusal.
pub(crate) fn first_text(refusal: Refusal) -> String {
    let (value, _) = render(Err(refusal));
    value["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The MCP result a bot reads and the line its memory keeps, if any.
pub(crate) fn render(result: ToolResult) -> (Value, Option<String>) {
    let Reply { body, log } = match result {
        Ok(Done(reply)) | Err(Refusal(reply)) => reply,
    };
    let value = match body {
        Body::Json(value, _) => text_result(value),
        Body::FlaggedJson(value) => {
            let mut value = text_result(value);
            value["isError"] = json!(true);
            value
        }
        Body::Text(message) => {
            json!({ "content": [{ "type": "text", "text": message }], "isError": true })
        }
        Body::Image { meta, png } => {
            use base64::Engine;
            json!({ "content": [
                { "type": "text", "text": meta.to_string() },
                { "type": "image", "mimeType": "image/png",
                  "data": base64::engine::general_purpose::STANDARD.encode(png) },
            ] })
        }
    };
    (value, log)
}

/// The bytes `value` takes as an MCP text result.
pub(crate) fn wire_size(value: &Value) -> usize {
    text_result(value.clone()).to_string().len()
}

/// At most `max` bytes of `text` from `start`, both moved back to a
/// character boundary, and where the slice begins.
pub(crate) fn slice(text: &str, start: usize, max: usize) -> (usize, &str) {
    let mut from = start.min(text.len());
    while !text.is_char_boundary(from) {
        from -= 1;
    }
    let mut to = from.saturating_add(max).min(text.len());
    while !text.is_char_boundary(to) {
        to -= 1;
    }
    (from, &text[from..to])
}

/// A JSON value as an MCP text result.
pub(crate) fn text_result(value: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": value.to_string() }] })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(refusal: Refusal) -> Value {
        render(Err(refusal)).0
    }

    #[test]
    fn a_parked_result_keeps_its_parked_field_where_the_tool_wrote_it() {
        let parked =
            Done::json(json!({ "status": "pending", "parked": true, "next": "End." })).parked(true);
        assert_eq!(outcome(&Ok(parked)), Outcome::Parked);
        let parked =
            Done::json(json!({ "status": "pending", "parked": true, "next": "End." })).parked(true);
        assert_eq!(
            render(Ok(parked)).0["content"][0]["text"],
            "{\"status\":\"pending\",\"parked\":true,\"next\":\"End.\"}"
        );
        let settled = Done::json(json!({ "status": "settled", "parked": false })).parked(false);
        assert_eq!(outcome(&Ok(settled)), Outcome::Done);
    }

    #[test]
    fn a_plain_text_refusal_becomes_the_json_error_envelope() {
        let denied = Refusal::text("DENIED: the owner declined this external action.")
            .logged("denied")
            .enveloped();
        let (value, log) = render(Err(denied));
        assert_eq!(
            value,
            json!({"content": [{"type": "text", "text":
                "{\"error\":{\"code\":\"denied\",\"message\":\"the owner declined this external action.\"}}"}],
                "isError": true})
        );
        assert_eq!(log.as_deref(), Some("denied"), "the log line is kept");
        assert_eq!(
            wire(Refusal::text("The shared MCP library is unavailable.").enveloped()),
            wire(Refusal::envelope(
                "error",
                "The shared MCP library is unavailable."
            ))
        );
        assert_eq!(
            wire(Refusal::envelope("invalid_request", "Use an MCP id only.").enveloped()),
            wire(Refusal::envelope("invalid_request", "Use an MCP id only.")),
            "an envelope passes through"
        );
        assert_eq!(
            wire(Refusal::text("{\"mcp_id\":\"x\"}").enveloped()),
            json!({"content": [{"type": "text", "text": "{\"mcp_id\":\"x\"}"}], "isError": true}),
            "JSON text is kept"
        );
        assert_eq!(
            wire(Refusal::declined(json!({"status": "denied"})).enveloped()),
            json!({"content": [{"type": "text", "text": "{\"status\":\"denied\"}"}]}),
            "a result that is not an error is kept"
        );
    }
}
