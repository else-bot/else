use uuid::Uuid;

use crate::app::providers::{DelegationContext, Origin, TurnInput};

/// Who a turn works for and what it may do. Each turn has exactly one
/// mode, and the system prompt, the request frame, and the session
/// fingerprint all follow from it.
#[derive(Debug, Clone, Copy)]
pub(super) enum TurnMode<'a> {
    /// An untrusted external social message.
    Social,
    /// A task an email turn handed on, from the bot that handed it.
    EmailTask { sender: Option<&'a (Uuid, String)> },
    /// Incoming mail in the bot's own conversation.
    EmailEvent,
    /// A task another bot delegated, from that bot.
    Delegated { sender: Option<&'a (Uuid, String)> },
    /// The bot's own conversation.
    Owner { main: bool },
}

impl<'a> TurnMode<'a> {
    pub(super) fn of(delegation: &'a DelegationContext) -> Self {
        let sender = delegation.delegated_by.as_ref();
        match delegation.origin {
            Origin::SocialEvent => Self::Social,
            Origin::ConnectorEvent if delegation.delegated_task() => Self::EmailTask { sender },
            Origin::ConnectorEvent => Self::EmailEvent,
            _ if delegation.delegated_task() => Self::Delegated { sender },
            _ => Self::Owner {
                main: delegation.main_bot,
            },
        }
    }
}

/// The mode line in the session fingerprint: a native session resumes only
/// in the same mode with the same routine permission. A social event's
/// line is empty, and a sender name's own line breaks end it.
pub fn fingerprint_tag(input: &TurnInput) -> String {
    let mut tag = mode_line(input);
    tag.truncate(tag.find('\n').unwrap_or(tag.len()));
    tag
}

fn mode_line(input: &TurnInput) -> String {
    let delegation = &input.delegation;
    let routines = if delegation.can_manage_routines() {
        "own-routines"
    } else {
        "no-routines"
    };
    match TurnMode::of(delegation) {
        TurnMode::Social => String::new(),
        TurnMode::EmailTask { sender } => format!(
            "mode:email-task{} no-delegation no-computer no-routines",
            from(sender)
        ),
        TurnMode::EmailEvent => "mode:external-email no-computer no-routines".into(),
        TurnMode::Delegated { sender } => {
            format!("mode:delegated-task{} {routines}", from(sender))
        }
        TurnMode::Owner { main } => {
            format!("mode:{} {routines}", if main { "main" } else { "peer" })
        }
    }
}

/// " from NAME (bot_id ID)", or nothing.
fn from(sender: Option<&(Uuid, String)>) -> String {
    sender.map_or_else(String::new, |(id, name)| {
        format!(" from {name} (bot_id {id})")
    })
}
