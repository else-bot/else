//! Every else.bot tool's policy, defined once: the grant it needs, whether a
//! turn lists it, whether it may park the turn, how its calls reach the
//! owner's approval, and what of its arguments the activity log keeps. A
//! name with no row is not a tool. `http::tool_defs` binds each [`ToolId`]
//! to its schema and handler.
//!
//! Rows are in listed order, then deferred order: provider adapters hash
//! and allowlist [`TOOL_NAMES`], `tools/list` follows it, and `find_tool`
//! breaks ties by it, so moving a listed row rebuilds every native session
//! and invalidates every provider's prompt cache.

use serde_json::{Value, json};

use crate::grants::Tool;

pub const SERVER_NAME: &str = "elsebot";
pub const FIND: &str = "find_tool";
pub const USE: &str = "use_tool";
/// The tools a turn lists, in the order adapters hash and allowlist, then
/// the two search tools. Rarely used tools are reached through `find_tool`
/// and `use_tool` instead (see [`DEFERRED_TOOLS`]).
pub const TOOL_NAMES: &[&str] = &LISTED;
/// Tools reached only through `use_tool`, after `find_tool` returns their
/// schema. None may park a turn: providers recognize parking by the name of
/// the tool they called, which for these is `use_tool`.
pub const DEFERRED_TOOLS: &[&str] = &DEFERRED;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolId {
    McpList,
    McpInstall,
    McpRequest,
    McpWait,
    McpConnect,
    SkillInstall,
    SkillRead,
    MemoryBots,
    MemorySearch,
    MemoryRead,
    MemoryView,
    Delegate,
    Council,
    RequestSecret,
    AskOwner,
    AskPermission,
    DeleteBot,
    DraftEmail,
    DraftMessage,
    WebhookCreate,
    MailTriggerSet,
    WebhookUpdate,
    SocialConnect,
    SocialAction,
    ComputerRun,
    ComputerJobStart,
    ComputerJob,
    ComputerWatchStart,
    ComputerWriteFile,
    ComputerReadFile,
    ComputerFetch,
    ComputerBrowser,
    ComputerScreenshot,
    ComputerInput,
    ComputerResize,
    ComputerHandoff,
    NotifyOwner,
    MailAccounts,
    MailConnect,
    MailDisconnect,
    MailAction,
    MailActionStatus,
    SocialConnections,
    SocialActionStatus,
    RulesUpdate,
    VisualPreview,
    McpConnectionStatus,
    MemoryZoom,
    MemoryDate,
    ElseStatus,
    ListBots,
    CreateBot,
    DelegationUpdate,
    DelegationCancel,
    RenameBot,
    SetBotModel,
    ConfirmPermission,
    ConfirmDraftApproval,
    StartDraftSend,
    SendDraft,
    ReportDraftSend,
    RoutineCreate,
    RoutineList,
    RoutineUpdate,
    RoutineDelete,
    RoutineRun,
    ReminderCreate,
    NoNews,
    WebhookList,
    WebhookDelete,
    SocialTriggerSet,
    SocialTriggerList,
    SocialTriggerDelete,
    MailTriggerDelete,
    ComputerJobCancel,
    ComputerWatches,
    ComputerWatchStop,
    ComputerSecrets,
}

impl ToolId {
    pub const fn def(self) -> &'static ToolDef {
        &TOOLS[self as usize]
    }
}

pub struct ToolDef {
    pub id: ToolId,
    pub name: &'static str,
    pub needs: Tool,
    pub listing: Listing,
    pub approval: Approval,
    pub loggable: fn(&Value) -> Value,
}

#[derive(Clone, Copy)]
pub enum Listing {
    /// In every turn's tool list. Only these may park a turn: providers
    /// recognize parking by the name of the tool they called, which for a
    /// deferred tool is `use_tool`.
    Listed { parks: bool },
    /// Reached through `find_tool` and `use_tool`.
    Deferred,
}

/// How a tool's calls reach the owner's approval.
#[derive(Clone, Copy)]
pub enum Approval {
    /// The handler asks the owner itself, if ever.
    Handler,
    /// The computer tools' shared path, which approves a call by its
    /// declared effect. One standing owner grant per tool and effect may
    /// cover the calls of a `standing` tool.
    Computer { effect: EffectRule, standing: bool },
}

/// Which calls must say what they do before approval.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EffectRule {
    /// The schema declares no effect, and a call that gives one is refused.
    Undeclared,
    /// Every call declares one.
    Required,
    /// Declared; required for a browser click, typing, or accepting a dialog.
    BrowserAction,
}

impl EffectRule {
    pub fn declared(self) -> bool {
        self != Self::Undeclared
    }

    pub fn required(self, args: &Value) -> bool {
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

impl ToolDef {
    const fn listed(id: ToolId, name: &'static str, needs: Tool) -> Self {
        Self::new(id, name, needs, Listing::Listed { parks: false })
    }

    const fn deferred(id: ToolId, name: &'static str, needs: Tool) -> Self {
        Self::new(id, name, needs, Listing::Deferred)
    }

    const fn new(id: ToolId, name: &'static str, needs: Tool, listing: Listing) -> Self {
        Self {
            id,
            name,
            needs,
            listing,
            approval: Approval::Handler,
            loggable: Value::clone,
        }
    }

    const fn parking(mut self) -> Self {
        match self.listing {
            Listing::Listed { .. } => self.listing = Listing::Listed { parks: true },
            Listing::Deferred => panic!("a deferred tool cannot park"),
        }
        self
    }

    const fn computer(mut self, effect: EffectRule) -> Self {
        self.approval = Approval::Computer {
            effect,
            standing: false,
        };
        self
    }

    const fn standing(mut self) -> Self {
        match self.approval {
            Approval::Computer { effect, .. } => {
                self.approval = Approval::Computer {
                    effect,
                    standing: true,
                };
            }
            Approval::Handler => panic!("only a computer tool takes a standing grant"),
        }
        self
    }

    const fn logs(mut self, loggable: fn(&Value) -> Value) -> Self {
        self.loggable = loggable;
        self
    }

    /// The row a provider or a stored record names.
    pub fn named(name: &str) -> Option<&'static Self> {
        TOOLS.iter().find(|tool| tool.name == name)
    }

    pub const fn parks(&self) -> bool {
        matches!(self.listing, Listing::Listed { parks: true })
    }

    pub const fn is_listed(&self) -> bool {
        matches!(self.listing, Listing::Listed { .. })
    }

    /// Whether one standing grant per tool and effect may cover its calls.
    pub const fn standing_grants(&self) -> bool {
        matches!(self.approval, Approval::Computer { standing: true, .. })
    }

    /// Whether its schema declares `effect` for the computer approval path.
    pub fn declares_effect(&self) -> bool {
        matches!(self.approval, Approval::Computer { effect, .. } if effect.declared())
    }
}

const LISTED: [&str; count(true) + 2] = {
    let mut names = names::<{ count(true) + 2 }>(true);
    names[names.len() - 2] = FIND;
    names[names.len() - 1] = USE;
    names
};
const DEFERRED: [&str; count(false)] = names::<{ count(false) }>(false);

const fn count(listed: bool) -> usize {
    let (mut i, mut n) = (0, 0);
    while i < TOOLS.len() {
        if TOOLS[i].is_listed() == listed {
            n += 1;
        }
        i += 1;
    }
    n
}

/// The listed or deferred names in row order; slots past them stay empty.
const fn names<const N: usize>(listed: bool) -> [&'static str; N] {
    let mut out = [""; N];
    let (mut i, mut n) = (0, 0);
    while i < TOOLS.len() {
        if TOOLS[i].is_listed() == listed {
            out[n] = TOOLS[i].name;
            n += 1;
        }
        i += 1;
    }
    out
}

// `ToolId::def` indexes the rows by ID.
const _: () = {
    assert!(TOOLS.len() == ToolId::ComputerSecrets as usize + 1);
    let mut i = 0;
    while i < TOOLS.len() {
        assert!(TOOLS[i].id as usize == i, "rows follow ToolId order");
        i += 1;
    }
};

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
        "actions": summarize_input(
            args["actions"].as_array().map(Vec::as_slice).unwrap_or_default()
        ),
    })
}

pub const TOOLS: &[ToolDef] = &[
    ToolDef::listed(ToolId::McpList, "mcp_list", Tool::Mcp),
    ToolDef::listed(ToolId::McpInstall, "mcp_install", Tool::ManageMcp)
        .parking()
        .logs(|args| json!({ "id": args["id"], "name": args["name"] })),
    ToolDef::listed(ToolId::McpRequest, "mcp_request", Tool::Mcp)
        .parking()
        .logs(|args| {
            json!({
                "id": args["id"],
                "method": args["method"],
                "tool": args["params"]["name"],
                "effect": args["effect"],
            })
        }),
    ToolDef::listed(ToolId::McpWait, "mcp_wait", Tool::Mcp)
        .parking()
        .logs(|args| json!({ "id": args["id"], "tool": args["tool"] })),
    ToolDef::listed(ToolId::McpConnect, "mcp_connect", Tool::Mcp)
        .parking()
        .logs(by_id),
    ToolDef::listed(ToolId::SkillInstall, "skill_install", Tool::ManageMcp).parking(),
    ToolDef::listed(ToolId::SkillRead, "skill_read", Tool::Mcp),
    ToolDef::listed(ToolId::MemoryBots, "memory_bots", Tool::ReadMemory),
    ToolDef::listed(ToolId::MemorySearch, "memory_search", Tool::ReadMemory),
    ToolDef::listed(ToolId::MemoryRead, "memory_read", Tool::ReadMemory),
    ToolDef::listed(ToolId::MemoryView, "memory_view", Tool::ReadMemory),
    ToolDef::listed(ToolId::Delegate, "delegate", Tool::Delegate).parking(),
    ToolDef::listed(ToolId::Council, "council", Tool::Delegate).parking(),
    ToolDef::listed(ToolId::RequestSecret, "request_secret", Tool::RequestSecret).parking(),
    ToolDef::listed(ToolId::AskOwner, "ask_owner", Tool::AskOwner).parking(),
    ToolDef::listed(ToolId::AskPermission, "ask_permission", Tool::AskPermission).parking(),
    ToolDef::listed(ToolId::DeleteBot, "delete_bot", Tool::EditBot).parking(),
    ToolDef::listed(ToolId::DraftEmail, "draft_email", Tool::DraftEmail).parking(),
    ToolDef::listed(ToolId::DraftMessage, "draft_message", Tool::AskOwner).parking(),
    ToolDef::listed(ToolId::WebhookCreate, "webhook_create", Tool::Routines).parking(),
    ToolDef::listed(ToolId::MailTriggerSet, "mail_trigger_set", Tool::Routines).parking(),
    ToolDef::listed(ToolId::WebhookUpdate, "webhook_update", Tool::Routines).parking(),
    ToolDef::listed(ToolId::SocialConnect, "social_connect", Tool::AskOwner)
        .parking()
        .logs(|args| json!({ "provider": args["provider"], "reason": args["reason"] })),
    ToolDef::listed(ToolId::SocialAction, "social_action", Tool::Social)
        .parking()
        .logs(social_action),
    ToolDef::listed(ToolId::ComputerRun, "computer_run", Tool::Computer)
        .parking()
        .computer(EffectRule::Required)
        .standing(),
    ToolDef::listed(
        ToolId::ComputerJobStart,
        "computer_job_start",
        Tool::Computer,
    )
    .parking()
    .computer(EffectRule::Required)
    .standing(),
    ToolDef::listed(ToolId::ComputerJob, "computer_job", Tool::Computer)
        .parking()
        .computer(EffectRule::Undeclared)
        .standing(),
    ToolDef::listed(
        ToolId::ComputerWatchStart,
        "computer_watch_start",
        Tool::Computer,
    )
    .computer(EffectRule::Undeclared)
    .standing(),
    ToolDef::listed(
        ToolId::ComputerWriteFile,
        "computer_write_file",
        Tool::Computer,
    )
    .parking()
    .computer(EffectRule::Required)
    .standing(),
    ToolDef::listed(
        ToolId::ComputerReadFile,
        "computer_read_file",
        Tool::Computer,
    )
    .computer(EffectRule::Undeclared)
    .standing(),
    ToolDef::listed(ToolId::ComputerFetch, "computer_fetch", Tool::Computer)
        .computer(EffectRule::Undeclared)
        .standing(),
    ToolDef::listed(ToolId::ComputerBrowser, "computer_browser", Tool::Computer)
        .parking()
        .computer(EffectRule::BrowserAction)
        .standing()
        .logs(browser),
    ToolDef::listed(
        ToolId::ComputerScreenshot,
        "computer_screenshot",
        Tool::Computer,
    )
    .computer(EffectRule::Undeclared)
    .standing(),
    ToolDef::listed(ToolId::ComputerInput, "computer_input", Tool::Computer)
        .parking()
        .computer(EffectRule::Required)
        .standing()
        .logs(input),
    // A resize takes host memory, so only a grant for its size covers it.
    ToolDef::listed(ToolId::ComputerResize, "computer_resize", Tool::Computer)
        .parking()
        .computer(EffectRule::Undeclared),
    ToolDef::listed(ToolId::ComputerHandoff, "computer_handoff", Tool::Computer).parking(),
    // Every turn may read its history; the tool itself admits only the turn
    // it serves: a running delegated task or routine run.
    ToolDef::listed(ToolId::NotifyOwner, "notify_owner", Tool::ReadHistory),
    ToolDef::listed(ToolId::MailAccounts, "mail_accounts", Tool::Mail).logs(nothing),
    ToolDef::listed(ToolId::MailConnect, "mail_connect", Tool::AskOwner)
        .parking()
        .logs(|args| json!({ "provider": args["provider"] })),
    ToolDef::listed(ToolId::MailDisconnect, "mail_disconnect", Tool::AskOwner)
        .parking()
        .logs(|args| json!({ "account_id": args["account_id"] })),
    ToolDef::listed(ToolId::MailAction, "mail_action", Tool::Mail)
        .parking()
        .logs(|args| {
            json!({
                "account_id": args["account_id"],
                "action_id": args["action_id"],
                "operation": args["command"]["operation"],
            })
        }),
    ToolDef::listed(ToolId::MailActionStatus, "mail_action_status", Tool::Mail),
    ToolDef::listed(
        ToolId::SocialConnections,
        "social_connections",
        Tool::Social,
    )
    .logs(nothing),
    ToolDef::listed(
        ToolId::SocialActionStatus,
        "social_action_status",
        Tool::Social,
    )
    .logs(social_action),
    ToolDef::listed(ToolId::RulesUpdate, "rules_update", Tool::Rules),
    // Previews read the project, which only turns with the computer may do.
    ToolDef::listed(ToolId::VisualPreview, "visual_preview", Tool::Computer),
    ToolDef::deferred(
        ToolId::McpConnectionStatus,
        "mcp_connection_status",
        Tool::Mcp,
    )
    .logs(by_id),
    ToolDef::deferred(ToolId::MemoryZoom, "memory_zoom", Tool::ReadMemory),
    ToolDef::deferred(ToolId::MemoryDate, "memory_date", Tool::ReadMemory),
    ToolDef::deferred(ToolId::ElseStatus, "else_status", Tool::Diagnose),
    ToolDef::deferred(ToolId::ListBots, "list_bots", Tool::Delegate),
    ToolDef::deferred(ToolId::CreateBot, "create_bot", Tool::CreateBot),
    ToolDef::deferred(
        ToolId::DelegationUpdate,
        "delegation_update",
        Tool::Delegate,
    ),
    ToolDef::deferred(
        ToolId::DelegationCancel,
        "delegation_cancel",
        Tool::Delegate,
    ),
    ToolDef::deferred(ToolId::RenameBot, "rename_bot", Tool::EditBot),
    ToolDef::deferred(ToolId::SetBotModel, "set_bot_model", Tool::EditBot),
    ToolDef::deferred(
        ToolId::ConfirmPermission,
        "confirm_permission",
        Tool::AskOwner,
    ),
    ToolDef::deferred(
        ToolId::ConfirmDraftApproval,
        "confirm_draft_approval",
        Tool::AskOwner,
    ),
    ToolDef::deferred(ToolId::StartDraftSend, "start_draft_send", Tool::AskOwner),
    // Sending also needs the draft's channel: Mail or Social.
    ToolDef::deferred(ToolId::SendDraft, "send_draft", Tool::DraftEmail),
    ToolDef::deferred(ToolId::ReportDraftSend, "report_draft_send", Tool::AskOwner),
    ToolDef::deferred(ToolId::RoutineCreate, "routine_create", Tool::Routines),
    ToolDef::deferred(ToolId::RoutineList, "routine_list", Tool::Routines),
    ToolDef::deferred(ToolId::RoutineUpdate, "routine_update", Tool::Routines),
    ToolDef::deferred(ToolId::RoutineDelete, "routine_delete", Tool::Routines),
    ToolDef::deferred(ToolId::RoutineRun, "routine_run", Tool::Routines),
    ToolDef::deferred(ToolId::ReminderCreate, "reminder_create", Tool::Routines),
    // Like notify_owner: the tool admits only a running routine run.
    ToolDef::deferred(ToolId::NoNews, "no_news", Tool::ReadHistory),
    ToolDef::deferred(ToolId::WebhookList, "webhook_list", Tool::Routines),
    ToolDef::deferred(ToolId::WebhookDelete, "webhook_delete", Tool::Routines),
    ToolDef::deferred(
        ToolId::SocialTriggerSet,
        "social_trigger_set",
        Tool::Routines,
    ),
    ToolDef::deferred(
        ToolId::SocialTriggerList,
        "social_trigger_list",
        Tool::Routines,
    ),
    ToolDef::deferred(
        ToolId::SocialTriggerDelete,
        "social_trigger_delete",
        Tool::Routines,
    ),
    ToolDef::deferred(
        ToolId::MailTriggerDelete,
        "mail_trigger_delete",
        Tool::Routines,
    ),
    ToolDef::deferred(
        ToolId::ComputerJobCancel,
        "computer_job_cancel",
        Tool::Computer,
    )
    .computer(EffectRule::Undeclared)
    .standing(),
    ToolDef::deferred(ToolId::ComputerWatches, "computer_watches", Tool::Computer)
        .computer(EffectRule::Undeclared)
        .standing(),
    ToolDef::deferred(
        ToolId::ComputerWatchStop,
        "computer_watch_stop",
        Tool::Computer,
    )
    .computer(EffectRule::Undeclared)
    .standing(),
    ToolDef::deferred(ToolId::ComputerSecrets, "computer_secrets", Tool::Computer)
        .computer(EffectRule::Undeclared)
        .standing(),
];

pub fn parks(tool: &str, result: &serde_json::Value) -> bool {
    use serde_json::Value;

    let parking = ToolDef::named(tool).is_some_and(ToolDef::parks);
    if !parking || result["isError"] == true {
        return false;
    }
    let parked = |text: &str| {
        serde_json::from_str::<Value>(text).is_ok_and(|payload| payload["parked"] == true)
    };
    match result.get("content").unwrap_or(result) {
        Value::String(text) => parked(text),
        Value::Array(blocks) => blocks
            .iter()
            .any(|block| block["type"] == "text" && block["text"].as_str().is_some_and(parked)),
        _ => false,
    }
}

pub fn summarize_input(actions: &[serde_json::Value]) -> String {
    actions
        .iter()
        .map(|a| {
            if let Some(t) = a["type"].as_str() {
                format!("type({} chars)", t.chars().count())
            } else if let Some(k) = a["key"].as_str() {
                format!("key({})", &k[..k.floor_char_boundary(k.len().min(40))])
            } else if let Some((kind, v)) = a.as_object().and_then(|o| o.iter().next()) {
                format!("{kind}({v})")
            } else {
                "?".into()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn each_tool_has_one_row() {
        for (i, tool) in TOOLS.iter().enumerate() {
            let rows = TOOLS[i..].iter().filter(|t| t.name == tool.name).count();
            assert_eq!(rows, 1, "{} has {rows} rows", tool.name);
        }
    }

    #[test]
    fn parking_reads_the_protocol_field_in_each_provider_envelope() {
        let blocks =
            json!([{ "type": "text", "text": "{ \"parked\": true, \"status\": \"pending\" }" }]);
        assert!(parks("computer_run", &json!({ "content": blocks })));
        assert!(parks("delegate", &blocks));
        assert!(parks("ask_owner", &json!("{\"parked\":true}")));
        assert!(!parks("memory_search", &blocks));
        assert!(!parks(
            "computer_run",
            &json!({ "content": blocks, "isError": true })
        ));
    }

    #[test]
    fn tool_output_cannot_impersonate_a_parking_result() {
        for text in [
            "pending",
            "not JSON: \"parked\":true",
            "{\"stdout\":\"{\\\"parked\\\":true}\"}",
            "{\"nested\":{\"parked\":true}}",
            "{\"parked\":\"true\"}",
        ] {
            let result = json!({ "content": [{ "type": "text", "text": text }] });
            assert!(!parks("computer_run", &result), "{text}");
        }
    }
}
