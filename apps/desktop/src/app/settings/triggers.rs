use super::{BotPanel, automation_section};
use crate::app::delete_dialog::{DeleteTarget, open_delete_confirmation};
use crate::app::panel::{alert, item_name, meta, meta_line};
use crate::controls::toggle;
use chrono::{DateTime, Utc};
use else_client::request::{Request, Settle};
use else_wire::conversation::BotExtras;
use else_wire::schedule::webhook_pace as pace;
use else_wire::socials::Provider;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Icon, Sizable as _, h_flex, v_flex,
};
use gpui_kit::*;
use uuid::Uuid;

/// A change to one trigger. An accepted change stays until the stream
/// reports it; `Enable` holds the switch at the value sent until then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TriggerChange {
    Enable(bool),
    Delete,
}

impl BotPanel {
    pub(super) fn sync_triggers(&mut self, automations: &BotExtras) {
        let cards = trigger_cards(automations);
        self.triggers.retain(|key, save| {
            let enabled = cards
                .iter()
                .find(|card| card.source.key() == *key)
                .map(|card| card.enabled);
            still_pending(save, enabled)
        });
    }

    fn changeable_trigger(&self, source: TriggerSource) -> Option<TriggerCard> {
        if !self.connected() {
            return None;
        }
        if self.triggers.get(&source.key()).is_some_and(Request::busy) {
            return None;
        }
        trigger_cards(self.automations()?)
            .into_iter()
            .find(|card| card.source == source)
    }

    /// Pause or resume a trigger, or delete it with `None`.
    fn change_trigger(
        &mut self,
        source: TriggerSource,
        enabled: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.changeable_trigger(source).is_none() {
            return;
        }
        let change = match enabled {
            Some(enabled) => TriggerChange::Enable(enabled),
            None => TriggerChange::Delete,
        };
        let Some(attempt) = self.triggers.entry(source.key()).or_default().begin(change) else {
            return;
        };
        cx.notify();
        let (client, bot) = (self.client.clone(), self.bot);
        let app = self.app.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = match (source, enabled) {
                (TriggerSource::Mail { account }, Some(wake)) => {
                    client.set_email_wake(account, bot, wake).await
                }
                (TriggerSource::Mail { account }, None) => {
                    client.delete_email_trigger(account, bot).await
                }
                (TriggerSource::Social { id }, Some(enabled)) => client
                    .set_social_trigger_enabled(bot, id, enabled)
                    .await
                    .map(|_| ()),
                (TriggerSource::Social { id }, None) => client.delete_social_trigger(bot, id).await,
                (TriggerSource::Webhook { id }, Some(enabled)) => {
                    client.set_webhook_enabled(bot, id, enabled).await
                }
                (TriggerSource::Webhook { id }, None) => client.delete_webhook(bot, id).await,
            };
            if let Err(err) = &result {
                crate::app::report_unauthorized(&app, err, cx);
            }
            let _ = this.update(cx, |panel, cx| {
                let action = if enabled.is_some() { "save" } else { "delete" };
                let outcome = panel
                    .triggers
                    .get_mut(&source.key())
                    .map_or(Settle::Stale, |save| {
                        save.settle_as(attempt, &result, |err| format!("Couldn't {action}. {err}"))
                    });
                if outcome == Settle::Stale {
                    return;
                }
                // A change that matched the server's value publishes no
                // update to release the switch.
                if let Some(automations) = panel.inputs.automations.clone() {
                    panel.sync_triggers(&automations);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn open_trigger_delete_dialog(
        &mut self,
        source: TriggerSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(card) = self.changeable_trigger(source) else {
            return;
        };
        let target = match source {
            TriggerSource::Webhook { .. } => DeleteTarget::Webhook(card.title),
            TriggerSource::Mail { .. } | TriggerSource::Social { .. } => {
                DeleteTarget::Trigger(card.delete_name)
            }
        };
        open_delete_confirmation(target, window, cx, move |panel, window, cx| {
            panel.change_trigger(source, None, window, cx);
        });
    }

    pub(super) fn render_triggers(&self, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.automations().map(|automations| {
            trigger_cards(automations)
                .into_iter()
                .map(|card| {
                    let save = self.triggers.get(&card.source.key());
                    self.render_trigger(card, save, cx)
                })
                .collect()
        });
        automation_section(
            "Triggers",
            "No triggers yet. Ask the bot in chat to react to something, for example \
             \"wake up when new mail arrives from my accountant\", \"answer mentions in \
             our Telegram group\", or \"give me a webhook for failed deploys\". Its \
             triggers appear here.",
            rows,
            cx,
        )
    }

    fn render_trigger(
        &self,
        card: TriggerCard,
        save: Option<&Request<TriggerChange>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let source = card.source;
        let key = source.key();
        let pending = save.and_then(Request::pending);
        let checked = match pending {
            Some(TriggerChange::Enable(enabled)) => *enabled,
            _ => card.enabled,
        };
        let disabled = save.is_some_and(Request::busy) || !self.connected();
        let switch = toggle(
            ElementId::Name(format!("trigger-{key}").into()),
            checked,
            disabled,
            cx,
        )
        .accessibility_label(format!("Run the trigger for {}", card.title))
        .on_click(cx.listener(move |panel, enabled: &bool, window, cx| {
            panel.change_trigger(source, Some(*enabled), window, cx)
        }));
        let delete = Button::new(ElementId::Name(format!("trigger-delete-{key}").into()))
            .focus_ring(false)
            .cursor_pointer()
            .ghost()
            .opacity(0.)
            .group_hover("automation-card", |style| style.opacity(1.))
            .focus(|style| style.opacity(1.))
            .xsmall()
            .icon(gpui_kit::assets::IconName::Trash)
            .accessibility_label(format!("Delete the trigger for {}", card.title))
            .loading(pending == Some(&TriggerChange::Delete))
            .disabled(disabled)
            .on_click(cx.listener(move |panel, _, window, cx| {
                panel.open_trigger_delete_dialog(source, window, cx)
            }));
        let error = save.and_then(Request::error).map(str::to_owned);
        let mut facts = card.pills;
        if !checked {
            facts.push("Paused".to_owned());
        }
        if let Some(at) = card.last_woke {
            facts.push(else_wire::schedule::last_woke(at));
        }
        // Same anatomy as a routine: the name over one meta line, beside a
        // centred switch. What the trigger asks for follows as its body.
        let mut tokens = vec![
            h_flex()
                .flex_none()
                .gap_1p5()
                .items_center()
                .text_color(theme.muted_foreground)
                .child(card.icon.clone().size_3().flex_none())
                .child(meta(facts.first().cloned().unwrap_or_default(), cx).whitespace_nowrap())
                .into_any_element(),
        ];
        tokens.extend(facts.into_iter().skip(1).map(|fact| {
            meta(fact, cx)
                .flex_none()
                .whitespace_nowrap()
                .into_any_element()
        }));
        v_flex()
            .w_full()
            .py_0()
            .gap_1()
            .group("automation-card")
            .child(
                // The switch lines up with the name, the row's first line.
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(item_name(card.title.clone()))
                            .child(meta_line(tokens, cx)),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .items_center()
                            .child(delete)
                            .child(switch),
                    ),
            )
            .child(meta(card.instructions, cx).pt_0p5())
            .children(error.map(|error| alert(error, theme.danger, cx)))
            .into_any_element()
    }
}

/// What a trigger card changes: each kind has its own routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TriggerSource {
    Mail { account: Uuid },
    Social { id: Uuid },
    Webhook { id: Uuid },
}

impl TriggerSource {
    /// Unique across kinds, since every ID is a random UUID.
    fn key(self) -> Uuid {
        match self {
            Self::Mail { account } => account,
            Self::Social { id } | Self::Webhook { id } => id,
        }
    }
}

/// One trigger as its sidebar card shows it.
struct TriggerCard {
    source: TriggerSource,
    icon: Icon,
    title: String,
    /// How the delete dialog names it.
    delete_name: String,
    instructions: String,
    pills: Vec<String>,
    enabled: bool,
    last_woke: Option<DateTime<Utc>>,
}

/// Every trigger that can wake the bot: mail, then chats, then webhooks.
fn trigger_cards(automations: &BotExtras) -> Vec<TriggerCard> {
    let mail = automations.mail_triggers.iter().map(|trigger| {
        let mut pills = vec!["Email".to_owned(), "New mail".to_owned()];
        if trigger.state == else_wire::email::AccountState::Paused {
            pills.push("inbox paused".to_owned());
        }
        TriggerCard {
            source: TriggerSource::Mail {
                account: trigger.account_id,
            },
            icon: Icon::new(gpui_kit::assets::IconName::Mail),
            title: trigger.address.clone(),
            delete_name: format!("the trigger for {}", trigger.address),
            instructions: trigger
                .instructions
                .clone()
                .unwrap_or_else(|| "Wakes the bot when new mail arrives.".to_owned()),
            pills,
            enabled: trigger.wake,
            last_woke: trigger
                .last_woke_at
                .as_deref()
                .and_then(|at| at.parse().ok()),
        }
    });
    let social = automations.social_triggers.iter().map(|trigger| {
        let connection = trigger
            .username
            .as_ref()
            .map(|username| format!("@{username}"))
            .unwrap_or_else(|| trigger.connection_name.clone());
        TriggerCard {
            source: TriggerSource::Social { id: trigger.id },
            icon: match trigger.provider {
                Provider::Telegram => Icon::new(gpui_kit::assets::IconName::Send),
                Provider::Discord => Icon::empty().path(crate::DISCORD_ICON),
                Provider::WhatsApp | Provider::Unknown => {
                    Icon::new(gpui_kit::assets::IconName::MessageSquare)
                }
            },
            title: trigger.title().to_owned(),
            delete_name: match trigger.destination {
                Some(_) => format!("the trigger for {}", trigger.title()),
                None => "the trigger for any chat".to_owned(),
            },
            instructions: trigger.instructions.clone(),
            pills: vec![
                trigger.provider.label().to_owned(),
                connection,
                trigger.wake.label().to_owned(),
            ],
            enabled: trigger.enabled,
            last_woke: trigger
                .last_woke_at
                .as_deref()
                .and_then(|at| at.parse().ok()),
        }
    });
    let webhooks = automations.webhooks.iter().map(|webhook| TriggerCard {
        source: TriggerSource::Webhook { id: webhook.id },
        icon: Icon::new(gpui_kit::assets::IconName::Webhook),
        title: webhook.name.clone(),
        delete_name: webhook.name.clone(),
        instructions: webhook.prompt.clone(),
        pills: vec!["Webhook".to_owned(), pace(webhook.min_interval_minutes)],
        enabled: webhook.enabled,
        last_woke: webhook.last_wake.as_ref().map(|wake| wake.woke_at),
    });
    mail.chain(social).chain(webhooks).collect()
}

/// Whether a trigger's local change still waits for the server: a save until
/// the trigger reports the saved state, a failure until the trigger is gone.
fn still_pending(save: &Request<TriggerChange>, current: Option<bool>) -> bool {
    match (save.pending(), current) {
        (_, None) => false,
        (Some(TriggerChange::Enable(enabled)), Some(current)) => *enabled != current,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{Request, TriggerChange, pace, still_pending, trigger_cards};
    use crate::model::Conversation;
    use else_wire::conversation::Snapshot;
    use else_wire::socials::Trigger;
    use serde_json::json;
    use uuid::Uuid;

    fn trigger(enabled: bool) -> Trigger {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::from_u128(1), "connection_id": Uuid::from_u128(2), "bot_id": Uuid::from_u128(3),
            "provider": "telegram", "connection_name": "Proliquid", "username": "proliquid_bot",
            "destination": null, "destination_name": null, "wake": "mentions",
            "instructions": "Answer questions.", "enabled": enabled,
            "created_at": "2026-11-01T07:00:00.123+00:00", "updated_at": "2026-11-01T07:00:00.123+00:00"
        }))
        .unwrap()
    }

    #[test]
    fn a_trigger_switch_stays_busy_until_the_server_reports_the_saved_state() {
        let mut saving = Request::Idle;
        let attempt = saving.begin(TriggerChange::Enable(false)).unwrap();
        assert!(
            still_pending(&saving, Some(trigger(true).enabled)),
            "a stale event keeps it busy"
        );
        assert!(!still_pending(&saving, Some(trigger(false).enabled)));
        let _ = saving.settle(attempt, &Ok::<_, crate::api::ClientError>(()));
        assert!(
            still_pending(&saving, Some(trigger(true).enabled)),
            "an accepted save stays busy until the stream reports it"
        );
        assert!(!still_pending(&saving, Some(trigger(false).enabled)));
        let failed = Request::Failed("Couldn't save.".into());
        assert!(
            still_pending(&failed, Some(trigger(true).enabled)),
            "the error stays visible"
        );
        let mut deleting = Request::Idle;
        deleting.begin(TriggerChange::Delete);
        assert!(
            !still_pending(&deleting, None),
            "a deleted trigger drops its state"
        );
        assert!(!still_pending(&failed, None));
    }

    #[test]
    fn every_trigger_kind_becomes_one_card_in_a_stable_order() {
        let snapshot: Snapshot = serde_json::from_value(json!({
            "event_cursor": "server:bot:1", "items": [], "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 },
            "mail_triggers": [{
                "account_id": Uuid::from_u128(1), "address": "owner@example.com",
                "state": "paused", "health": "connected", "wake": false
            }],
            "social_triggers": [{
                "id": Uuid::from_u128(2), "connection_id": Uuid::from_u128(4),
                "bot_id": Uuid::from_u128(5), "provider": "telegram",
                "connection_name": "Team", "username": null, "destination": null,
                "destination_name": null, "wake": "mentions", "instructions": "Answer.",
                "enabled": true, "created_at": "2026-11-01T07:00:00Z",
                "updated_at": "2026-11-01T07:00:00Z"
            }],
            "webhooks": [{
                "id": Uuid::from_u128(3), "name": "Deploys", "prompt": "Open an issue.",
                "min_interval_minutes": 120, "enabled": true, "last_wake": null
            }]
        }))
        .unwrap();
        let conversation = Conversation::from_snapshot(snapshot).unwrap();
        let cards = trigger_cards(conversation.extras());
        let titles: Vec<_> = cards.iter().map(|card| card.title.as_str()).collect();
        assert_eq!(titles, ["owner@example.com", "Any chat", "Deploys"]);
        let mail = &cards[0];
        assert_eq!(mail.instructions, "Wakes the bot when new mail arrives.");
        assert_eq!(mail.pills, ["Email", "New mail", "inbox paused"]);
        assert!(!mail.enabled);
        assert_eq!(cards[1].delete_name, "the trigger for any chat");
        assert_eq!(cards[2].pills, ["Webhook", "at most every 2 hours"]);
        let keys: std::collections::HashSet<_> =
            cards.iter().map(|card| card.source.key()).collect();
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn webhook_paces_read_naturally() {
        assert_eq!(pace(1), "at most every minute");
        assert_eq!(pace(15), "at most every 15 min");
        assert_eq!(pace(60), "at most hourly");
        assert_eq!(pace(90), "at most every 90 min");
        assert_eq!(pace(1440), "at most every 24 hours");
    }
}
