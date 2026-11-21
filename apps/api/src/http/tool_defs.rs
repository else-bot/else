//! Every else.bot tool, defined once. A row names the grant the tool needs,
//! its schema, whether a turn lists it, whether it may park the turn, which
//! calls must declare an effect, what of its arguments the activity log
//! keeps, and the handler that runs it. A name with no row is not a tool.
//!
//! Rows are in listed order, then deferred order: provider adapters hash
//! and allowlist [`LISTED`], `tools/list` follows it, and `find_tool` breaks
//! ties by it, so moving a listed row rebuilds every native session and
//! invalidates every provider's prompt cache.

use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use super::AppState;
use super::tool_result::{Done, ToolResult};
use super::tool_search::{FIND, USE};
use super::{
    bot_tools, connectors, delegation_tools, guest_tools, mcp_wait, memory_tools, outgoing_tools,
    owner_tools, routines, rules, shared_mcp, skills, social_tools, visual_tools, webhooks,
};
use crate::grants::{Grant, Tool};

/// One call to a tool's handler.
#[derive(Clone, Copy)]
pub(crate) struct Call<'a> {
    pub state: &'a AppState,
    pub grant: &'a Grant,
    pub args: &'a Value,
    pub tool: &'static ToolDef,
}

type Run = for<'a> fn(Call<'a>) -> BoxFuture<'a, ToolResult>;

#[derive(Clone, Copy)]
pub(crate) enum Listing {
    /// In every turn's tool list. Only these may park a turn: providers
    /// recognize parking by the name of the tool they called, which for a
    /// deferred tool is `use_tool`.
    Listed { parks: bool },
    /// Reached through `find_tool` and `use_tool`.
    Deferred,
}

/// Which calls must say what they do before approval.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Effect {
    /// The schema declares no effect, and a call that gives one is refused.
    Undeclared,
    /// Every call declares one.
    Required,
    /// Declared; required for a browser click, typing, or accepting a dialog.
    BrowserAction,
}

impl Effect {
    pub(crate) fn declared(self) -> bool {
        self != Self::Undeclared
    }

    pub(crate) fn required(self, args: &Value) -> bool {
        match self {
            Self::Undeclared => false,
            Self::Required => true,
            Self::BrowserAction => {
                matches!(args["action"].as_str(), Some("click" | "type"))
                    || args["dialog"] == "accept"
            }
        }
    }
}

pub(crate) struct ToolDef {
    pub name: &'static str,
    pub needs: Tool,
    /// The tool's MCP definition without its name: description, input
    /// schema, and annotations.
    schema: fn() -> Value,
    pub listing: Listing,
    pub effect: Effect,
    /// One standing owner grant per tool and effect may cover its calls.
    pub standing: bool,
    pub loggable: fn(&Value) -> Value,
    pub run: Run,
}

impl ToolDef {
    const fn listed(name: &'static str, needs: Tool, schema: fn() -> Value, run: Run) -> Self {
        Self::new(name, needs, schema, Listing::Listed { parks: false }, run)
    }

    const fn deferred(name: &'static str, needs: Tool, schema: fn() -> Value, run: Run) -> Self {
        Self::new(name, needs, schema, Listing::Deferred, run)
    }

    const fn new(
        name: &'static str,
        needs: Tool,
        schema: fn() -> Value,
        listing: Listing,
        run: Run,
    ) -> Self {
        Self {
            name,
            needs,
            schema,
            listing,
            effect: Effect::Undeclared,
            standing: false,
            loggable: Value::clone,
            run,
        }
    }

    const fn parking(mut self) -> Self {
        match self.listing {
            Listing::Listed { .. } => self.listing = Listing::Listed { parks: true },
            Listing::Deferred => panic!("a deferred tool cannot park"),
        }
        self
    }

    const fn effect(mut self, effect: Effect) -> Self {
        self.effect = effect;
        self
    }

    const fn standing(mut self) -> Self {
        self.standing = true;
        self
    }

    const fn logs(mut self, loggable: fn(&Value) -> Value) -> Self {
        self.loggable = loggable;
        self
    }

    /// The tool's MCP definition, its name first.
    pub(crate) fn definition(&self) -> Value {
        let mut definition = serde_json::Map::from_iter([("name".into(), json!(self.name))]);
        let Value::Object(schema) = (self.schema)() else {
            unreachable!("{}'s schema is an object", self.name)
        };
        definition.extend(schema.into_iter().filter(|(key, _)| key != "name"));
        Value::Object(definition)
    }

    pub(crate) fn parks(&self) -> bool {
        matches!(self.listing, Listing::Listed { parks: true })
    }

    pub(crate) fn run<'a>(
        &'static self,
        state: &'a AppState,
        grant: &'a Grant,
        args: &'a Value,
    ) -> BoxFuture<'a, ToolResult> {
        (self.run)(Call {
            state,
            grant,
            args,
            tool: self,
        })
    }
}

pub(crate) fn get(name: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|tool| tool.name == name)
}

/// Every tool's definition, listed then deferred, in row order.
pub(crate) fn definitions() -> Vec<Value> {
    TOOLS.iter().map(ToolDef::definition).collect()
}

/// Annotations for a tool that only reads else.bot's own state.
pub(super) fn read_only() -> Value {
    json!({ "readOnlyHint": true, "destructiveHint": false,
            "idempotentHint": true, "openWorldHint": false })
}

/// What the activity log and approval records keep of a call's arguments.
pub(crate) fn loggable_args(name: &str, args: &Value) -> Value {
    get(name).map_or_else(|| args.clone(), |tool| (tool.loggable)(args))
}

/// The tools a turn lists, in the order adapters hash, then the two search
/// tools.
pub(crate) const LISTED: [&str; count(Select::Listed) + 2] = {
    let mut names = names::<{ count(Select::Listed) + 2 }>(Select::Listed);
    names[names.len() - 2] = FIND;
    names[names.len() - 1] = USE;
    names
};
pub(crate) const DEFERRED: [&str; count(Select::Deferred)] =
    names::<{ count(Select::Deferred) }>(Select::Deferred);
pub(crate) const PARKING: [&str; count(Select::Parking)] =
    names::<{ count(Select::Parking) }>(Select::Parking);

#[derive(Clone, Copy)]
enum Select {
    Listed,
    Deferred,
    Parking,
}

const fn selects(select: Select, tool: &ToolDef) -> bool {
    matches!(
        (select, tool.listing),
        (Select::Listed, Listing::Listed { .. })
            | (Select::Deferred, Listing::Deferred)
            | (Select::Parking, Listing::Listed { parks: true })
    )
}

const fn count(select: Select) -> usize {
    let (mut i, mut n) = (0, 0);
    while i < TOOLS.len() {
        if selects(select, &TOOLS[i]) {
            n += 1;
        }
        i += 1;
    }
    n
}

/// The selected names in row order; slots past them stay empty.
const fn names<const N: usize>(select: Select) -> [&'static str; N] {
    let mut out = [""; N];
    let (mut i, mut n) = (0, 0);
    while i < TOOLS.len() {
        if selects(select, &TOOLS[i]) {
            out[n] = TOOLS[i].name;
            n += 1;
        }
        i += 1;
    }
    out
}

fn by_id(args: &Value) -> Value {
    json!({ "id": args["id"] })
}

fn nothing(_: &Value) -> Value {
    json!({})
}

fn social_action(args: &Value) -> Value {
    json!({
        "connection_id": args["connection_id"],
        "action_id": args["action_id"],
        "operation": args["command"]["operation"],
    })
}

fn browser(args: &Value) -> Value {
    let mut safe = args.clone();
    if args["action"] == "type"
        && let Some(text) = args["text"].as_str()
    {
        safe["text"] = Value::String(format!("type({} chars)", text.chars().count()));
    }
    safe
}

fn input(args: &Value) -> Value {
    json!({
        "frame": args["frame"],
        "actions": crate::app::tools::summarize_input(
            args["actions"].as_array().map(Vec::as_slice).unwrap_or_default()
        ),
    })
}

const TOOLS: &[ToolDef] = &[
    ToolDef::listed("mcp_list", Tool::Mcp, shared_mcp::list_schema, |c| {
        Box::pin(shared_mcp::call(c, shared_mcp::Op::List))
    }),
    ToolDef::listed(
        "mcp_install",
        Tool::ManageMcp,
        shared_mcp::install_schema,
        |c| Box::pin(shared_mcp::call(c, shared_mcp::Op::Install)),
    )
    .parking()
    .logs(|args| json!({ "id": args["id"], "name": args["name"] })),
    ToolDef::listed("mcp_request", Tool::Mcp, shared_mcp::request_schema, |c| {
        Box::pin(shared_mcp::call(c, shared_mcp::Op::Request))
    })
    .parking()
    .logs(|args| {
        json!({
            "id": args["id"],
            "method": args["method"],
            "tool": args["params"]["name"],
            "effect": args["effect"],
        })
    }),
    ToolDef::listed(mcp_wait::NAME, Tool::Mcp, mcp_wait::schema, |c| {
        Box::pin(mcp_wait::call(c.state, c.grant, c.args))
    })
    .parking()
    .logs(|args| json!({ "id": args["id"], "tool": args["tool"] })),
    ToolDef::listed("mcp_connect", Tool::Mcp, shared_mcp::connect_schema, |c| {
        Box::pin(shared_mcp::call(c, shared_mcp::Op::Connect))
    })
    .parking()
    .logs(by_id),
    ToolDef::listed(
        skills::INSTALL,
        Tool::ManageMcp,
        skills::install_schema,
        |c| Box::pin(skills::install(c.state, c.grant, c.args)),
    )
    .parking(),
    ToolDef::listed(skills::READ, Tool::Mcp, skills::read_schema, |c| {
        Box::pin(skills::read(c.state, c.grant, c.args))
    }),
    ToolDef::listed(
        "memory_bots",
        Tool::ReadMemory,
        memory_tools::bots_schema,
        |c| Box::pin(memory_tools::bots(c.state, c.grant)),
    ),
    ToolDef::listed(
        "memory_search",
        Tool::ReadMemory,
        memory_tools::search_schema,
        |c| Box::pin(memory_tools::search(c.state, c.grant, c.args)),
    ),
    ToolDef::listed(
        "memory_read",
        Tool::ReadMemory,
        memory_tools::read_schema,
        |c| Box::pin(memory_tools::read(c.state, c.grant, c.args)),
    ),
    ToolDef::listed(
        "memory_view",
        Tool::ReadMemory,
        memory_tools::view_schema,
        |c| Box::pin(memory_tools::view(c.state, c.grant, c.args)),
    ),
    ToolDef::listed(
        "delegate",
        Tool::Delegate,
        delegation_tools::delegate_schema,
        |c| Box::pin(delegation_tools::delegate(c.state, c.grant, c.args)),
    )
    .parking(),
    ToolDef::listed(
        "council",
        Tool::Delegate,
        delegation_tools::council_schema,
        |c| Box::pin(delegation_tools::council(c.state, c.grant, c.args)),
    )
    .parking(),
    ToolDef::listed(
        "request_secret",
        Tool::RequestSecret,
        owner_tools::secret_schema,
        |c| Box::pin(owner_tools::request_secret(c.state, c.grant, c.args)),
    )
    .parking(),
    ToolDef::listed(
        "ask_owner",
        Tool::AskOwner,
        owner_tools::choice_schema,
        |c| Box::pin(owner_tools::ask_owner(c.state, c.grant, c.args)),
    )
    .parking(),
    ToolDef::listed(
        "ask_permission",
        Tool::AskPermission,
        owner_tools::permission_schema,
        |c| Box::pin(owner_tools::ask_permission(c.state, c.grant, c.args)),
    )
    .parking(),
    ToolDef::listed("delete_bot", Tool::EditBot, bot_tools::delete_schema, |c| {
        Box::pin(bot_tools::delete_bot(c.state, c.grant, c.args))
    })
    .parking(),
    ToolDef::listed(
        "draft_email",
        Tool::DraftEmail,
        outgoing_tools::email_schema,
        |c| Box::pin(outgoing_tools::call(c, outgoing_tools::Op::DraftEmail)),
    )
    .parking(),
    ToolDef::listed(
        "draft_message",
        Tool::AskOwner,
        outgoing_tools::message_schema,
        |c| Box::pin(outgoing_tools::call(c, outgoing_tools::Op::DraftMessage)),
    )
    .parking(),
    ToolDef::listed(
        "webhook_create",
        Tool::Routines,
        webhooks::create_schema,
        |c| Box::pin(webhooks::create(c.state, c.grant, c.args)),
    )
    .parking(),
    ToolDef::listed(
        "mail_trigger_set",
        Tool::Routines,
        connectors::trigger_set_schema,
        |c| Box::pin(connectors::call(c, connectors::Op::TriggerSet)),
    )
    .parking(),
    ToolDef::listed(
        "webhook_update",
        Tool::Routines,
        webhooks::update_schema,
        |c| Box::pin(webhooks::update(c.state, c.grant, c.args)),
    )
    .parking(),
    ToolDef::listed(
        "social_connect",
        Tool::AskOwner,
        social_tools::connect_schema,
        |c| Box::pin(social_tools::call(c, social_tools::Op::Connect)),
    )
    .parking()
    .logs(|args| json!({ "provider": args["provider"], "reason": args["reason"] })),
    ToolDef::listed(
        "social_action",
        Tool::Social,
        social_tools::action_schema,
        |c| Box::pin(social_tools::call(c, social_tools::Op::Action)),
    )
    .parking()
    .logs(social_action),
    ToolDef::listed(
        "computer_run",
        Tool::Computer,
        guest_tools::run_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::Run)),
    )
    .parking()
    .effect(Effect::Required)
    .standing(),
    ToolDef::listed(
        "computer_job_start",
        Tool::Computer,
        guest_tools::job_start_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::JobStart)),
    )
    .parking()
    .effect(Effect::Required)
    .standing(),
    ToolDef::listed(
        "computer_job",
        Tool::Computer,
        guest_tools::job_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::Job)),
    )
    .parking()
    .standing(),
    ToolDef::listed(
        "computer_watch_start",
        Tool::Computer,
        guest_tools::watch::start_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::WatchStart)),
    )
    .standing(),
    ToolDef::listed(
        "computer_write_file",
        Tool::Computer,
        guest_tools::write_file_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::WriteFile)),
    )
    .parking()
    .effect(Effect::Required)
    .standing(),
    ToolDef::listed(
        "computer_read_file",
        Tool::Computer,
        guest_tools::read_file_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::ReadFile)),
    )
    .standing(),
    ToolDef::listed(
        "computer_fetch",
        Tool::Computer,
        guest_tools::fetch_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::Fetch)),
    )
    .standing(),
    ToolDef::listed(
        "computer_browser",
        Tool::Computer,
        guest_tools::browser_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::Browser)),
    )
    .parking()
    .effect(Effect::BrowserAction)
    .standing()
    .logs(browser),
    ToolDef::listed(
        "computer_screenshot",
        Tool::Computer,
        guest_tools::screenshot_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::Screenshot)),
    )
    .standing(),
    ToolDef::listed(
        "computer_input",
        Tool::Computer,
        guest_tools::input_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::Input)),
    )
    .parking()
    .effect(Effect::Required)
    .standing()
    .logs(input),
    // A resize takes host memory, so only a grant for its size covers it.
    ToolDef::listed(
        "computer_resize",
        Tool::Computer,
        guest_tools::resize_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::Resize)),
    )
    .parking(),
    ToolDef::listed(
        "computer_handoff",
        Tool::Computer,
        owner_tools::handoff_schema,
        |c| Box::pin(owner_tools::computer_handoff(c.state, c.grant, c.args)),
    )
    .parking(),
    // Every turn may read its history; the tool itself admits only the turn
    // it serves: a running delegated task or routine run.
    ToolDef::listed(
        "notify_owner",
        Tool::ReadHistory,
        owner_tools::notice_schema,
        |c| Box::pin(owner_tools::notify_owner(c.state, c.grant, c.args)),
    ),
    ToolDef::listed(
        "mail_accounts",
        Tool::Mail,
        connectors::accounts_schema,
        |c| Box::pin(connectors::call(c, connectors::Op::Accounts)),
    )
    .logs(nothing),
    ToolDef::listed(
        "mail_connect",
        Tool::AskOwner,
        connectors::connect_schema,
        |c| Box::pin(connectors::call(c, connectors::Op::Connect)),
    )
    .parking()
    .logs(|args| json!({ "provider": args["provider"] })),
    ToolDef::listed(
        "mail_disconnect",
        Tool::AskOwner,
        connectors::disconnect_schema,
        |c| Box::pin(connectors::call(c, connectors::Op::Disconnect)),
    )
    .parking()
    .logs(|args| json!({ "account_id": args["account_id"] })),
    ToolDef::listed("mail_action", Tool::Mail, connectors::action_schema, |c| {
        Box::pin(connectors::call(c, connectors::Op::Action))
    })
    .parking()
    .logs(|args| {
        json!({
            "account_id": args["account_id"],
            "action_id": args["action_id"],
            "operation": args["command"]["operation"],
        })
    }),
    ToolDef::listed(
        "mail_action_status",
        Tool::Mail,
        connectors::action_status_schema,
        |c| Box::pin(connectors::call(c, connectors::Op::ActionStatus)),
    ),
    ToolDef::listed(
        "social_connections",
        Tool::Social,
        social_tools::connections_schema,
        |c| Box::pin(social_tools::call(c, social_tools::Op::Connections)),
    )
    .logs(nothing),
    ToolDef::listed(
        "social_action_status",
        Tool::Social,
        social_tools::action_status_schema,
        |c| Box::pin(social_tools::call(c, social_tools::Op::ActionStatus)),
    )
    .logs(social_action),
    ToolDef::listed("rules_update", Tool::Rules, rules::schema, |c| {
        Box::pin(rules::update(c.state, c.grant, c.args))
    }),
    // Previews read the project, which only turns with the computer may do.
    ToolDef::listed(
        visual_tools::NAME,
        Tool::Computer,
        visual_tools::schema,
        |c| Box::pin(visual_tools::call(c.state, c.grant, c.args)),
    ),
    ToolDef::deferred(
        "mcp_connection_status",
        Tool::Mcp,
        shared_mcp::connection_status_schema,
        |c| Box::pin(shared_mcp::call(c, shared_mcp::Op::ConnectionStatus)),
    )
    .logs(by_id),
    ToolDef::deferred(
        "memory_zoom",
        Tool::ReadMemory,
        memory_tools::zoom_schema,
        |c| Box::pin(memory_tools::zoom(c.state, c.grant, c.args)),
    ),
    ToolDef::deferred(
        "memory_date",
        Tool::ReadMemory,
        memory_tools::date_schema,
        |c| Box::pin(memory_tools::date(c.state, c.grant, c.args)),
    ),
    ToolDef::deferred("else_status", Tool::Diagnose, super::status_schema, |c| {
        Box::pin(async move { Ok(Done::json(super::status(c.state).await)) })
    }),
    ToolDef::deferred("list_bots", Tool::Delegate, bot_tools::list_schema, |c| {
        Box::pin(bot_tools::list_bots(c.state, c.grant, c.args))
    }),
    ToolDef::deferred(
        "create_bot",
        Tool::CreateBot,
        bot_tools::create_schema,
        |c| Box::pin(bot_tools::create_bot(c.state, c.grant, c.args)),
    ),
    ToolDef::deferred(
        "delegation_update",
        Tool::Delegate,
        delegation_tools::update_schema,
        |c| Box::pin(delegation_tools::update(c.state, c.grant, c.args)),
    ),
    ToolDef::deferred(
        "delegation_cancel",
        Tool::Delegate,
        delegation_tools::cancel_schema,
        |c| Box::pin(delegation_tools::cancel(c.state, c.grant, c.args)),
    ),
    ToolDef::deferred("rename_bot", Tool::EditBot, bot_tools::rename_schema, |c| {
        Box::pin(bot_tools::rename_bot(c.state, c.grant, c.args))
    }),
    ToolDef::deferred(
        "set_bot_model",
        Tool::EditBot,
        bot_tools::model_schema,
        |c| Box::pin(bot_tools::set_bot_model(c.state, c.grant, c.args)),
    ),
    ToolDef::deferred(
        "confirm_permission",
        Tool::AskOwner,
        owner_tools::confirm_permission_schema,
        |c| Box::pin(owner_tools::confirm_permission(c.state, c.grant, c.args)),
    ),
    ToolDef::deferred(
        "confirm_draft_approval",
        Tool::AskOwner,
        outgoing_tools::confirm_approval_schema,
        |c| Box::pin(outgoing_tools::call(c, outgoing_tools::Op::ConfirmApproval)),
    ),
    ToolDef::deferred(
        "start_draft_send",
        Tool::AskOwner,
        outgoing_tools::start_send_schema,
        |c| Box::pin(outgoing_tools::call(c, outgoing_tools::Op::StartSend)),
    ),
    // Sending also needs the draft's channel: Mail or Social.
    ToolDef::deferred(
        "send_draft",
        Tool::DraftEmail,
        outgoing_tools::send_schema,
        |c| Box::pin(outgoing_tools::call(c, outgoing_tools::Op::Send)),
    ),
    ToolDef::deferred(
        "report_draft_send",
        Tool::AskOwner,
        outgoing_tools::report_send_schema,
        |c| Box::pin(outgoing_tools::call(c, outgoing_tools::Op::ReportSend)),
    ),
    ToolDef::deferred(
        "routine_create",
        Tool::Routines,
        routines::create_schema,
        |c| Box::pin(routines::create(c.state, c.grant, c.args)),
    ),
    ToolDef::deferred("routine_list", Tool::Routines, routines::list_schema, |c| {
        Box::pin(routines::list(c.state, c.grant))
    }),
    ToolDef::deferred(
        "routine_update",
        Tool::Routines,
        routines::update_schema,
        |c| Box::pin(routines::update(c.state, c.grant, c.args)),
    ),
    ToolDef::deferred(
        "routine_delete",
        Tool::Routines,
        routines::delete_schema,
        |c| Box::pin(routines::delete(c.state, c.grant, c.args)),
    ),
    ToolDef::deferred("routine_run", Tool::Routines, routines::run_schema, |c| {
        Box::pin(routines::run(c.state, c.grant, c.args))
    }),
    ToolDef::deferred(
        "reminder_create",
        Tool::Routines,
        routines::reminder_schema,
        |c| Box::pin(routines::create_reminder(c.state, c.grant, c.args)),
    ),
    // Like notify_owner: the tool admits only a running routine run.
    ToolDef::deferred(
        routines::NO_NEWS,
        Tool::ReadHistory,
        routines::no_news_schema,
        |c| Box::pin(routines::no_news(c.state, c.grant)),
    ),
    ToolDef::deferred("webhook_list", Tool::Routines, webhooks::list_schema, |c| {
        Box::pin(webhooks::list(c.state, c.grant))
    }),
    ToolDef::deferred(
        "webhook_delete",
        Tool::Routines,
        webhooks::delete_schema,
        |c| Box::pin(webhooks::delete(c.state, c.grant, c.args)),
    ),
    ToolDef::deferred(
        "social_trigger_set",
        Tool::Routines,
        social_tools::trigger_set_schema,
        |c| Box::pin(social_tools::call(c, social_tools::Op::TriggerSet)),
    ),
    ToolDef::deferred(
        "social_trigger_list",
        Tool::Routines,
        social_tools::trigger_list_schema,
        |c| Box::pin(social_tools::call(c, social_tools::Op::TriggerList)),
    ),
    ToolDef::deferred(
        "social_trigger_delete",
        Tool::Routines,
        social_tools::trigger_delete_schema,
        |c| Box::pin(social_tools::call(c, social_tools::Op::TriggerDelete)),
    ),
    ToolDef::deferred(
        "mail_trigger_delete",
        Tool::Routines,
        connectors::trigger_delete_schema,
        |c| Box::pin(connectors::call(c, connectors::Op::TriggerDelete)),
    ),
    ToolDef::deferred(
        "computer_job_cancel",
        Tool::Computer,
        guest_tools::job_cancel_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::JobCancel)),
    )
    .standing(),
    ToolDef::deferred(
        "computer_watches",
        Tool::Computer,
        guest_tools::watch::list_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::Watches)),
    )
    .standing(),
    ToolDef::deferred(
        "computer_watch_stop",
        Tool::Computer,
        guest_tools::watch::stop_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::WatchStop)),
    )
    .standing(),
    ToolDef::deferred(
        "computer_secrets",
        Tool::Computer,
        guest_tools::secrets_schema,
        |c| Box::pin(guest_tools::call(c, guest_tools::Op::Secrets)),
    )
    .standing(),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_tool_has_one_row() {
        for (i, tool) in TOOLS.iter().enumerate() {
            let rows = TOOLS[i..].iter().filter(|t| t.name == tool.name).count();
            assert_eq!(rows, 1, "{} has {rows} rows", tool.name);
        }
    }
}
