//! MCP tools for outgoing drafts the owner approves: emails, messages, and
//! posts. `draft_email` and
//! `draft_message` parse their channel's content; every other step is
//! shared. Drafts bound to connected email or social accounts are sent by
//! the API through that account (`send_draft`), as one durable action. Other
//! drafts are sent by the bot's existing delivery: an external computer
//! tool call naming the approved draft, or its host tools after
//! `start_draft_send`.

use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::mcp::{Outcome, tool_error, tool_result};
use crate::app::outgoing::{self, Content, Delivery, Email, MAX_DETAIL, Message, Platform, Status};
use crate::connectors::{store, types::MailError};
use crate::db::outgoing_drafts::{self, DraftError, OutgoingDraft};
use crate::grants::{Grant, Tool};

pub const TOOL_NAMES: &[&str] = &[
    "draft_email",
    "draft_message",
    "confirm_draft_approval",
    "start_draft_send",
    "send_draft",
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
                owner, instead of ask_permission or an approval card. To send from a connected \
                account (mail_accounts with send permission), pass its account_id instead of \
                from, and reply_to to answer a message in its thread; else.bot then sends it \
                with send_draft. They may edit its subject and body. {REVIEW}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "from": { "type": "string", "minLength": 3, "maxLength": outgoing::MAX_ADDRESS,
                              "description": "The address you will send from with your own \
                                  tools. Omit with account_id." },
                    "account_id": { "type": "string", "format": "uuid",
                                    "description": "A connected account from mail_accounts that \
                                        this email goes from." },
                    "reply_to": { "type": "object",
                                  "description": "With account_id: the message this answers, \
                                      {folder, uid_validity, uid} from mail_action. Only its \
                                      thread is used; set to, cc, and subject yourself.",
                                  "properties": {
                                      "folder": { "type": "string" },
                                      "uid_validity": { "type": "integer", "minimum": 1 },
                                      "uid": { "type": "integer", "minimum": 1 } },
                                  "required": ["folder", "uid_validity", "uid"],
                                  "additionalProperties": false },
                    "to": { "type": "array", "minItems": 1, "maxItems": outgoing::MAX_RECIPIENTS,
                            "items": address },
                    "cc": list.clone(),
                    "bcc": list,
                    "subject": { "type": "string", "maxLength": outgoing::MAX_SUBJECT },
                    "body": body.clone(),
                    "replaces": draft_id(),
                },
                "required": ["to", "subject", "body"],
                "additionalProperties": false,
            },
            "annotations": write,
        }),
        json!({
            "name": "draft_message",
            "description": format!("Show the owner a message or post you drafted for a \
                messaging or social platform: x for a post on X (Twitter), telegram for a \
                Telegram message, discord for a Discord message. With a native social connection pass account_id and the exact destination ID from social_connections or a public Telegram @username or t.me link; sending uses send_draft. Use this whenever the owner asks to draft, write, revise, or \
                send one you would send for the owner, instead of ask_permission or an approval \
                card. It has no subject or addresses; use draft_email for email. They may edit \
                its text. {REVIEW}"),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "platform": { "type": "string", "enum": platforms },
                    "account_id": {"type":"string","format":"uuid","description":"Native social connection ID from social_connections. Sends through send_draft."},
                    "reply_to": handle("Optional remote message ID being replied to."),
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
                and replaces), a question, or a hesitation. With a connected account, send \
                it with send_draft. Otherwise send it once (see start_draft_send and the \
                approved_draft argument of computer tools), then report with report_draft_send.",
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
            "name": "send_draft",
            "description": "Send an approved email or Telegram/Discord message from a connected account \
                (drafted with account_id). else.bot sends exactly the approved revision once \
                through that account and records the receipt on the card; do not call \
                report_draft_send for it. Calling it again in this request returns the same \
                send, never a second one. Status sending means the result is not known yet: \
                call it again later in this request, never mail_action.",
            "inputSchema": {
                "type": "object",
                "properties": { "draft_id": draft_id() },
                "required": ["draft_id"],
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false,
                             "idempotentHint": true, "openWorldHint": true },
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
        "draft_email" => match sender(state, grant, args).await {
            Ok(account) => {
                let email = Email::parse(args, account.as_deref()).map(Content::Email);
                draft(state, grant, args, email).await
            }
            Err(outcome) => Err(outcome),
        },
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
        "send_draft" => send(state, grant, args).await,
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

async fn sender(state: &AppState, grant: &Grant, args: &Value) -> Result<Option<String>, Outcome> {
    let Some(id) = args.get("account_id").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let Some(id) = id.as_str().and_then(|id| id.parse::<Uuid>().ok()) else {
        return Ok(None);
    };
    let forbidden = || {
        (
            tool_error(
                "MAIL_FORBIDDEN: this task cannot send from that account. Use an account \
                        mail_accounts lists with send permission, or omit account_id and send \
                        with your own tools.",
            ),
            Some("rejected MAIL_FORBIDDEN".to_owned()),
        )
    };
    if !grant.allows(Tool::Mail) {
        return Err(forbidden());
    }
    let (account, permissions) = tokio::join!(
        store::account(&state.db, id),
        store::permissions(&state.db, id, grant.bot_id)
    );
    let unavailable = |_| {
        (
            tool_error("UNAVAILABLE: mail accounts could not be read."),
            None,
        )
    };
    match (
        account.map_err(unavailable)?,
        permissions.map_err(unavailable)?,
    ) {
        (Some(account), Some(p)) if account.state == "active" && p.read && p.write && p.send => {
            Ok(Some(account.config.address))
        }
        _ => Err(forbidden()),
    }
}

async fn confirm(state: &AppState, grant: &Grant, args: &Value) -> Result<Outcome, Outcome> {
    only(args, &["draft_id"])?;
    let id = id_arg(args, "draft_id")?;
    let row = outgoing_drafts::confirm(&state.db, grant.turn_id, id)
        .await
        .map_err(refused)?;
    let next = if row.content.account().is_some() {
        "Send exactly this email with send_draft and this draft_id. It uses the connected \
         account and records the result; do not send with other tools or report_draft_send."
            .to_owned()
    } else {
        format!(
            "Send exactly this {} now, once: with a computer tool declared \
                 effect external and approved_draft set to this draft_id, or with your host \
                 tools after start_draft_send. Then call report_draft_send.",
            row.content.noun()
        )
    };
    Ok((
        tool_result(json!({
            "status": "approved",
            "draft": draft_json(&row),
            "next": next,
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

enum ConnectedPayload<'a> {
    Mail {
        account_id: Uuid,
        action_id: Uuid,
        email: &'a Email,
    },
    Social {
        account_id: Uuid,
        action_id: Uuid,
        message: &'a Message,
        destination: &'a str,
    },
}

fn connected_payload(draft: &OutgoingDraft) -> Result<ConnectedPayload<'_>, DraftError> {
    match (&draft.content.0, draft.delivery.as_deref()) {
        (
            Content::Email(email),
            Some(Delivery::Mail {
                account_id,
                action_id,
            }),
        ) if email.account_id == Some(*account_id) => Ok(ConnectedPayload::Mail {
            account_id: *account_id,
            action_id: *action_id,
            email,
        }),
        (
            Content::Message(message),
            Some(Delivery::Social {
                account_id,
                action_id,
            }),
        ) if message.account_id == Some(*account_id) => Ok(ConnectedPayload::Social {
            account_id: *account_id,
            action_id: *action_id,
            message,
            destination: message
                .destination
                .as_deref()
                .ok_or(DraftError::NotConnected)?,
        }),
        _ => Err(DraftError::NotConnected),
    }
}

async fn send(state: &AppState, grant: &Grant, args: &Value) -> Result<Outcome, Outcome> {
    only(args, &["draft_id"])?;
    let id = id_arg(args, "draft_id")?;
    let draft = outgoing_drafts::load_for(&state.db, grant.bot_id, id)
        .await
        .map_err(refused)?;
    let account_id = draft
        .content
        .account()
        .ok_or_else(|| refused(DraftError::NotConnected))?;
    let delivery = match &draft.content.0 {
        Content::Email(_) if grant.allows(Tool::Mail) && state.connectors.is_some() => {
            Delivery::Mail {
                account_id,
                action_id: Uuid::new_v4(),
            }
        }
        Content::Message(_) if grant.allows(Tool::Social) && state.socials.is_some() => {
            Delivery::Social {
                account_id,
                action_id: Uuid::new_v4(),
            }
        }
        _ => {
            return Err((
                tool_error("Native sending is unavailable for this task and account."),
                None,
            ));
        }
    };
    let settled = matches!(
        draft.status,
        Status::Sent | Status::Failed | Status::Uncertain
    ) && draft.send_turn_id == Some(grant.turn_id)
        && matches!(
            (draft.delivery.as_deref(), &delivery),
            (Some(Delivery::Mail { .. }), Delivery::Mail { .. })
                | (Some(Delivery::Social { .. }), Delivery::Social { .. })
        );
    let row = if settled {
        draft
    } else {
        let row = outgoing_drafts::claim(&state.db, grant.turn_id, id, &delivery)
            .await
            .map_err(refused)?;
        match connected_payload(&row).map_err(refused)? {
            ConnectedPayload::Mail {
                account_id,
                action_id,
                email,
            } => {
                let manager = state
                    .connectors
                    .as_ref()
                    .ok_or_else(|| refused(DraftError::NotConnected))?;
                let result = manager
                    .execute(
                        account_id,
                        grant.bot_id,
                        grant.turn_id,
                        Some(action_id),
                        email.send_command(),
                    )
                    .await;
                let refusal = result.as_ref().err().map(|err: &MailError| err.code);
                outgoing_drafts::after_mail_send(&state.db, id, refusal)
                    .await
                    .map_err(refused)?
            }
            ConnectedPayload::Social {
                account_id,
                action_id,
                message,
                destination,
            } => {
                let manager = state
                    .socials
                    .as_ref()
                    .ok_or_else(|| refused(DraftError::NotConnected))?;
                let result = async {
                    let connection =
                        crate::socials::store::connection(&state.db, account_id).await?;
                    let platform = match connection.provider {
                        crate::socials::types::Provider::Telegram => Platform::Telegram,
                        crate::socials::types::Provider::Discord => Platform::Discord,
                    };
                    if message.platform != platform {
                        return Err(crate::socials::types::Error::Invalid(
                            "The connection belongs to another platform.",
                        ));
                    }
                    manager
                        .send(
                            account_id,
                            grant.bot_id,
                            grant.turn_id,
                            action_id,
                            crate::socials::types::SendMessage {
                                destination: destination.to_owned(),
                                body: message.body.clone(),
                                reply_to: message.reply_to.clone(),
                            },
                        )
                        .await
                }
                .await;
                outgoing_drafts::after_social_send(
                    &state.db,
                    id,
                    result.err().map(|e| e.to_string()).as_deref(),
                )
                .await
                .map_err(refused)?
            }
        }
    };
    let next = match row.status {
        Status::Sent => "It was sent. Tell the owner briefly.",
        Status::Sending => {
            "The result is not known yet. Call send_draft again later in this request to read it; it never sends twice."
        }
        Status::Failed => "It was not sent. Tell the owner why; they can approve it again in chat.",
        _ => {
            "It may have been sent. Tell the owner to check the destination service; never send it again without their new approval."
        }
    };
    let medium = row.content.noun();
    Ok((
        tool_result(json!({
            "status": row.status,
            "draft_id": row.id,
            "medium": medium,
            "detail": row.result_detail,
            "next": next,
        })),
        Some(format!(
            "{medium} {} {} through its connected account",
            row.id,
            row.status.as_str()
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

#[cfg(test)]
mod tests;
