//! MCP tools for outgoing drafts the owner approves: emails, messages, and
//! posts. `draft_email` and
//! `draft_message` parse their channel's content; every other step is
//! shared. Drafts bound to connected email or social accounts are sent by
//! the API through that account (`send_draft`), as one durable action. Other
//! drafts are sent by the bot's existing delivery: an external computer
//! tool call naming the approved draft, or its host tools after
//! `start_draft_send`.

use crate::app::tools::ToolDef;
use crate::http::tool_result::{Done, Refusal, ToolResult};
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use crate::app::outgoing::send::{self, Connected, SendError};
use crate::app::outgoing::{
    self, ApprovalSource, Channels, Content, Delivery, Email, MAX_DETAIL, Message, Platform, Status,
};
use crate::connectors::{
    store,
    types::{AccountState, MailCommand, MailError},
};
use crate::db::outgoing_drafts::{self, DraftError, OutgoingDraft};
use crate::grants::{Grant, Tool};
use crate::socials::types::{self as socials, SendMessage};

fn draft_id() -> Value {
    json!({ "type": "string", "format": "uuid", "description": "The draft's id." })
}

fn write() -> Value {
    json!({ "readOnlyHint": false, "destructiveHint": false,
            "idempotentHint": true, "openWorldHint": false })
}

fn address() -> Value {
    json!({ "type": "string", "minLength": 3, "maxLength": outgoing::MAX_ADDRESS })
}

fn list() -> Value {
    json!({ "type": "array", "maxItems": outgoing::MAX_RECIPIENTS, "items": address() })
}

fn body() -> Value {
    json!({ "type": "string", "minLength": 1, "maxLength": outgoing::MAX_BODY,
            "description": "The complete plain text to send." })
}

fn attachments() -> Value {
    json!({
        "type": "array", "maxItems": outgoing::MAX_FILES,
        "description": "Files to send: {path} in your project or ~/work, or {email} for an \
            attachment of an email you can read, by its part. Limits per platform are checked now.",
        "items": { "oneOf": [
            { "type": "object", "required": ["path"], "additionalProperties": false,
              "properties": { "path": { "type": "string", "minLength": 1 } } },
            { "type": "object", "required": ["email"], "additionalProperties": false,
              "properties": { "email": {
                  "type": "object", "required": ["account_id", "message", "part"],
                  "additionalProperties": false,
                  "properties": {
                      "account_id": { "type": "string", "format": "uuid" },
                      "message": { "type": "object",
                                   "required": ["folder", "uid_validity", "uid"],
                                   "properties": {
                                       "folder": { "type": "string" },
                                       "uid_validity": { "type": "integer", "minimum": 1 },
                                       "uid": { "type": "integer", "minimum": 1 } },
                                   "additionalProperties": false },
                      "part": { "type": "integer", "minimum": 0 } } } } } ] },
    })
}

fn handle(description: &str) -> Value {
    json!({ "type": "string", "minLength": 1, "maxLength": outgoing::MAX_HANDLE,
            "description": description })
}

fn platforms() -> Vec<&'static str> {
    Platform::ALL.iter().map(|p| p.as_str()).collect()
}

pub(super) fn email_schema() -> Value {
    json!({
        "description": "Draft an email for the owner to review, edit, and approve in a card; use it, not \
            ask_permission, for any email you would send. For a connected account, pass \
            account_id instead of from, and reply_to to answer one of its messages; send_draft \
            sends it once approved. To revise, pass replaces with the draft's id. At most one \
            per attempt.",
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
                        "items": address() },
                "cc": list(),
                "bcc": list(),
                "subject": { "type": "string", "maxLength": outgoing::MAX_SUBJECT },
                "body": body(),
                "attachments": attachments(),
                "replaces": draft_id(),
            },
            "required": ["to", "subject", "body"],
            "additionalProperties": false,
        },
        "annotations": write(),
    })
}

pub(super) fn message_schema() -> Value {
    json!({
        "description": "Draft a message or post (x, telegram, discord, whatsapp) for the owner to review, edit, and \
            approve in a card; use it, not ask_permission, for anything you would send. With a \
            native connection, pass account_id and the destination from social_connections or a \
            public Telegram @username or t.me link; send_draft sends it once approved. To \
            revise, pass replaces. At most one per attempt.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "platform": { "type": "string", "enum": platforms() },
                "account_id": {"type":"string","format":"uuid","description":"Native social connection ID from social_connections. Sends through send_draft."},
                "reply_to": handle("Optional remote message ID being replied to."),
                "account": handle("The account you will send from, as the platform shows \
                    it (for example @handle). Optional."),
                "destination": handle("Where it goes, as the platform knows it: the \
                    chat, group, channel, or user (required except for x), or for x \
                    the user to DM or the post it replies to. Omit for a new public X post."),
                "task": handle("The recurring job of a message to a chat or user (never a \
                    public post), such as \"Initial lead DMs\", so Approve always covers it to \
                    anyone. Reuse a listed always-approved task exactly."),
                "body": body(),
                "attachments": attachments(),
                "replaces": draft_id(),
            },
            "required": ["platform", "body"],
            "additionalProperties": false,
        },
        "annotations": write(),
    })
}

pub(super) fn confirm_approval_schema() -> Value {
    json!({
        "description": "Record that the owner's latest message approves the open draft exactly as it reads: \
            only when [else.bot outgoing drafts] marks it in_context=\"true\" and their words \
            clearly say to send it as is. A change request means revising with replaces. Then \
            send it: send_draft for a connected account, otherwise once through start_draft_send \
            or approved_draft, then report_draft_send.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "draft_id": draft_id(),
                "always": { "type": "string", "minLength": 1, "maxLength": 1000,
                            "description": "Only when the owner asks to always approve drafts \
                                like this: their exact words from that message. Grants what the \
                                card's Approve always would; rules grant nothing." },
            },
            "required": ["draft_id"],
            "additionalProperties": false,
        },
        "annotations": write(),
    })
}

pub(super) fn start_send_schema() -> Value {
    json!({
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
    })
}

pub(super) fn send_schema() -> Value {
    json!({
        "description": "Send an approved draft from its connected account: exactly the approved revision, \
            once, with the receipt on its card, so no report_draft_send. Calling it again in \
            this request returns the same send. Status sending means the outcome is not known \
            yet: call it again later, never mail_action.",
        "inputSchema": {
            "type": "object",
            "properties": { "draft_id": draft_id() },
            "required": ["draft_id"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": false,
                         "idempotentHint": true, "openWorldHint": true },
    })
}

pub(super) fn report_send_schema() -> Value {
    json!({
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
        "annotations": write(),
    })
}

fn draft_json(draft: &OutgoingDraft) -> Value {
    let mut value = json!({
        "draft_id": draft.id,
        "lineage_id": draft.lineage_id,
        "revision": draft.revision,
        "status": draft.status,
        "content": draft.content,
    });
    if draft.content.account().is_none() && !draft.content.files().is_empty() {
        value["attachment_paths"] = draft
            .content
            .files()
            .iter()
            .map(files::staged_path)
            .collect();
    }
    value
}

impl From<DraftError> for Refusal {
    fn from(err: DraftError) -> Self {
        let code = err.code();
        if let DraftError::Db(err) = &err {
            tracing::warn!(error = %err, "outgoing draft tool failed");
        }
        Refusal::coded(code, err)
    }
}

fn id_arg(args: &Value, key: &str) -> Result<Uuid, Refusal> {
    args[key]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Refusal::text(format!("{key} must be a draft id.")))
}

fn only(args: &Value, allowed: &[&str]) -> Result<(), Refusal> {
    if crate::app::args::only(args, allowed) {
        Ok(())
    } else {
        Err(Refusal::text(format!("use only {}.", allowed.join(", "))))
    }
}

pub(super) async fn draft_email(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    let account = sender(state, grant, args).await?;
    let email = Email::parse(args, account.as_deref()).map(Content::Email);
    draft(state, grant, args, email).await
}

pub(super) async fn draft_message(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    if grant.drafts_only() {
        drafts_only(state, args).await?;
    }
    let content = social_destination(state, grant, Message::parse(args)).await?;
    draft(state, grant, args, content).await
}

/// What a draft-only task may draft: a message through a native connection
/// to a chat that connection already lists, with no files. It has no host
/// tools to send an unconnected draft, and mail chose what it asks for, so
/// it may neither look up a new chat nor read the bot's files.
async fn drafts_only(state: &AppState, args: &Value) -> Result<(), Refusal> {
    let refused =
        |why: &str| Refusal::text(format!("INVALID_DRAFT: {why}")).logged("rejected INVALID_DRAFT");
    if args
        .get("attachments")
        .is_some_and(|files| !files.is_null())
    {
        return Err(refused("this task cannot attach files."));
    }
    let account = args["account_id"]
        .as_str()
        .and_then(|id| id.parse::<Uuid>().ok());
    let (Some(account), Some(destination)) = (account, args["destination"].as_str()) else {
        return Err(refused(
            "this task sends only through a native connection. Pass account_id and a \
             destination id from social_connections.",
        ));
    };
    let listed = crate::socials::store::destinations(&state.db, account)
        .await
        .map_err(|e| Refusal::text(e.to_string()))?;
    if !listed.iter().any(|chat| chat.id == destination) {
        return Err(refused(
            "name a chat by the id social_connections lists for that connection.",
        ));
    }
    Ok(())
}

/// A native social draft names its chat by ID, so the owner approves the
/// chat it reaches even if a public username later moves.
async fn social_destination(
    state: &AppState,
    grant: &Grant,
    message: Result<Message, &'static str>,
) -> Result<Result<Content, &'static str>, Refusal> {
    let Ok(mut message) = message else {
        return Ok(message.map(Content::Message));
    };
    if let (Some(account), Some(destination), socials) = (
        message.account_id,
        message.destination.as_mut(),
        &state.socials,
    ) {
        *destination = socials
            .resolve(account, grant.bot_id, grant.turn_id, destination)
            .await
            .map_err(|e| Refusal::text(e.to_string()))?;
    }
    Ok(Ok(Content::Message(message)))
}

async fn draft(
    state: &AppState,
    grant: &Grant,
    args: &Value,
    content: Result<Content, &'static str>,
) -> ToolResult {
    let content = content.map_err(|message| Refusal::text(format!("INVALID_DRAFT: {message}")))?;
    let sources = files::sources(args).map_err(Refusal::text)?;
    let content = if sources.is_empty() {
        content
    } else {
        let rejected = |message: String| {
            let code = message.split(':').next().unwrap_or_default().to_owned();
            Refusal::text(message).logged(format!("rejected {code}"))
        };
        let files = files::snapshot(state, grant, sources)
            .await
            .map_err(rejected)?;
        content
            .with_files(files)
            .map_err(|why| rejected(format!("INVALID_DRAFT: {why}")))?
    };
    let replaces = match &args["replaces"] {
        Value::Null => None,
        _ => Some(id_arg(args, "replaces")?),
    };
    let row = outgoing_drafts::propose(&state.db, grant.turn_id, &content, replaces).await?;
    if row.status == Status::Approved
        && row.approval_source == Some(ApprovalSource::Standing)
        && row.send_turn_id == Some(grant.turn_id)
    {
        return Ok(Done::json(json!({
            "status": "approved",
            "draft": draft_json(&row),
            "next": format!(
                "The owner approves {} like this always, so it is approved as drafted. {}",
                row.content.noun(),
                send_next(&row)
            ),
        }))
        .logged(format!(
            "{} {} approved always to {}",
            row.content.noun(),
            row.id,
            row.content.audience()
        )));
    }
    if row.status != Status::Pending {
        let log = format!("already {} {}", row.status.as_str(), row.id);
        return Ok(Done::json(json!({ "draft": draft_json(&row),
                "next": "You already drafted this in this request; follow its status." }))
        .logged(log));
    }
    state.grants.park(grant.turn_id).await;
    let log = format!(
        "drafted {} {} revision {} to {}",
        row.content.noun(),
        row.id,
        row.revision,
        row.content.audience()
    );
    Ok(Done::json(json!({
        "status": "pending",
        "parked": true,
        "draft": draft_json(&row),
        "next": "Tell the owner briefly that the draft is ready for review and end this \
                 attempt now. Do not send it, wait, poll, or call more tools; their decision \
                 arrives in a continuation.",
    }))
    .parked(true)
    .logged(log))
}

async fn sender(state: &AppState, grant: &Grant, args: &Value) -> Result<Option<String>, Refusal> {
    let Some(id) = args.get("account_id").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let Some(id) = id.as_str().and_then(|id| id.parse::<Uuid>().ok()) else {
        return Ok(None);
    };
    let forbidden = || {
        Refusal::text(
            "MAIL_FORBIDDEN: this task cannot send from that account. Use an account \
             mail_accounts lists with send permission, or omit account_id and send with your \
             own tools.",
        )
        .logged("rejected MAIL_FORBIDDEN")
    };
    if !grant.allows(Tool::Mail) {
        return Err(forbidden());
    }
    let (account, permissions) = tokio::join!(
        store::account(&state.db, id),
        store::permissions(&state.db, id, grant.bot_id)
    );
    let unavailable = |_| Refusal::text("UNAVAILABLE: mail accounts could not be read.");
    match (
        account.map_err(unavailable)?,
        permissions.map_err(unavailable)?,
    ) {
        (Some(account), Some(p))
            if account.state == AccountState::Active && p.read && p.write && p.send =>
        {
            Ok(Some(account.config.address))
        }
        _ => Err(forbidden()),
    }
}

/// How to send an approved draft, for the tool result that approved it.
fn send_next(row: &OutgoingDraft) -> String {
    if row.content.account().is_some() {
        format!(
            "Send exactly this {} with send_draft and this draft_id. It uses the connected \
             account and records the result; do not send with other tools or report_draft_send.",
            row.content.noun()
        )
    } else {
        format!(
            "Send exactly this {} now, once: with a computer tool declared \
                 effect external and approved_draft set to this draft_id, or with your host \
                 tools after start_draft_send. Then call report_draft_send.",
            row.content.noun()
        )
    }
}

pub(super) async fn confirm(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    only(args, &["draft_id", "always"])?;
    let id = id_arg(args, "draft_id")?;
    let always = match &args["always"] {
        Value::Null => None,
        Value::String(words) => Some(words.as_str()),
        _ => return Err(Refusal::text("always is the owner's exact words.")),
    };
    let row = outgoing_drafts::confirm(&state.db, grant.turn_id, id, always).await?;
    let mut result = json!({ "status": "approved", "draft": draft_json(&row) });
    let next = send_next(&row);
    let log = format!(
        "owner message approved {} {} version {}",
        row.content.noun(),
        row.id,
        row.version
    );
    let (next, log) = match (always, &row.kind) {
        (Some(_), Some(kind)) => {
            result["always"] = json!(kind);
            (
                format!(
                    "The owner now approves drafts matching always (same platform, account, and \
                     destination or task) as you propose them, except drafts with files. Tell \
                     them what it covers and that Stop approving always on the card ends it. {next}"
                ),
                format!("{log}, always"),
            )
        }
        _ => (next, log),
    };
    result["next"] = json!(next);
    Ok(Done::json(result).logged(log))
}

pub(super) async fn start(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    only(args, &["draft_id"])?;
    let id = id_arg(args, "draft_id")?;
    let row = outgoing_drafts::claim(&state.db, grant.turn_id, id, &Delivery::Host).await?;
    stage_claimed(state, &row).await?;
    Ok(Done::json(json!({
            "status": "sending",
            "draft": draft_json(&row),
            "next": format!("Send exactly this {}{} now with your host tools, once, then call \
                     report_draft_send with the result. Do not send it any other way or again.",
                     row.content.noun(),
                     if row.content.files().is_empty() { "" } else { " with the files at attachment_paths" }),
        })).logged(format!(
            "claimed {} {} for a host send",
            row.content.noun(),
            row.id
        )))
}

/// The connected accounts this grant's caller sends through.
struct Accounts<'a> {
    state: &'a AppState,
    grant: &'a Grant,
}

#[async_trait::async_trait]
impl Connected for Accounts<'_> {
    async fn send_mail(
        &self,
        account_id: Uuid,
        action_id: Uuid,
        command: MailCommand,
    ) -> Result<(), MailError> {
        self.state
            .connectors
            .execute(
                account_id,
                self.grant.bot_id,
                self.grant.turn_id,
                Some(action_id),
                command,
            )
            .await
            .map(drop)
    }

    async fn find_sent(&self, account_id: Uuid, message_id: String) -> Result<bool, MailError> {
        let found = self
            .state
            .connectors
            .execute(
                account_id,
                self.grant.bot_id,
                self.grant.turn_id,
                None,
                MailCommand::FindSent { message_id },
            )
            .await?;
        Ok(found["found"] == true)
    }

    async fn send_social(
        &self,
        account_id: Uuid,
        action_id: Uuid,
        platform: Platform,
        message: SendMessage,
    ) -> Result<(), socials::Error> {
        let connection = crate::socials::store::connection(&self.state.db, account_id).await?;
        let connected = match connection.provider {
            socials::Provider::Telegram => Platform::Telegram,
            socials::Provider::Discord => Platform::Discord,
            socials::Provider::WhatsApp => Platform::WhatsApp,
        };
        if platform != connected {
            return Err(socials::Error::Invalid(
                "The connection belongs to another platform.",
            ));
        }
        self.state
            .socials
            .send(
                account_id,
                self.grant.bot_id,
                self.grant.turn_id,
                action_id,
                message,
            )
            .await
            .map(drop)
    }
}

impl From<SendError> for Refusal {
    fn from(err: SendError) -> Self {
        match err {
            SendError::Draft(err) => err.into(),
            SendError::FileChanged(changed) => Refusal::text(format!(
                "FILE_CHANGED: {changed}. Tell the owner; draft it again for a new approval."
            ))
            .logged("rejected FILE_CHANGED"),
            SendError::FileChangedAfterClaim(changed) => Refusal::text(format!(
                "FILE_CHANGED: the attachment {} changed or is missing since the owner approved \
                 it. An earlier call already started this send and its result is not known; \
                 never send it again without the owner's new approval.",
                changed.0
            ))
            .logged("rejected FILE_CHANGED"),
        }
    }
}

pub(super) async fn send(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    only(args, &["draft_id"])?;
    let id = id_arg(args, "draft_id")?;
    let channels = Channels {
        mail: grant.allows(Tool::Mail) && grant.allows(Tool::DraftEmail),
        social: grant.allows(Tool::DraftMessage),
    };
    let row = send::send_connected(
        &state.db,
        grant.turn_id,
        id,
        channels,
        &Accounts { state, grant },
    )
    .await?;
    let next = match row.status {
        Status::Sent => "It was sent. Tell the owner briefly.",
        Status::Sending => {
            "The result is not known yet. Call send_draft again later in this request to read it; it never sends twice."
        }
        Status::Failed => "It was not sent. Tell the owner why; they can approve it again in chat.",
        _ if matches!(row.delivery.as_deref(), Some(Delivery::Mail { .. })) => {
            "It may have been sent. Tell the owner the detail; never send it again without their new approval."
        }
        _ => {
            "It may have been sent. Tell the owner to check the destination service; never send it again without their new approval."
        }
    };
    let medium = row.content.noun();
    let mut result = json!({
        "status": row.status,
        "draft_id": row.id,
        "medium": medium,
        "detail": row.result_detail,
        "next": next,
    });
    if matches!(row.delivery.as_deref(), Some(Delivery::Mail { .. })) {
        let attempts = outgoing_drafts::mail_attempts(&state.db, row.id)
            .await
            .map_err(DraftError::from)?;
        result["sends"] = json!(attempts);
    }
    Ok(Done::json(result).logged(format!(
        "{medium} {} {} through its connected account",
        row.id,
        row.status.as_str()
    )))
}

/// Stage the files of an unconnected draft this call just claimed for the
/// bot's own send; nothing was sent if that fails.
async fn stage_claimed(state: &AppState, row: &OutgoingDraft) -> Result<(), Refusal> {
    let mut conn = state.db.acquire().await.map_err(DraftError::from)?;
    let approved = send::approved(&mut conn, row).await;
    drop(conn);
    let approved = match approved {
        Ok(approved) => approved,
        Err(changed) => {
            outgoing_drafts::fail_unsent(&state.db, row.id, &changed.to_string()).await?;
            return Err(SendError::FileChanged(changed).into());
        }
    };
    if let Err(message) = files::stage(state, row.bot_id, &approved).await {
        outgoing_drafts::fail_unsent(&state.db, row.id, &message).await?;
        return Err(Refusal::text(message).logged("rejected FILE_UNAVAILABLE"));
    }
    Ok(())
}

pub(super) async fn report(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    only(args, &["draft_id", "status", "detail"])?;
    let id = id_arg(args, "draft_id")?;
    let outcome = args["status"]
        .as_str()
        .and_then(outgoing::Outcome::parse)
        .ok_or_else(|| Refusal::text("status is sent, failed, or uncertain."))?;
    let detail = outgoing::detail(args.get("detail"))
        .map_err(|message| Refusal::text(format!("INVALID_REPORT: {message}")))?;
    let (row, changed) =
        outgoing_drafts::report(&state.db, grant.turn_id, id, outcome, detail.as_deref()).await?;
    let noun = row.content.noun();
    let log = if changed {
        format!("{noun} {} {}", row.id, row.status.as_str())
    } else {
        format!("{noun} {} already {}", row.id, row.status.as_str())
    };
    Ok(Done::json(json!({ "status": row.status, "draft_id": row.id, "medium": noun })).logged(log))
}

pub(super) async fn claim_for_tool(
    state: &AppState,
    grant: &Grant,
    tool: &ToolDef,
    args: &Value,
) -> Result<String, Refusal> {
    let id = id_arg(args, "approved_draft")?;
    let delivery = Delivery::Tool {
        tool: tool.name.to_owned(),
        args: (tool.loggable)(args),
    };
    let row = outgoing_drafts::claim(&state.db, grant.turn_id, id, &delivery)
        .await
        .map_err(|err| {
            Refusal::coded(
                err.code(),
                format!(
                    "{err}. Nothing was done. approved_draft names a draft approved for this \
                     request and not yet sent."
                ),
            )
        })?;
    stage_claimed(state, &row).await?;
    Ok(format!(
        "sends approved {} {} version {}",
        row.content.noun(),
        row.id,
        row.version
    ))
}

mod files;
