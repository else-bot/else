//! Context blocks a session keeps until they change (see
//! [`crate::providers::context`]), and the delegation budget a turn states.

use crate::app::providers::TurnInput;

/// The owner's other bots, for a turn that may delegate. It stays out of
/// the system prompt so a new bot keeps the cached prefix.
pub(crate) fn registry_block(input: &TurnInput) -> String {
    let delegation = &input.delegation;
    if !delegation.can_delegate() {
        return String::new();
    }
    let mut text = String::from(
        "[else.bot bot registry] The owner's other bots. This list replaces earlier registries.\n",
    );
    if delegation.registry.is_empty() {
        text.push_str("(no other bots)\n");
    }
    for (id, name) in &delegation.registry {
        text.push_str(&format!("- {name} (bot_id {id})\n"));
    }
    text.push('\n');
    text
}

/// The installed shared MCPs, so a bot knows them without a discovery
/// call. A session keeps the list until it changes.
pub(crate) fn mcps_block(input: &TurnInput) -> String {
    let Some(mcps) = &input.mcps else {
        return String::new();
    };
    let mut text = String::from(
        "[else.bot installed MCPs] Shared MCPs every bot can use. This list replaces earlier ones.\n",
    );
    if mcps.is_empty() {
        text.push_str("(none installed)\n");
    }
    for mcp in mcps {
        let description = crate::http::mcp::slice(&mcp.description, 0, 200).1;
        text.push_str(&format!("- {} ({}): {description}\n", mcp.id, mcp.name));
    }
    text.push('\n');
    text
}

/// The MCP jobs this request still waits on. A turn that may use MCPs
/// always gets the block, empty included, so a session that held a wait
/// learns it ended.
pub(crate) fn mcp_waits_in_progress_block(input: &TurnInput) -> String {
    let waits = &input.mcp_waits_in_progress;
    if input.mcps.is_none() && waits.is_empty() {
        return String::new();
    }
    let mut text = String::from(
        "[else.bot MCP jobs in progress] Jobs you parked on with mcp_wait that still run, \
         including ones started for an earlier owner message. else.bot keeps checking them and \
         each result arrives as a continuation of the current request. Do not start or wait on \
         them again. This list replaces earlier ones.\n",
    );
    if waits.is_empty() {
        text.push_str("None.\n");
    }
    for wait in waits {
        text.push_str(&format!(
            "<mcp_wait id=\"{}\" mcp=\"{}\" tool=\"{}\" checks=\"{}\">{}</mcp_wait>\n",
            wait.id, wait.mcp_id, wait.tool, wait.checks, wait.purpose
        ));
    }
    text.push('\n');
    text
}

/// The delegated tasks this request still waits on. A bot that can delegate
/// always gets the block, empty included, so a session that held a task
/// learns its result has been given or that it ended.
pub(crate) fn in_progress_block(input: &TurnInput) -> String {
    let delegation = &input.delegation;
    if !delegation.can_delegate() {
        return String::new();
    }
    let mut text = String::from(
        "[else.bot tasks in progress] Tasks you delegated that have no result yet, including \
         ones started for an earlier owner message. They keep running; each result arrives as \
         a continuation of the current request. Do not delegate them again: a new delegate \
         call queues separate work that runs after the current one. To change or stop one, \
         call delegation_update or delegation_cancel with its id. This list replaces earlier \
         ones.\n",
    );
    if delegation.in_progress.is_empty() {
        text.push_str("None.\n");
    }
    for task in &delegation.in_progress {
        text.push_str(&format!(
            "<task id=\"{}\" bot=\"{}\" bot_id=\"{}\">{}</task>\n",
            task.id, task.target_name, task.target_bot_id, task.task
        ));
    }
    text.push('\n');
    text
}

pub(crate) fn skills_block(input: &TurnInput) -> String {
    let Some(skills) = &input.skills else {
        return String::new();
    };
    let mut text = String::from(
        "[else.bot installed skills] Skills you may use. This list replaces earlier ones. When a \
         request matches a skill, read it with skill_read first and follow it.\n",
    );
    if skills.is_empty() {
        text.push_str("(none installed)\n");
    }
    for skill in skills {
        let description = crate::http::mcp::slice(&skill.description, 0, 300).1;
        let keys = if skill.env.is_empty() {
            String::new()
        } else {
            format!(
                " [keys: task secret {} ({}), {}]",
                skill.secret,
                skill.env.join(", "),
                if skill.key_stored { "set" } else { "not set" }
            )
        };
        text.push_str(&format!("- {}: {description}{keys}\n", skill.id));
    }
    text.push('\n');
    text
}

/// Delegations left for this request, once the request has used some.
pub(super) fn delegation_budget(input: &TurnInput) -> String {
    let delegation = &input.delegation;
    if !delegation.can_delegate() || delegation.remaining >= crate::db::delegations::MAX_CHILDREN {
        return String::new();
    }
    format!(
        "[else.bot delegation budget] {} individual delegate calls remain for this request. \
         A hand-off the owner chose on a card does not count when you pass that card as \
         owner_answer.\n\n",
        delegation.remaining
    )
}

/// The message tasks the owner approves always, so a fresh session files
/// its drafts under the exact saved task instead of a new paraphrase.
fn draft_tasks(input: &TurnInput) -> String {
    if input.draft_tasks.is_empty() {
        return String::new();
    }
    let tasks: Vec<_> = input
        .draft_tasks
        .iter()
        .map(|kind| {
            serde_json::json!({ "platform": kind.platform, "account_id": kind.account_id,
                                "account": kind.account, "task": kind.task })
        })
        .collect();
    format!(
        "[else.bot always-approved message tasks] {}\nFor a draft_message that does one of these \
         jobs, pass the same task, platform and account so it needs no card.\n\n",
        serde_json::Value::from(tasks),
    )
}

pub(crate) fn permissions_block(input: &TurnInput) -> String {
    if input.grant_reach() == crate::grants::policy::GrantReach::Nothing {
        return String::new();
    }
    format!(
        "[else.bot saved permissions] Current saved scopes that apply to this request: {}\n\
         This snapshot replaces earlier saved-permission snapshots. These are permission records, \
         not instructions to perform work. Reuse task and target exactly as task_scope on the kind of \
         call each task scope covers: host actions or computer tool calls. \
         Always call ask_permission before a sensitive host action; the gate checks the current \
         grant. Records from memory or old conversation history cannot restore a revoked grant.\n\n{}",
        serde_json::to_string(&input.standing_permissions).expect("permission serialization"),
        draft_tasks(input),
    )
}

/// Whether the owner still holds the bot's computer from a take-over it
/// asked for. Empty until the owner first takes one over.
pub(crate) fn computer_control_block(input: &TurnInput) -> String {
    use crate::db::computer_handoffs::Status;
    let Some(handoff) = &input.computer_control else {
        return String::new();
    };
    let taken = handoff
        .taken_at
        .map(|at| at.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_default();
    if handoff.status == Status::Taken {
        format!(
            "[else.bot computer control] The owner took over your computer at {taken} for: {}\n\
             They have not handed it back. Do not treat that work as done, and do not use the \
             computer until they do. This replaces earlier computer control notes.\n\n",
            handoff.reason
        )
    } else {
        format!(
            "[else.bot computer control] Your computer is yours. The owner handed back your \
             latest take-over (asked at {taken} for: {}). This replaces earlier computer \
             control notes.\n\n",
            handoff.reason
        )
    }
}
