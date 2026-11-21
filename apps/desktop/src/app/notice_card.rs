use crate::api::ClientError;
use crate::app::conversation::ConversationView;
use crate::app::transcript::CardKey;
use crate::controls::{Control as _, Scale, Tier};
use else_wire::notices::{OwnerNotice, OwnerNoticeKind, OwnerNoticeStatus};
use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::Button;
use gpui_kit::*;
use uuid::Uuid;

impl ConversationView {
    fn dismiss_owner_notice(
        &mut self,
        bot: Uuid,
        notice: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (client, app) = (self.client.clone(), self.app.clone());
        cx.spawn_in(window, async move |this, cx| {
            let result = client.dismiss_owner_notice(bot, notice).await;
            if let Err(err) = &result {
                super::report_unauthorized(&app, err, cx);
            }
            let _ = this.update_in(cx, |view, window, cx| {
                match result {
                    Ok(notice) => {
                        if let Some(c) = view.conversation.as_mut() {
                            c.apply_owner_notice(notice);
                        }
                        view.sync_rows(window, cx);
                    }
                    Err(ClientError::Unauthorized) => {}
                    // The owner's message settled it first; the stream shows that.
                    Err(err) if err.code() == Some("owner_notice_settled") => {}
                    Err(err) => view.notice(err.to_string(), cx),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_notice_card(
        &self,
        id: Uuid,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let c = self.conversation.as_ref()?;
        let notice = c.owner_notice(id)?;
        if notice.status == OwnerNoticeStatus::Superseded {
            return None;
        }
        let name = self
            .inputs
            .bots
            .iter()
            .find(|b| b.id == notice.bot_id)
            .map_or("The bot", |b| b.name.as_str());
        let receipt = c.task_receipt(notice.turn_id);
        let sender = receipt
            .and_then(|item| item.bot_task.as_ref())
            .map(|task| &task.sender_name);
        // A received task's ask is its own row. Right under its exchange,
        // the exchange's header names the task; further down, the card does.
        let here = self.row_index(super::RowKey::Card(CardKey::OwnerNotice(id)));
        let exchange =
            receipt.and_then(|r| Some((r.id, self.row_index(super::RowKey::Item(r.id))?)));
        let below = exchange.is_some_and(|(_, ix)| here == Some(ix + 1));
        let subtitle = sender
            .filter(|_| !below)
            .map_or_else(String::new, |sender| format!("From {sender}'s task"));
        let task = exchange.filter(|_| !below).map(|(item, _)| item);
        Some(self.ask_card(notice, name, subtitle, task, cx))
    }

    /// An owner notice as a card: the bot's ask in its own words with the
    /// owner's actions, or one muted line once settled. `task` is the
    /// exchange item whose task its "Show task" button reveals.
    pub(super) fn ask_card(
        &self,
        notice: &OwnerNotice,
        name: &str,
        subtitle: String,
        task: Option<Uuid>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let id = notice.id;
        let settled = match notice.status {
            OwnerNoticeStatus::Pending | OwnerNoticeStatus::Superseded => None,
            OwnerNoticeStatus::Replied => Some("You replied"),
            OwnerNoticeStatus::Dismissed => Some("Handled"),
            OwnerNoticeStatus::Other(_) => Some("Notice status unavailable"),
        };
        if let Some(label) = settled {
            return div()
                .w_full()
                .min_w_0()
                .text_xs()
                .text_color(theme.muted_foreground)
                .truncate()
                .child(format!("{label} · {}", notice.summary))
                .into_any_element();
        }
        let bot = notice.bot_id;
        let title = match notice.kind {
            OwnerNoticeKind::Blocked => format!("{name} is blocked and needs you"),
            OwnerNoticeKind::Decision => format!("{name} needs a decision from you"),
            OwnerNoticeKind::Other(_) => format!("{name} needs you"),
        };
        let since = crate::sidebar::status::waited(notice.created_at, chrono::Utc::now());
        let connected = self.inputs.connected;
        let here = self.bot_id == bot;
        let button = |action: &str, label: String, tier: Tier, disabled: bool| {
            Button::new(SharedString::from(format!("notice-{id}-{action}")))
                .control(tier, Scale::Compact, disabled, cx)
                .accessibility_label(label.clone())
                .label(label)
        };
        let actions = super::action_card::footer()
            .children(task.map(|item| {
                button("task", "Show task".into(), Tier::Ghost, false).on_click(
                    cx.listener(move |app, _, window, cx| app.show_exchange_task(item, window, cx)),
                )
            }))
            .child(
                button(
                    "dismiss",
                    "Mark handled".into(),
                    Tier::Secondary,
                    !connected,
                )
                .on_click(cx.listener(move |app, _, window, cx| {
                    app.dismiss_owner_notice(bot, id, window, cx);
                })),
            )
            .child({
                let summary = notice.summary.clone();
                button("reply", format!("Reply to {name}"), Tier::Primary, false).on_click(
                    self.on_app(move |app, _, window, cx| {
                        // The asking bot answers in its own chat. The reply
                        // quotes the ask, and settles only it where the
                        // server can.
                        if !here {
                            app.open_bot(bot, window, cx);
                        }
                        let settles = app.caps.as_ref().is_some_and(|caps| caps.answers_notice);
                        let quote = super::reply::Quote::ask(id, summary.clone(), settles);
                        app.add_quote(bot, quote, window, cx);
                    }),
                )
            });
        super::action_card::card(IconName::TriangleAlert, title, subtitle)
            .meta(since)
            .child(
                div()
                    .text_sm()
                    .whitespace_normal()
                    .child(notice.summary.clone()),
            )
            .children(notice.tried.as_ref().map(|tried| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("Tried · {tried}"))
            }))
            .footer(actions)
            .into_any_element()
    }
}
