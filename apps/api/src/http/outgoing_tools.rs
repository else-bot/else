//! MCP tools for outgoing drafts the owner approves: emails, messages, and
//! posts (see docs/decisions/0003-outgoing-drafts.md). `draft_email` and
//! `draft_message` parse their channel's content; every other step is
//! shared. Sending itself uses the bot's existing delivery: an external
//! computer tool call naming the approved draft, or its host tools after
//! `start_draft_send`.

use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::mcp::{Outcome, tool_error, tool_result};
use crate::app::outgoing::{self, Content, Delivery, Email, MAX_DETAIL, Message, Platform, Status};
use crate::db::outgoing_drafts::{self, DraftError, OutgoingDraft};
use crate::grants::Grant;

pub const TOOL_NAMES: &[&str] = &[
    "draft_email",
    "draft_message",
    "confirm_draft_approval",
    "start_draft_send",
    "report_draft_send",
];

fn draft_id() -> Value {
    json!({ "type": "string", "format": "uuid", "description": "The draft's id." })
}

const REVIEW: &str = "The owner reviews it in a native card where they can edit it, approve, or \
    discard it. To revise a draft after the owner's feedback, pass its id as replaces: the old \
    card is replaced by the new revision, which keeps the same channel. Returns status pending at \
    once: tell the owner briefly that the draft is ready, end this attempt, and you continue \
    automatically when they decide. A draft is approved only on its card or by the owner's own \
    message sent while it was open; it is never sent before that. At most one per attempt.";

pub(super) fn schemas() -> Vec<Value> {
    let write = json!({ "readOnlyHint": false, "destructiveHint": false,
                        "idempotentHint": true, "openWorldHint": false });
    let address = json!({ "type": "string", "minLength": 3, "maxLength": outgoing::MAX_ADDRESS });
    let list = json!({ "type": "array", "maxItems": outgoing::MAX_RECIPIENTS, "items": address });
    let body = json!({ "type": "string", "minLength": 1, "maxLength": outgoing::MAX_BODY,
                       "description": "The complete plain text to send." });
    let handle = |description: &str| {
        json!({ "type": "string", "minLength": 1, "maxLength": outgoing::MAX_HANDLE,
                "description": description })
    };
    let platforms: Vec<&str> = Platform::ALL.iter().map(|p| p.as_str()).collect();
    vec![
        json!({
            "name": "draft_email",
            "description": format!("Show the owner an email you drafted. Use this whenever the \
                owner asks to draft, write, revise, or send an email you would send for the \
                owner, instead of ask_permission or an approval card. They may edit its subject \
                and body. {REVIEW}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "minLength": 3, "maxLength": outgoing::MAX_ADDRESS,
                              "description": "The address you will send from." },
                    "to": { "type": "array", "minItems": 1, "maxItems": outgoing::MAX_RECIPIENTS,
                            "items": address },
                    "cc": list.clone(),
                    "bcc": list,
                    "subject": { "type": "string", "maxLength": outgoing::MAX_SUBJECT },
                    "body": body.clone(),
                    "replaces": draft_id(),
                },
                "required": ["from", "to", "subject", "body"],
                "additionalProperties": false,
            },
            "annotations": write,
        }),
        json!({
            "name": "draft_message",
            "description": format!("Show the owner a message or post you drafted for a \
                messaging or social platform: x for a post on X (Twitter), telegram for a \
                Telegram message. Use this whenever the owner asks to draft, write, revise, or \
                send one you would send for the owner, instead of ask_permission or an approval \
                card. It has no subject or addresses; use draft_email for email. They may edit \
                its text. {REVIEW}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "platform": { "type": "string", "enum": platforms },
                    "account": handle("The account you will send from, as the platform shows \
                        it (for example @handle). Optional."),
                    "destination": handle("Where it goes, as the platform knows it: the \
                        Telegram chat, group, channel, or user (required for telegram), or for x \
                        the post it replies to. Omit for a new public X post."),
                    "body": body,
                    "replaces": draft_id(),
                },
                "required": ["platform", "body"],
                "additionalProperties": false,
            },
            "annotations": write,
        }),
        json!({
            "name": "confirm_draft_approval",
            "description": "Record that the owner's latest message approves sending the draft \
                that was open when they sent it, exactly as it now reads. Call it only when the \
                [else.bot outgoing drafts] context marks the draft in_context=\"true\" and the \
                owner's own words clearly say to send it as it is (for example \"looks good, send \
                it\"). Not for a request to change it (revise with draft_email or draft_message \
                and replaces), a question, or a hesitation. Then send it once (see \
                start_draft_send and the approved_draft argument of computer tools) and report \
                with report_draft_send.",
            "inputSchema": {
                "type": "object",
                "properties": { "draft_id": draft_id() },
                "required": ["draft_id"],
                "additionalProperties": false,
            },
            "annotations": write,
        }),
        json!({
            "name": "start_draft_send",
            "description": "Claim an approved draft before sending it with your HOST tools (the \
                owner's machine). Returns the exact content to send; send exactly that, once, \
                then call report_draft_send. With a computer tool, do not call this: pass the \
                draft id as approved_draft with effect external instead, which claims it for \
                that one call. No other approval is needed for that one send.",
            "inputSchema": {
                "type": "object",
                "properties": { "draft_id": draft_id() },
                "required": ["draft_id"],
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false,
                             "idempotentHint": false, "openWorldHint": false },
        }),
        json!({
            "name": "report_draft_send",
            "description": "Report how sending an approved draft ended: sent only after the mail \
                server, platform, or service confirmed it; failed when it was refused or never \
                sent; uncertain when it may have gone out (a timeout or dropped connection after \
                sending). Never send an uncertain draft again without the owner's new approval.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "draft_id": draft_id(),
                    "status": { "type": "string", "enum": ["sent", "failed", "uncertain"] },
                    "detail": { "type": "string", "maxLength": MAX_DETAIL,
                                "description": "The receipt, message or post id, or the error." },
                },
                "required": ["draft_id", "status"],
                "additionalProperties": false,
            },
            "annotations": write,
        }),
    ]
}

fn draft_json(draft: &OutgoingDraft) -> Value {
    json!({
        "draft_id": draft.id,
        "lineage_id": draft.lineage_id,
        "revision": draft.revision,
        "status": draft.status,
        "content": draft.content,
    })
}

pub(super) fn refused(err: DraftError) -> Outcome {
    let code = err.code();
    if let DraftError::Db(err) = &err {
        tracing::warn!(error = %err, "outgoing draft tool failed");
    }
    (
        tool_error(&format!("{code}: {err}")),
        Some(format!("rejected {code}")),
    )
}

fn id_arg(args: &Value, key: &str) -> Result<Uuid, Outcome> {
    args[key]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| (tool_error(&format!("{key} must be a draft id.")), None))
}

fn only(args: &Value, allowed: &[&str]) -> Result<(), Outcome> {
    match args.as_object() {
        Some(object) if object.keys().all(|k| allowed.contains(&k.as_str())) => Ok(()),
        _ => Err((
            tool_error(&format!("use only {}.", allowed.join(", "))),
            None,
        )),
    }
}

pub(super) async fn call(state: &AppState, grant: &Grant, name: &str, args: &Value) -> Outcome {
    let result = match name {
        "draft_email" => draft(state, grant, args, Email::parse(args).map(Content::Email)).await,
        "draft_message" => {
            draft(
                state,
                grant,
                args,
                Message::parse(args).map(Content::Message),
            )
            .await
        }
        "confirm_draft_approval" => confirm(state, grant, args).await,
        "start_draft_send" => start(state, grant, args).await,
        _ => report(state, grant, args).await,
    };
    result.unwrap_or_else(|outcome| outcome)
}

async fn draft(
    state: &AppState,
    grant: &Grant,
    args: &Value,
    content: Result<Content, &'static str>,
) -> Result<Outcome, Outcome> {
    let content =
        content.map_err(|message| (tool_error(&format!("INVALID_DRAFT: {message}")), None))?;
    let replaces = match &args["replaces"] {
        Value::Null => None,
        _ => Some(id_arg(args, "replaces")?),
    };
    let row = outgoing_drafts::propose(&state.db, grant.turn_id, &content, replaces)
        .await
        .map_err(refused)?;
    if row.status != Status::Pending {
        let log = format!("already {} {}", row.status.as_str(), row.id);
        return Ok((
            tool_result(json!({ "draft": draft_json(&row),
                "next": "You already drafted this in this request; follow its status." })),
            Some(log),
        ));
    }
    state.scheduler.grants().park(grant.turn_id).await;
    let log = format!(
        "drafted {} {} revision {} to {}",
        row.content.noun(),
        row.id,
        row.revision,
        row.content.audience()
    );
    Ok((
        tool_result(json!({
            "status": "pending",
            "parked": true,
            "draft": draft_json(&row),
            "next": "Tell the owner briefly that the draft is ready for review and end this \
                     attempt now. Do not send it, wait, poll, or call more tools; their decision \
                     arrives in a continuation.",
        })),
        Some(log),
    ))
}

async fn confirm(state: &AppState, grant: &Grant, args: &Value) -> Result<Outcome, Outcome> {
    only(args, &["draft_id"])?;
    let id = id_arg(args, "draft_id")?;
    let row = outgoing_drafts::confirm(&state.db, grant.turn_id, id)
        .await
        .map_err(refused)?;
    Ok((
        tool_result(json!({
            "status": "approved",
            "draft": draft_json(&row),
            "next": format!("Send exactly this {} now, once: with a computer tool declared \
                     effect external and approved_draft set to this draft_id, or with your host \
                     tools after start_draft_send. Then call report_draft_send.",
                     row.content.noun()),
        })),
        Some(format!(
            "owner message approved {} {} version {}",
            row.content.noun(),
            row.id,
            row.version
        )),
    ))
}

async fn start(state: &AppState, grant: &Grant, args: &Value) -> Result<Outcome, Outcome> {
    only(args, &["draft_id"])?;
    let id = id_arg(args, "draft_id")?;
    let row = outgoing_drafts::claim(&state.db, grant.turn_id, id, &Delivery::Host)
        .await
        .map_err(refused)?;
    Ok((
        tool_result(json!({
            "status": "sending",
            "draft": draft_json(&row),
            "next": format!("Send exactly this {} now with your host tools, once, then call \
                     report_draft_send with the result. Do not send it any other way or again.",
                     row.content.noun()),
        })),
        Some(format!(
            "claimed {} {} for a host send",
            row.content.noun(),
            row.id
        )),
    ))
}

async fn report(state: &AppState, grant: &Grant, args: &Value) -> Result<Outcome, Outcome> {
    only(args, &["draft_id", "status", "detail"])?;
    let id = id_arg(args, "draft_id")?;
    let outcome = args["status"]
        .as_str()
        .and_then(outgoing::Outcome::parse)
        .ok_or_else(|| (tool_error("status is sent, failed, or uncertain."), None))?;
    let detail = outgoing::detail(args.get("detail"))
        .map_err(|message| (tool_error(&format!("INVALID_REPORT: {message}")), None))?;
    let (row, changed) =
        outgoing_drafts::report(&state.db, grant.turn_id, id, outcome, detail.as_deref())
            .await
            .map_err(refused)?;
    let noun = row.content.noun();
    let log = if changed {
        format!("{noun} {} {}", row.id, row.status.as_str())
    } else {
        format!("{noun} {} already {}", row.id, row.status.as_str())
    };
    Ok((
        tool_result(json!({ "status": row.status, "draft_id": row.id, "medium": noun })),
        Some(log),
    ))
}

/// The approved draft an external computer tool call sends, claimed for
/// exactly that call: it needs no other approval card.
pub(super) async fn claim_for_tool(
    state: &AppState,
    grant: &Grant,
    name: &str,
    args: &Value,
) -> Result<String, Outcome> {
    let id = id_arg(args, "approved_draft")?;
    let delivery = Delivery::Tool {
        tool: name.to_owned(),
        args: crate::app::tools::loggable_args(name, args),
    };
    let row = outgoing_drafts::claim(&state.db, grant.turn_id, id, &delivery)
        .await
        .map_err(|err| {
            let code = err.code();
            (
                tool_error(&format!(
                    "{code}: {err}. Nothing was done. approved_draft names a draft approved for \
                     this request and not yet sent."
                )),
                Some(format!("rejected {code}")),
            )
        })?;
    Ok(format!(
        "sends approved {} {} version {}",
        row.content.noun(),
        row.id,
        row.version
    ))
}
