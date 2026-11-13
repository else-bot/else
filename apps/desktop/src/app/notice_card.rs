use super::{Connection, ElseApp};
use crate::api::ClientError;
use crate::controls::{Control as _, Scale, Tier};
use crate::model::{OwnerNoticeKind, OwnerNoticeStatus};
use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::Button;
use gpui_kit::*;
use uuid::Uuid;

impl ElseApp {
    fn dismiss_owner_notice(
        &mut self,
        bot: Uuid,
        notice: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.dismiss_owner_notice(bot, notice).await;
            let _ = this.update_in(cx, |app, window, cx| {
                match result {
                    Ok(notice) => {
                        if let Some(view) = app.view.as_mut().filter(|v| v.bot_id == bot)
                            && let Some(c) = view.conversation.as_mut()
                        {
                            c.apply_owner_notice(notice);
                        }
                        app.sync_rows(window, cx);
                    }
                    Err(ClientError::Unauthorized) => {
                        app.connection =
                            Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
                    }
                    // The owner's message settled it first; the stream shows that.
                    Err(err) if err.code() == Some("owner_notice_settled") => {}
                    Err(err) => app.notice = Some(err.to_string()),
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
        let view = self.view.as_ref()?;
        let c = view.conversation.as_ref()?;
        let notice = c.owner_notice(id)?;
        let theme = cx.theme();
        let settled = match notice.status {
            OwnerNoticeStatus::Pending => None,
            OwnerNoticeStatus::Replied => Some("You replied"),
            OwnerNoticeStatus::Dismissed => Some("Handled"),
            OwnerNoticeStatus::Superseded => return None,
        };
        if let Some(label) = settled {
            return Some(
                div()
                    .w_full()
                    .min_w_0()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(format!("{label} · {}", notice.summary))
                    .into_any_element(),
            );
        }
        let bot = notice.bot_id;
        let name = self
            .bots
            .iter()
            .find(|b| b.id == bot)
            .map_or("The bot", |b| b.name.as_str());
        let title = match notice.kind {
            OwnerNoticeKind::Blocked => format!("{name} is blocked and needs you"),
            OwnerNoticeKind::Decision => format!("{name} needs a decision from you"),
        };
        let receipt = c.task_receipt(notice.turn_id);
        let sender = receipt
            .and_then(|item| item.bot_task.as_ref())
            .map(|task| &task.sender_name);
        let since = crate::sidebar::status::waited(notice.created_at, chrono::Utc::now());
        let subtitle = sender.map_or_else(
            || since.clone(),
            |sender| format!("From {sender}'s task · {since}"),
        );
        let anchor = receipt.and_then(|receipt| {
            view.rows.iter().find_map(|row| {
                let activity = row.activity.as_ref()?;
                activity
                    .contains(receipt.id)
                    .then(|| activity.items.first().copied())
                    .flatten()
            })
        });
        let exchange_title =
            sender.map_or_else(|| name.to_owned(), |sender| format!("{sender} & {name}"));
        let connected = self.connection == Connection::Connected;
        let button = |action: &str, label: String, tier: Tier, disabled: bool| {
            Button::new(SharedString::from(format!("notice-{id}-{action}")))
                .control(tier, Scale::Compact, disabled, cx)
                .accessibility_label(label.clone())
                .label(label)
        };
        let actions = super::action_card::footer()
            .children(anchor.map(|anchor| {
                button("open", "Open task".into(), Tier::Ghost, false).on_click(cx.listener(
                    move |app, _, window, cx| {
                        app.open_bot_exchange(anchor, exchange_title.clone(), window, cx);
                    },
                ))
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
            .child(
                button("reply", format!("Reply to {name}"), Tier::Primary, false).on_click(
                    cx.listener(|app, _, window, cx| {
                        app.composer.read(cx).focus_handle(cx).focus(window, cx);
                    }),
                ),
            );
        Some(
            super::action_card::card(IconName::TriangleAlert, title, subtitle)
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
                .into_any_element(),
        )
    }
}
