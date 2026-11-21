//! `mail_disconnect`: the owner removes an inbox a bot can read.

use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::remove_account;
use crate::connectors::store;
use crate::connectors::types::MailErrorCode;
use crate::grants::Grant;
use crate::http::AppState;
use crate::http::owner_tools::{Card, Declined, ask};
use crate::http::tool_result::{Done, Refusal, ToolResult};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DisconnectRequest {
    account_id: Uuid,
    reason: Option<String>,
}

/// Asks the owner to remove an inbox the bot can read, on a card no saved
/// permission covers, and removes it once they approve.
pub(super) async fn disconnect_inbox(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    use crate::app::questions::{EmailRemoval, MAX_TITLE, Spec};
    let request = serde_json::from_value::<DisconnectRequest>(args.clone())
        .ok()
        .filter(|r| r.reason.as_ref().is_none_or(|r| r.len() <= MAX_TITLE));
    let Some(request) = request else {
        return Err(Refusal::text(
            "Pass an account_id from mail_accounts and optionally a reason of at most 500 bytes.",
        ));
    };
    let manager = &state.connectors;
    let id = request.account_id;
    let not_connected = || {
        Ok(Done::json(json!({"status":"not_connected","account_id":id,
                "next":"This inbox is not connected, so there is nothing to remove. It may \
                    already be removed; call mail_accounts to see the connected inboxes."})))
    };
    let (account, permissions, readers) = tokio::join!(
        store::account(&state.db, id),
        store::permissions(&state.db, id, grant.bot_id),
        store::readers(&state.db, id),
    );
    let (account, permissions, readers) = match (account, permissions, readers) {
        (Ok(account), Ok(permissions), Ok(readers)) => (account, permissions, readers),
        (Err(error), ..) | (_, Err(error), _) | (.., Err(error)) => {
            tracing::warn!(%error, "reading an inbox to remove failed");
            return Err(Refusal::text(MailErrorCode::MailUnavailable.as_str()));
        }
    };
    let Some(account) = account else {
        return not_connected();
    };
    if !permissions.is_some_and(|p| p.read) {
        return Err(Refusal::text(
            "mail_forbidden: you can only remove an inbox you can read.",
        ));
    }
    let address = account.config.address;
    let spec = Spec::email_removal(
        EmailRemoval {
            account_id: id,
            address: address.clone(),
        },
        removal_context(request.reason.as_deref(), &readers),
    );
    let card = Card::approval(
        "Tell the owner briefly which inbox this removes and why, then end this attempt now. If \
         they approve, call mail_disconnect again with the same account_id.",
    );
    let declined = Declined::Status(
        "The owner denied removing this inbox. Do not remove it or ask again in this request.",
    );
    ask(state, grant, &spec, card, declined).await?;
    match remove_account(state, manager, id).await {
        Ok(true) => Ok(
            Done::json(json!({"status":"removed","account_id":id,"address":address,
                    "next":"Tell the owner the inbox is removed for every bot and its triggers \
                        stopped. To connect it again, call mail_connect with its provider and \
                        address; it becomes a new account."}))
            .logged(format!("removed inbox {id}")),
        ),
        Ok(false) => not_connected(),
        Err(error) => {
            tracing::warn!(%error, "removing an inbox from a turn failed");
            Err(Refusal::text(
                "mail_unavailable: the inbox was not removed; tell the owner they can \
                         remove it in Settings, Email.",
            ))
        }
    }
}

/// What the owner reads on a removal card: who loses the inbox and what is
/// kept. Each list of bot names stops at [`NAMES_BUDGET`] bytes so the card
/// fits [`crate::app::questions::MAX_CONTEXT`] with any number of bots.
fn removal_context(reason: Option<&str>, readers: &[(String, bool)]) -> String {
    let mut lines: Vec<String> = reason
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_owned)
        .into_iter()
        .collect();
    lines.push(format!(
        "Bots that lose access: {}.",
        name_list(readers.iter().map(|(name, _)| name.as_str()))
    ));
    let woken: Vec<&str> = readers
        .iter()
        .filter(|(_, wakes)| *wakes)
        .map(|(name, _)| name.as_str())
        .collect();
    if !woken.is_empty() {
        lines.push(format!(
            "Mail triggers that stop: {}.",
            name_list(woken.into_iter())
        ));
    }
    lines.push(
        "The saved sign-in is erased. Messages stay in the mailbox, and you can connect it again."
            .to_owned(),
    );
    lines.join("\n")
}

const NAMES_BUDGET: usize = 1200;

fn name_list<'a>(names: impl ExactSizeIterator<Item = &'a str>) -> String {
    let total = names.len();
    let mut shown = String::new();
    let mut count = 0;
    for name in names {
        if !shown.is_empty() && shown.len() + name.len() + 2 > NAMES_BUDGET {
            break;
        }
        if !shown.is_empty() {
            shown.push_str(", ");
        }
        shown.push_str(name);
        count += 1;
    }
    match total - count {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_removal_card_fits_its_context_with_many_long_bot_names() {
        let readers: Vec<(String, bool)> = (0..200)
            .map(|i| (format!("{i:03}{}", "x".repeat(77)), true))
            .collect();
        let context = removal_context(
            Some(&"r".repeat(crate::app::questions::MAX_TITLE)),
            &readers,
        );
        assert!(
            context.len() <= crate::app::questions::MAX_CONTEXT,
            "{} bytes",
            context.len()
        );
        assert!(context.contains("more."), "{context}");
        let few = removal_context(None, &[("Inbox".into(), true), ("Growth".into(), false)]);
        assert_eq!(
            few,
            "Bots that lose access: Inbox, Growth.\nMail triggers that stop: Inbox.\nThe saved \
             sign-in is erased. Messages stay in the mailbox, and you can connect it again."
        );
    }
}
