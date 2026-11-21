//! Each else.bot tool's schema and handler, bound to its [`ToolId`]. The
//! match is exhaustive, so a tool without a binding does not compile; its
//! policy lives in `app::tools`.

use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use super::AppState;
use super::tool_result::{Done, ToolResult};
use super::{
    bot_tools, connectors, delegation_tools, guest_tools, mcp_wait, memory_tools, outgoing_tools,
    owner_tools, routines, rules, shared_mcp, skills, social_tools, visual_tools, webhooks,
};
use crate::app::tools::{ToolDef, ToolId};
use crate::grants::Grant;

/// One call to a tool's handler.
#[derive(Clone, Copy)]
pub(crate) struct Call<'a> {
    pub state: &'a AppState,
    pub grant: &'a Grant,
    pub args: &'a Value,
    pub tool: &'static ToolDef,
}

type Run = for<'a> fn(Call<'a>) -> BoxFuture<'a, ToolResult>;

struct Binding {
    /// The tool's MCP definition without its name: description, input
    /// schema, and annotations.
    schema: fn() -> Value,
    run: Run,
}

fn bind(schema: fn() -> Value, run: Run) -> Binding {
    Binding { schema, run }
}

/// The tool's MCP definition, its name first.
pub(crate) fn definition(tool: &ToolDef) -> Value {
    let mut definition = serde_json::Map::from_iter([("name".into(), json!(tool.name))]);
    let Value::Object(schema) = (binding(tool.id).schema)() else {
        unreachable!("{}'s schema is an object", tool.name)
    };
    definition.extend(schema.into_iter().filter(|(key, _)| key != "name"));
    Value::Object(definition)
}

pub(crate) fn run<'a>(
    tool: &'static ToolDef,
    state: &'a AppState,
    grant: &'a Grant,
    args: &'a Value,
) -> BoxFuture<'a, ToolResult> {
    (binding(tool.id).run)(Call {
        state,
        grant,
        args,
        tool,
    })
}

/// Annotations for a tool that only reads else.bot's own state.
pub(super) fn read_only() -> Value {
    json!({ "readOnlyHint": true, "destructiveHint": false,
            "idempotentHint": true, "openWorldHint": false })
}

fn binding(id: ToolId) -> Binding {
    match id {
        ToolId::McpList => bind(shared_mcp::list_schema, |c| {
            Box::pin(shared_mcp::call(c, shared_mcp::Op::List))
        }),
        ToolId::McpInstall => bind(shared_mcp::install_schema, |c| {
            Box::pin(shared_mcp::call(c, shared_mcp::Op::Install))
        }),
        ToolId::McpRequest => bind(shared_mcp::request_schema, |c| {
            Box::pin(shared_mcp::call(c, shared_mcp::Op::Request))
        }),
        ToolId::McpWait => bind(mcp_wait::schema, |c| {
            Box::pin(mcp_wait::call(c.state, c.grant, c.args))
        }),
        ToolId::McpConnect => bind(shared_mcp::connect_schema, |c| {
            Box::pin(shared_mcp::call(c, shared_mcp::Op::Connect))
        }),
        ToolId::SkillInstall => bind(skills::install_schema, |c| {
            Box::pin(skills::install(c.state, c.grant, c.args))
        }),
        ToolId::SkillRead => bind(skills::read_schema, |c| {
            Box::pin(skills::read(c.state, c.grant, c.args))
        }),
        ToolId::MemoryBots => bind(memory_tools::bots_schema, |c| {
            Box::pin(memory_tools::bots(c.state, c.grant))
        }),
        ToolId::MemorySearch => bind(memory_tools::search_schema, |c| {
            Box::pin(memory_tools::search(c.state, c.grant, c.args))
        }),
        ToolId::MemoryRead => bind(memory_tools::read_schema, |c| {
            Box::pin(memory_tools::read(c.state, c.grant, c.args))
        }),
        ToolId::MemoryView => bind(memory_tools::view_schema, |c| {
            Box::pin(memory_tools::view(c.state, c.grant, c.args))
        }),
        ToolId::Delegate => bind(delegation_tools::delegate_schema, |c| {
            Box::pin(delegation_tools::delegate(c.state, c.grant, c.args))
        }),
        ToolId::Council => bind(delegation_tools::council_schema, |c| {
            Box::pin(delegation_tools::council(c.state, c.grant, c.args))
        }),
        ToolId::RequestSecret => bind(owner_tools::secret_schema, |c| {
            Box::pin(owner_tools::request_secret(c.state, c.grant, c.args))
        }),
        ToolId::AskOwner => bind(owner_tools::choice_schema, |c| {
            Box::pin(owner_tools::ask_owner(c.state, c.grant, c.args))
        }),
        ToolId::AskPermission => bind(owner_tools::permission_schema, |c| {
            Box::pin(owner_tools::ask_permission(c.state, c.grant, c.args))
        }),
        ToolId::DeleteBot => bind(bot_tools::delete_schema, |c| {
            Box::pin(bot_tools::delete_bot(c.state, c.grant, c.args))
        }),
        ToolId::DraftEmail => bind(outgoing_tools::email_schema, |c| {
            Box::pin(outgoing_tools::draft_email(c.state, c.grant, c.args))
        }),
        ToolId::DraftMessage => bind(outgoing_tools::message_schema, |c| {
            Box::pin(outgoing_tools::draft_message(c.state, c.grant, c.args))
        }),
        ToolId::WebhookCreate => bind(webhooks::create_schema, |c| {
            Box::pin(webhooks::create(c.state, c.grant, c.args))
        }),
        ToolId::MailTriggerSet => bind(connectors::trigger_set_schema, |c| {
            Box::pin(connectors::call(c, connectors::Op::TriggerSet))
        }),
        ToolId::WebhookUpdate => bind(webhooks::update_schema, |c| {
            Box::pin(webhooks::update(c.state, c.grant, c.args))
        }),
        ToolId::SocialConnect => bind(social_tools::connect_schema, |c| {
            Box::pin(social_tools::call(c, social_tools::Op::Connect))
        }),
        ToolId::SocialAction => bind(social_tools::action_schema, |c| {
            Box::pin(social_tools::call(c, social_tools::Op::Action))
        }),
        ToolId::ComputerRun => bind(guest_tools::run_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::Run))
        }),
        ToolId::ComputerJobStart => bind(guest_tools::job_start_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::JobStart))
        }),
        ToolId::ComputerJob => bind(guest_tools::job_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::Job))
        }),
        ToolId::ComputerWatchStart => bind(guest_tools::watch::start_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::WatchStart))
        }),
        ToolId::ComputerWriteFile => bind(guest_tools::write_file_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::WriteFile))
        }),
        ToolId::ComputerReadFile => bind(guest_tools::read_file_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::ReadFile))
        }),
        ToolId::ComputerFetch => bind(guest_tools::fetch_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::Fetch))
        }),
        ToolId::ComputerBrowser => bind(guest_tools::browser_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::Browser))
        }),
        ToolId::ComputerScreenshot => bind(guest_tools::screenshot_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::Screenshot))
        }),
        ToolId::ComputerInput => bind(guest_tools::input_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::Input))
        }),
        ToolId::ComputerResize => bind(guest_tools::resize_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::Resize))
        }),
        ToolId::ComputerHandoff => bind(owner_tools::handoff_schema, |c| {
            Box::pin(owner_tools::computer_handoff(c.state, c.grant, c.args))
        }),
        ToolId::NotifyOwner => bind(owner_tools::notice_schema, |c| {
            Box::pin(owner_tools::notify_owner(c.state, c.grant, c.args))
        }),
        ToolId::MailAccounts => bind(connectors::accounts_schema, |c| {
            Box::pin(connectors::call(c, connectors::Op::Accounts))
        }),
        ToolId::MailConnect => bind(connectors::connect_schema, |c| {
            Box::pin(connectors::call(c, connectors::Op::Connect))
        }),
        ToolId::MailDisconnect => bind(connectors::disconnect_schema, |c| {
            Box::pin(connectors::call(c, connectors::Op::Disconnect))
        }),
        ToolId::MailAction => bind(connectors::action_schema, |c| {
            Box::pin(connectors::call(c, connectors::Op::Action))
        }),
        ToolId::MailActionStatus => bind(connectors::action_status_schema, |c| {
            Box::pin(connectors::call(c, connectors::Op::ActionStatus))
        }),
        ToolId::SocialConnections => bind(social_tools::connections_schema, |c| {
            Box::pin(social_tools::call(c, social_tools::Op::Connections))
        }),
        ToolId::SocialActionStatus => bind(social_tools::action_status_schema, |c| {
            Box::pin(social_tools::call(c, social_tools::Op::ActionStatus))
        }),
        ToolId::RulesUpdate => bind(rules::schema, |c| {
            Box::pin(rules::update(c.state, c.grant, c.args))
        }),
        ToolId::VisualPreview => bind(visual_tools::schema, |c| {
            Box::pin(visual_tools::call(c.state, c.grant, c.args))
        }),
        ToolId::McpConnectionStatus => bind(shared_mcp::connection_status_schema, |c| {
            Box::pin(shared_mcp::call(c, shared_mcp::Op::ConnectionStatus))
        }),
        ToolId::MemoryZoom => bind(memory_tools::zoom_schema, |c| {
            Box::pin(memory_tools::zoom(c.state, c.grant, c.args))
        }),
        ToolId::MemoryDate => bind(memory_tools::date_schema, |c| {
            Box::pin(memory_tools::date(c.state, c.grant, c.args))
        }),
        ToolId::ElseStatus => bind(super::status_schema, |c| {
            Box::pin(async move { Ok(Done::json(super::status(c.state).await)) })
        }),
        ToolId::ListBots => bind(bot_tools::list_schema, |c| {
            Box::pin(bot_tools::list_bots(c.state, c.grant, c.args))
        }),
        ToolId::CreateBot => bind(bot_tools::create_schema, |c| {
            Box::pin(bot_tools::create_bot(c.state, c.grant, c.args))
        }),
        ToolId::DelegationUpdate => bind(delegation_tools::update_schema, |c| {
            Box::pin(delegation_tools::update(c.state, c.grant, c.args))
        }),
        ToolId::DelegationCancel => bind(delegation_tools::cancel_schema, |c| {
            Box::pin(delegation_tools::cancel(c.state, c.grant, c.args))
        }),
        ToolId::RenameBot => bind(bot_tools::rename_schema, |c| {
            Box::pin(bot_tools::rename_bot(c.state, c.grant, c.args))
        }),
        ToolId::SetBotModel => bind(bot_tools::model_schema, |c| {
            Box::pin(bot_tools::set_bot_model(c.state, c.grant, c.args))
        }),
        ToolId::ConfirmPermission => bind(owner_tools::confirm_permission_schema, |c| {
            Box::pin(owner_tools::confirm_permission(c.state, c.grant, c.args))
        }),
        ToolId::ConfirmDraftApproval => bind(outgoing_tools::confirm_approval_schema, |c| {
            Box::pin(outgoing_tools::confirm(c.state, c.grant, c.args))
        }),
        ToolId::StartDraftSend => bind(outgoing_tools::start_send_schema, |c| {
            Box::pin(outgoing_tools::start(c.state, c.grant, c.args))
        }),
        ToolId::SendDraft => bind(outgoing_tools::send_schema, |c| {
            Box::pin(outgoing_tools::send(c.state, c.grant, c.args))
        }),
        ToolId::ReportDraftSend => bind(outgoing_tools::report_send_schema, |c| {
            Box::pin(outgoing_tools::report(c.state, c.grant, c.args))
        }),
        ToolId::RoutineCreate => bind(routines::create_schema, |c| {
            Box::pin(routines::create(c.state, c.grant, c.args))
        }),
        ToolId::RoutineList => bind(routines::list_schema, |c| {
            Box::pin(routines::list(c.state, c.grant))
        }),
        ToolId::RoutineUpdate => bind(routines::update_schema, |c| {
            Box::pin(routines::update(c.state, c.grant, c.args))
        }),
        ToolId::RoutineDelete => bind(routines::delete_schema, |c| {
            Box::pin(routines::delete(c.state, c.grant, c.args))
        }),
        ToolId::RoutineRun => bind(routines::run_schema, |c| {
            Box::pin(routines::run(c.state, c.grant, c.args))
        }),
        ToolId::ReminderCreate => bind(routines::reminder_schema, |c| {
            Box::pin(routines::create_reminder(c.state, c.grant, c.args))
        }),
        ToolId::NoNews => bind(routines::no_news_schema, |c| {
            Box::pin(routines::no_news(c.state, c.grant))
        }),
        ToolId::WebhookList => bind(webhooks::list_schema, |c| {
            Box::pin(webhooks::list(c.state, c.grant))
        }),
        ToolId::WebhookDelete => bind(webhooks::delete_schema, |c| {
            Box::pin(webhooks::delete(c.state, c.grant, c.args))
        }),
        ToolId::SocialTriggerSet => bind(social_tools::trigger_set_schema, |c| {
            Box::pin(social_tools::call(c, social_tools::Op::TriggerSet))
        }),
        ToolId::SocialTriggerList => bind(social_tools::trigger_list_schema, |c| {
            Box::pin(social_tools::call(c, social_tools::Op::TriggerList))
        }),
        ToolId::SocialTriggerDelete => bind(social_tools::trigger_delete_schema, |c| {
            Box::pin(social_tools::call(c, social_tools::Op::TriggerDelete))
        }),
        ToolId::MailTriggerDelete => bind(connectors::trigger_delete_schema, |c| {
            Box::pin(connectors::call(c, connectors::Op::TriggerDelete))
        }),
        ToolId::ComputerJobCancel => bind(guest_tools::job_cancel_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::JobCancel))
        }),
        ToolId::ComputerWatches => bind(guest_tools::watch::list_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::Watches))
        }),
        ToolId::ComputerWatchStop => bind(guest_tools::watch::stop_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::WatchStop))
        }),
        ToolId::ComputerSecrets => bind(guest_tools::secrets_schema, |c| {
            Box::pin(guest_tools::call(c, guest_tools::Op::Secrets))
        }),
    }
}
