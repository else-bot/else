//! What a turn may do, decided from what started it: the tools and memory
//! its task grant carries, whether its provider gets host tools, whether
//! it may delegate, and which saved owner grants reach it.

use sqlx::SqlitePool;
use uuid::Uuid;

use super::Tool;
use crate::app::providers::{DelegationContext, Origin, TurnInput};
use crate::app::questions::PermissionTarget;

/// What one turn's task grant carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnAccess {
    pub tools: Vec<Tool>,
    pub memory: Memory,
}

/// Whose memory a turn's grant reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Memory {
    /// Its bot's own and every bot's the owner shared with it.
    Shared,
    /// Only its bot's own.
    Own,
    /// A delegated task's: its bot's own, plus what every bot above it in
    /// the delegation chain may read too.
    Chain,
}

pub fn for_turn(delegation: &DelegationContext) -> TurnAccess {
    let memory = if delegation.delegated_task() && delegation.delegated_by.is_some() {
        Memory::Chain
    } else if delegation.delegated_task()
        || matches!(
            delegation.origin,
            Origin::ConnectorEvent | Origin::SocialEvent
        )
    {
        Memory::Own
    } else {
        Memory::Shared
    };
    TurnAccess {
        tools: tools(delegation),
        memory,
    }
}

fn tools(delegation: &DelegationContext) -> Vec<Tool> {
    if delegation.origin == Origin::SocialEvent {
        return vec![Tool::ReadHistory, Tool::ReadMemory, Tool::Social];
    }
    if delegation.email_task() {
        return vec![Tool::ReadHistory];
    }
    if delegation.origin == Origin::ConnectorEvent {
        return vec![
            Tool::ReadHistory,
            Tool::ReadMemory,
            Tool::Mail,
            Tool::DraftEmail,
            Tool::Delegate,
        ];
    }
    let mut tools = vec![
        Tool::ReadHistory,
        Tool::ReadMemory,
        Tool::Computer,
        Tool::Mcp,
        Tool::AskPermission,
        Tool::Diagnose,
    ];
    if delegation.acts_for_owner() {
        tools.extend([
            Tool::Mail,
            Tool::Social,
            Tool::RequestSecret,
            Tool::AskOwner,
            Tool::DraftEmail,
        ]);
    }
    if delegation.can_delegate() {
        tools.push(Tool::Delegate);
    }
    if delegation.can_create_bot {
        tools.push(Tool::CreateBot);
    }
    if delegation.origin == Origin::OwnerRequest {
        tools.extend([Tool::EditBot, Tool::ManageMcp]);
    }
    if delegation.can_manage_routines() {
        tools.push(Tool::Routines);
    }
    if delegation.can_edit_rules() {
        tools.push(Tool::Rules);
    }
    tools
}

/// The bots `turn_id`'s grant may read memory from; `None` reads every bot
/// shared with `bot_id`. A chain that cannot be read narrows to `bot_id`.
pub async fn memory_scope(
    pool: &SqlitePool,
    turn_id: Uuid,
    bot_id: Uuid,
    memory: Memory,
) -> Option<Vec<Uuid>> {
    match memory {
        Memory::Shared => None,
        Memory::Own => Some(vec![bot_id]),
        Memory::Chain => match chain_scope(pool, turn_id, bot_id).await {
            Ok(scope) => Some(scope),
            Err(err) => {
                tracing::warn!(turn = %turn_id, error = %err, "loading a delegated memory scope failed");
                Some(vec![bot_id])
            }
        },
    }
}

/// The memory a task of `target` may read: its own, plus each bot it may
/// read that every bot above it in the chain may read too, so handing work
/// down a chain never widens what the request can see.
async fn chain_scope(
    pool: &SqlitePool,
    turn_id: Uuid,
    target: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let upstream = {
        let mut conn = pool.acquire().await?;
        crate::db::delegations::chain(&mut conn, turn_id)
            .await?
            .upstream
    };
    let mut scope = vec![target];
    scope.extend(
        crate::db::memory::readable_bots(pool, target)
            .await?
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| *id != target),
    );
    for bot in upstream {
        let reads: Vec<Uuid> = crate::db::memory::readable_bots(pool, bot)
            .await?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        scope.retain(|id| *id == target || reads.contains(id));
    }
    Ok(scope)
}

impl Origin {
    /// Whether a turn `depth` delegations below its request may hand work
    /// to another bot. An email turn may pass mail on as the owner's
    /// standing instructions say, but its task may not hand it further; a
    /// social event may not delegate at all.
    pub fn delegates_at(self, depth: u8) -> bool {
        match self {
            Origin::SocialEvent => false,
            Origin::ConnectorEvent => depth == 0,
            _ => depth < crate::db::delegations::MAX_DEPTH,
        }
    }
}

impl DelegationContext {
    /// A task another bot delegated to this turn.
    pub fn delegated_task(&self) -> bool {
        self.task_depth > 0
    }

    /// Whether this turn may hand work to another bot. See
    /// [`Origin::delegates_at`].
    pub fn can_delegate(&self) -> bool {
        self.origin.delegates_at(self.task_depth)
    }

    /// A task an email turn handed on. Its task text came from email and its
    /// reply goes back to a turn that can send mail, so it gets no memory,
    /// mail, computer records or tools beyond its own history. Its bot's
    /// rules still apply to it.
    pub fn email_task(&self) -> bool {
        self.origin == Origin::ConnectorEvent && self.delegated_task()
    }

    /// Work done for the owner: the bot's own conversation, or a task another
    /// bot delegated for an owner request, a routine run, or a webhook wake,
    /// which may ask the owner itself. It may use mail and social tools and
    /// show the owner cards, which a task shows in its own bot's chat.
    pub fn acts_for_owner(&self) -> bool {
        !self.delegated_task()
            || matches!(
                self.origin,
                Origin::OwnerRequest | Origin::RoutineRun | Origin::WebhookWake
            )
    }

    /// A bot manages its own routines for work rooted in an owner message,
    /// or for a task delegated with the owner's answer on a card; never for
    /// a routine run or another task one delegated.
    pub fn can_manage_routines(&self) -> bool {
        self.origin == Origin::OwnerRequest || self.owner_answer.is_some()
    }

    /// A bot edits its rules while answering the owner directly in its own
    /// conversation, or in a task delegated with the owner's answer on a
    /// card; never for a routine, event, or other delegated task.
    pub fn can_edit_rules(&self) -> bool {
        (self.origin == Origin::OwnerRequest && !self.delegated_task())
            || self.owner_answer.is_some()
    }

    /// The provider's own host shell, file, and web tools. Never for work an
    /// incoming connector event started: its content is untrusted, so it
    /// gets only the tools its task grant names.
    pub fn host_tools(&self) -> bool {
        !matches!(self.origin, Origin::ConnectorEvent | Origin::SocialEvent)
    }
}

/// Which saved grants a turn may use, by what started it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantReach {
    /// The owner's own chat request: every saved grant.
    OwnerChat,
    /// A scheduled routine run, or work delegated for an owner request or
    /// a routine run.
    Tasks,
    /// Webhook wakes, events, and work delegated for them.
    Nothing,
}

impl GrantReach {
    pub fn of(origin: Origin, owner_chat: bool) -> Self {
        match origin {
            Origin::OwnerRequest if owner_chat => Self::OwnerChat,
            Origin::OwnerRequest | Origin::RoutineRun => Self::Tasks,
            Origin::WebhookWake | Origin::ConnectorEvent | Origin::SocialEvent | Origin::Other => {
                Self::Nothing
            }
        }
    }

    pub fn covers(self, target: &PermissionTarget) -> bool {
        match self {
            Self::OwnerChat => true,
            Self::Tasks => matches!(
                target,
                PermissionTarget::Task { .. }
                    | PermissionTarget::McpTool { .. }
                    | PermissionTarget::ComputerTool { .. }
            ),
            Self::Nothing => false,
        }
    }
}

impl TurnInput {
    pub fn grant_reach(&self) -> GrantReach {
        GrantReach::of(self.delegation.origin, !self.delegation.delegated_task())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::questions::Kind;

    fn turn_tools(delegation: &DelegationContext) -> Vec<Tool> {
        for_turn(delegation).tools
    }

    fn task(origin: Origin) -> Vec<Tool> {
        turn_tools(&DelegationContext {
            origin,
            task_depth: 1,
            ..DelegationContext::default()
        })
    }

    #[test]
    fn a_task_delegated_for_an_owner_request_can_ask_the_owner() {
        let tools = task(Origin::OwnerRequest);
        for tool in [
            Tool::Social,
            Tool::Mail,
            Tool::RequestSecret,
            Tool::AskOwner,
        ] {
            assert!(tools.contains(&tool), "{tool:?} missing from {tools:?}");
        }
        assert!(
            tools.contains(&Tool::Delegate),
            "a task may hand work on once"
        );
        assert!(!tools.contains(&Tool::CreateBot));
        let leaf = turn_tools(&DelegationContext {
            origin: Origin::OwnerRequest,
            task_depth: crate::db::delegations::MAX_DEPTH,
            ..DelegationContext::default()
        });
        assert!(
            !leaf.contains(&Tool::Delegate),
            "a task at the depth limit cannot"
        );
    }

    #[test]
    fn only_a_turn_answering_the_owner_directly_can_edit_rules() {
        let direct = turn_tools(&DelegationContext {
            origin: Origin::OwnerRequest,
            ..DelegationContext::default()
        });
        assert!(direct.contains(&Tool::Rules));
        for origin in [
            Origin::OwnerRequest,
            Origin::RoutineRun,
            Origin::SocialEvent,
            Origin::ConnectorEvent,
            Origin::Other,
        ] {
            assert!(!task(origin).contains(&Tool::Rules), "delegated {origin:?}");
            let own = turn_tools(&DelegationContext {
                origin,
                ..DelegationContext::default()
            });
            assert_eq!(
                own.contains(&Tool::Rules),
                origin == Origin::OwnerRequest,
                "{origin:?}"
            );
        }
    }

    #[test]
    fn inbound_social_and_mail_events_cannot_read_installation_status() {
        assert!(task(Origin::OwnerRequest).contains(&Tool::Diagnose));
        assert!(task(Origin::RoutineRun).contains(&Tool::Diagnose));
        for origin in [Origin::SocialEvent, Origin::ConnectorEvent] {
            assert!(!task(origin).contains(&Tool::Diagnose), "{origin:?}");
        }
    }

    #[test]
    fn a_mail_event_drafts_email_for_the_owner_but_cannot_ask_them() {
        let event = turn_tools(&DelegationContext {
            origin: Origin::ConnectorEvent,
            ..DelegationContext::default()
        });
        assert!(event.contains(&Tool::DraftEmail));
        assert!(!event.contains(&Tool::AskOwner));
        assert!(
            !turn_tools(&DelegationContext {
                origin: Origin::SocialEvent,
                ..DelegationContext::default()
            })
            .contains(&Tool::DraftEmail)
        );
        assert!(task(Origin::OwnerRequest).contains(&Tool::DraftEmail));
        assert!(task(Origin::RoutineRun).contains(&Tool::DraftEmail));
        assert!(!task(Origin::Other).contains(&Tool::DraftEmail));
    }

    #[test]
    fn a_task_delegated_by_a_routine_run_asks_the_owner_like_the_run() {
        let tools = task(Origin::RoutineRun);
        for tool in [
            Tool::Social,
            Tool::Mail,
            Tool::RequestSecret,
            Tool::AskOwner,
        ] {
            assert!(tools.contains(&tool), "{tool:?} missing from {tools:?}");
        }
        for tool in [Tool::Routines, Tool::Rules, Tool::EditBot] {
            assert!(!tools.contains(&tool), "{tool:?} granted in {tools:?}");
        }
        assert!(!task(Origin::Other).contains(&Tool::AskOwner));
    }

    #[test]
    fn delegated_tasks_and_inbound_events_read_narrowed_memory() {
        let memory = |origin, task_depth, delegated_by: Option<(Uuid, String)>| {
            for_turn(&DelegationContext {
                origin,
                task_depth,
                delegated_by,
                ..DelegationContext::default()
            })
            .memory
        };
        let by = || Some((Uuid::nil(), "Main".to_owned()));
        assert_eq!(memory(Origin::OwnerRequest, 0, None), Memory::Shared);
        assert_eq!(memory(Origin::RoutineRun, 0, None), Memory::Shared);
        assert_eq!(memory(Origin::OwnerRequest, 1, by()), Memory::Chain);
        assert_eq!(memory(Origin::OwnerRequest, 1, None), Memory::Own);
        assert_eq!(memory(Origin::ConnectorEvent, 0, None), Memory::Own);
        assert_eq!(memory(Origin::SocialEvent, 0, None), Memory::Own);
    }

    #[test]
    fn a_routine_run_and_its_tasks_reach_task_grants_and_webhooks_reach_none() {
        let reach = |origin, owner_chat| GrantReach::of(origin, owner_chat);
        assert_eq!(reach(Origin::OwnerRequest, true), GrantReach::OwnerChat);
        assert_eq!(reach(Origin::OwnerRequest, false), GrantReach::Tasks);
        for owner_chat in [true, false] {
            assert_eq!(
                reach(Origin::RoutineRun, owner_chat),
                GrantReach::Tasks,
                "a routine's own turn sits in the owner chat yet never reaches every grant"
            );
            for origin in [
                Origin::WebhookWake,
                Origin::ConnectorEvent,
                Origin::SocialEvent,
                Origin::Other,
            ] {
                assert_eq!(reach(origin, owner_chat), GrantReach::Nothing, "{origin:?}");
            }
        }
        let routine = PermissionTarget::Routine {
            routine_id: uuid::Uuid::nil(),
            prompt_revision: 1,
            kind: Kind::Destructive,
        };
        assert!(
            !GrantReach::Tasks.covers(&routine)
                && !GrantReach::Tasks.covers(&PermissionTarget::Bot),
            "a routine's destructive grant never carries over to its tasks"
        );
    }
}
