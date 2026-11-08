use super::{
    AppState,
    mcp::{Outcome, tool_error, tool_result},
};
use crate::{
    app::questions::{Kind, Spec},
    grants::Grant,
    socials::{store, types::*},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub fn schemas() -> Vec<Value> {
    vec![
        json!({"name":"social_connect","description":"Open a private native Telegram or Discord connection card in this bot chat. Explain why access is needed and what the bot will do. The owner enters a bot token directly to the API; never ask for tokens in chat, request_secret, shell commands, or guest scripts. A bot token acts as a bot account, not the owner's personal account. This tool parks the attempt until the owner finishes or cancels.","inputSchema":{"type":"object","properties":{"provider":{"type":"string","enum":["telegram","discord"]},"reason":{"type":"string","minLength":1,"maxLength":500}},"required":["provider","reason"],"additionalProperties":false},"annotations":{"readOnlyHint":false}}),
        json!({"name":"social_connections","description":"List the native social connections this task may use, the chats each has seen, their \
            capabilities and limits, and your triggers. Call it before social operations. \
            Telegram history starts at connection; a chat appears once it messages the bot, or \
            once you use a public @username or t.me link. Tokens are never returned.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
        json!({"name":"social_trigger_set","description":"Wake yourself when a message arrives on a social connection, as the owner asked. \
            destination: a chat ID from social_connections, or a public Telegram @username or \
            t.me link; omit it for any chat. wake: mentions or all. instructions: what to do \
            when woken, in the owner's words. Setting a chat again replaces its trigger. When \
            woken you may read only that chat.","inputSchema":{"type":"object","properties":{"connection_id":{"type":"string","format":"uuid"},"destination":{"type":"string","maxLength":100},"wake":{"type":"string","enum":["mentions","all"]},"instructions":{"type":"string","minLength":1,"maxLength":4000}},"required":["connection_id","wake","instructions"],"additionalProperties":false},"annotations":{"readOnlyHint":false}}),
        json!({"name":"social_trigger_list","description":"List your own social triggers.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
        json!({"name":"social_trigger_delete","description":"Delete one of your own social triggers by ID when the owner asks you to stop watching a chat.","inputSchema":{"type":"object","properties":{"trigger_id":{"type":"string","format":"uuid"}},"required":["trigger_id"],"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true}}),
        json!({"name":"social_action","description":"Native Telegram or Discord operations on an allowed connection and destination. \
            Reads: conversations, history, search (observed messages only), message, chat, \
            members. Mutations (edit, delete, react, pin, unpin, create_thread, poll) need a \
            stable action_id and the owner's approval of the exact call. To send a message, use \
            draft_message, then send_draft.","inputSchema":{"type":"object","properties":{"connection_id":{"type":"string","format":"uuid"},"action_id":{"type":"string","format":"uuid"},"command":{"type":"object","properties":{"operation":{"type":"string","enum":OPERATIONS},"destination":{"type":"string"},"message_id":{"type":"string"},"before":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100},"query":{"type":"string","maxLength":500},"body":{"type":"string","maxLength":4000},"emoji":{"type":"string","maxLength":100},"name":{"type":"string","maxLength":100},"question":{"type":"string","maxLength":300},"options":{"type":"array","items":{"type":"string"},"minItems":2,"maxItems":10}},"required":["operation"],"additionalProperties":false}},"required":["connection_id","command"],"additionalProperties":false},"annotations":{"readOnlyHint":false}}),
        json!({"name":"social_action_status","description":"Read the durable result of a social mutation. An uncertain or dispatching action must never be blindly repeated with a new ID.","inputSchema":{"type":"object","properties":{"connection_id":{"type":"string","format":"uuid"},"action_id":{"type":"string","format":"uuid"}},"required":["connection_id","action_id"],"additionalProperties":false},"annotations":{"readOnlyHint":true}}),
    ]
}
fn refused(e: Error) -> Outcome {
    (tool_error(&e.to_string()), None)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connect {
    provider: Provider,
    reason: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    connection_id: Uuid,
    #[serde(default)]
    action_id: Option<Uuid>,
    command: Command,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Status {
    connection_id: Uuid,
    action_id: Uuid,
}
pub async fn call(s: &AppState, g: &Grant, name: &str, args: &Value) -> Outcome {
    let result = match name {
        "social_connect" => connect(s, g, args).await,
        "social_connections" => connections(s, g, args).await,
        "social_action" => action(s, g, args).await,
        "social_action_status" => status(s, g, args).await,
        "social_trigger_set" => set_trigger(s, g, args).await,
        "social_trigger_list" => list_triggers(s, g, args).await,
        "social_trigger_delete" => delete_trigger(s, g, args).await,
        _ => Err(Error::Unsupported),
    };
    result
        .map(|value| (value, Some(format!("native social tool {name}"))))
        .unwrap_or_else(refused)
}

async fn connect(s: &AppState, g: &Grant, args: &Value) -> Result<Value, Error> {
    let input: Connect = serde_json::from_value(args.clone())
        .map_err(|_| Error::Invalid("Use provider and reason only. Never pass a token."))?;
    let setup = store::request_setup(&s.db, g.turn_id, input.provider, &input.reason).await?;
    let pending = setup.status == "pending";
    if pending {
        s.scheduler.grants().park(g.turn_id).await;
    }
    Ok(tool_result(json!({
        "setup": setup,
        "parked": pending,
        "next": if pending {
            "The owner sees the native connection card. Explain what the token connects and the intended access, then end this attempt. Never request a token in chat."
        } else {
            "The owner settled this setup. Discover social_connections to inspect current access and instructions."
        }
    })))
}

async fn connections(s: &AppState, g: &Grant, args: &Value) -> Result<Value, Error> {
    if !args.as_object().is_some_and(|a| a.is_empty()) {
        return Err(Error::Invalid("social_connections takes no arguments."));
    }
    let triggers = store::triggers(&mut *s.db.acquire().await?, g.bot_id).await?;
    let mut list = Vec::new();
    for c in store::connections(&s.db).await? {
        let access = {
            let mut conn = s.db.acquire().await?;
            match store::authorize(&mut conn, &c, g.bot_id, g.turn_id, None, false, false).await {
                Ok(access) => access,
                Err(Error::Forbidden) => continue,
                Err(error) => return Err(error),
            }
        };
        let destinations = store::destinations(&s.db, c.id)
            .await?
            .into_iter()
            .filter(|d| access.allows(&d.id))
            .collect::<Vec<_>>();
        let capabilities = OPERATIONS
            .iter()
            .map(|op| {
                let supported = c.provider.supports(*op);
                json!({
                    "operation": op,
                    "supported": supported,
                    "granted": supported && (!op.mutates() || access.mutates()),
                    "approval_required": op.mutates(),
                    "platform_permissions": "checked_by_provider_at_execution"
                })
            })
            .collect::<Vec<_>>();
        let no_chats = destinations.is_empty().then_some(
            "No chats seen yet. A chat appears once it sends the bot a message. Use a public Telegram channel or group by its @username or t.me link as the destination; it is then listed with can_send, which is true when the bot may post there.",
        );
        let own: Vec<&Trigger> = triggers
            .iter()
            .filter(|t| t.connection_id == c.id)
            .collect();
        list.push(json!({
            "connection": c, "destinations": destinations, "no_destinations": no_chats,
            "your_triggers": own, "capabilities": capabilities,
            "limitations": c.provider.limitations(),
            "sending": "draft_message with account_id then exact approved send_draft"
        }));
    }
    Ok(tool_result(json!({"connections": list})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteTrigger {
    trigger_id: Uuid,
}

async fn set_trigger(s: &AppState, g: &Grant, args: &Value) -> Result<Value, Error> {
    let mut input: TriggerInput = serde_json::from_value(args.clone()).map_err(|_| {
        Error::Invalid(
            "Use connection_id, wake (mentions or all), instructions, and optional destination.",
        )
    })?;
    input.validate()?;
    if let (Some(destination), Some(m)) = (input.destination.as_mut(), s.socials.as_ref()) {
        *destination = m
            .resolve(input.connection_id, g.bot_id, g.turn_id, destination)
            .await?;
    }
    let change = store::set_trigger(&s.db, g.turn_id, &input).await?;
    reconnect(s, &change).await;
    Ok(tool_result(json!({
        "trigger": change.value,
        "next": "Tell the owner what will wake you and what you will do. They can pause or delete it in your sidebar."
    })))
}

async fn list_triggers(s: &AppState, g: &Grant, args: &Value) -> Result<Value, Error> {
    if !args.as_object().is_some_and(|a| a.is_empty()) {
        return Err(Error::Invalid("social_trigger_list takes no arguments."));
    }
    let triggers = store::triggers(&mut *s.db.acquire().await?, g.bot_id).await?;
    Ok(tool_result(json!({ "triggers": triggers })))
}

async fn delete_trigger(s: &AppState, g: &Grant, args: &Value) -> Result<Value, Error> {
    let input: DeleteTrigger =
        serde_json::from_value(args.clone()).map_err(|_| Error::Invalid("Use trigger_id only."))?;
    let change = store::delete_trigger(&s.db, g.bot_id, input.trigger_id).await?;
    reconnect(s, &change).await;
    Ok(tool_result(json!({ "deleted": input.trigger_id })))
}

/// Restart the connection's listener only when what it receives changed.
pub(super) async fn reconnect<T>(s: &AppState, change: &store::TriggerChange<T>) {
    if change.reconnect
        && let Some(m) = s.socials.as_ref()
    {
        m.changed(change.connection).await;
    }
}

async fn action(s: &AppState, g: &Grant, args: &Value) -> Result<Value, Error> {
    let mut input: Action = serde_json::from_value(args.clone())
        .map_err(|_| Error::Invalid("Use connection_id, command, and action_id for a mutation."))?;
    input.command.validate()?;
    let manager = s.socials.as_ref().ok_or(Error::Crypto)?;
    let mut args = args.clone();
    if let Some(destination) = input.command.destination_mut() {
        *destination = manager
            .resolve(input.connection_id, g.bot_id, g.turn_id, destination)
            .await?;
        args["command"]["destination"] = json!(destination);
    }
    let args = &args;
    if input.command.mutates() {
        if input.action_id.is_none() {
            return Err(Error::Invalid("Mutations require a stable action_id."));
        }
        let connection = store::connection(&s.db, input.connection_id).await?;
        if !connection.provider.supports(input.command.operation()) {
            return Err(Error::Unsupported);
        }
        store::authorize(
            &mut *s.db.acquire().await?,
            &connection,
            g.bot_id,
            g.turn_id,
            input.command.destination(),
            true,
            input.command.sends(),
        )
        .await?;
        let kind = if matches!(input.command, Command::Delete { .. }) {
            Kind::Destructive
        } else {
            Kind::External
        };
        let spec = Spec::tool_approval(
            kind,
            "social_action",
            args,
            format!("{} social action", connection.provider.label()),
            serde_json::to_string(&input.command).map_err(|_| Error::Conflict)?,
        );
        match crate::db::owner_questions::gate(&s.db, g.turn_id, &spec)
            .await
            .map_err(|_| Error::Forbidden)?
        {
            crate::db::owner_questions::Gate::Pending(question) => {
                return Ok(super::owner_tools::pending_approval(s, g, &question,
                    "The owner sees an approval card for this exact social action. End this attempt. If approved, call social_action with exactly the same arguments once."
                ).await.0);
            }
            crate::db::owner_questions::Gate::Denied(_) => return Err(Error::Forbidden),
            crate::db::owner_questions::Gate::Run { .. } => {}
        }
    }
    Ok(tool_result(
        manager
            .execute(
                input.connection_id,
                g.bot_id,
                g.turn_id,
                input.action_id,
                input.command,
            )
            .await?,
    ))
}

async fn status(s: &AppState, g: &Grant, args: &Value) -> Result<Value, Error> {
    let input: Status = serde_json::from_value(args.clone())
        .map_err(|_| Error::Invalid("Use connection_id and action_id."))?;
    let connection = store::connection(&s.db, input.connection_id).await?;
    let row: Option<(String, String, Option<sqlx::types::Json<Value>>)> = sqlx::query_as(
        "SELECT destination,status,result FROM social_actions WHERE connection_id=?1 AND action_id=?2 AND bot_id=?3",
    ).bind(input.connection_id).bind(input.action_id).bind(g.bot_id).fetch_optional(&s.db).await?;
    let (destination, status, result) = row.ok_or(Error::NotFound)?;
    store::authorize(
        &mut *s.db.acquire().await?,
        &connection,
        g.bot_id,
        g.turn_id,
        Some(&destination),
        false,
        false,
    )
    .await?;
    Ok(tool_result(
        json!({"action_id": input.action_id, "status": status, "result": result.map(|x| x.0)}),
    ))
}
