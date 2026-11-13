//! A bot's request that the owner take over its computer. While it waits,
//! a bar above the composer offers Take over. Once taken, the bar and a
//! panel beside the expanded screen show what the bot asked for, with a
//! Hand back button that continues the bot. A one-line row in the
//! transcript records how it ended.

use crate::controls::{Control as _, Scale, Tier};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use super::{APP_CONTEXT, ElseApp, TakeOverComputer};
use crate::api::{Bot, ClientError, HandoffAction};
use crate::model::{ComputerHandoff, HandoffStatus};

impl ElseApp {
    fn open_handoff(&self) -> Option<(Uuid, ComputerHandoff)> {
        let view = self.view.as_ref()?;
        let handoff = view.conversation.as_ref()?.open_handoff()?.clone();
        Some((view.bot_id, handoff))
    }

    /// Open the bot's screen in control. The first time, record that the
    /// owner took over; the bot keeps waiting until they hand it back.
    pub(super) fn take_over_computer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((bot, handoff)) = self.open_handoff() else {
            return;
        };
        if self.selected != Some(bot) {
            return;
        }
        self.details_tab = super::computer::DetailsTab::Computer;
        if self.settings_open {
            self.ensure_settings(window, cx);
        } else {
            self.toggle_settings(window, cx);
        }
        if let Some(computer) = self.computer.clone() {
            computer.update(cx, |view, cx| view.take_over(window, cx));
        }
        if handoff.status == HandoffStatus::Pending {
            self.settle_handoff(bot, handoff.id, HandoffAction::Take, window, cx);
        }
    }

    /// Return control to the bot and let its waiting turn continue.
    fn hand_back_computer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((bot, handoff)) = self.open_handoff() else {
            return;
        };
        if let Some(computer) = self.computer.clone() {
            computer.update(cx, |view, cx| view.collapse(cx));
        }
        self.settle_handoff(bot, handoff.id, HandoffAction::HandBack, window, cx);
    }

    fn settle_handoff(
        &mut self,
        bot: Uuid,
        handoff: Uuid,
        action: HandoffAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.settle_handoff(bot, handoff, action).await;
            let _ = this.update(cx, |app, cx| {
                match result {
                    Ok(()) => return,
                    Err(ClientError::Unauthorized) => {
                        app.connection = super::Connection::InvalidCredentials(
                            ClientError::Unauthorized.to_string(),
                        );
                    }
                    Err(err) => app.notice = Some(err.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn hand_back_button(&self, id: Uuid, cx: &mut Context<Self>) -> Button {
        let connected = self.connection == super::Connection::Connected;
        Button::new(SharedString::from(format!("hand-back-{id}")))
            .control(Tier::Primary, Scale::Compact, !connected, cx)
            .label("Hand back")
            .on_click(cx.listener(|app, _, window, cx| app.hand_back_computer(window, cx)))
    }

    pub(super) fn render_handoff_bar(
        &self,
        bot: &Bot,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (bot_id, handoff) = self.open_handoff().filter(|(id, _)| *id == bot.id)?;
        let taken = handoff.status == HandoffStatus::Taken;
        let theme = cx.theme();
        let connected = self.connection == super::Connection::Connected;
        let id = handoff.id;
        let title = if taken {
            format!("You have {}'s computer", bot.name)
        } else {
            format!("{} is waiting on you", bot.name)
        };
        let open = Button::new(SharedString::from(format!("take-over-{id}")))
            .control(Tier::Secondary, Scale::Compact, !connected, cx)
            .label(if taken { "Open screen" } else { "Take over" })
            .map(|button| {
                if taken {
                    button.ghost()
                } else {
                    button.primary()
                }
            })
            .children(
                Kbd::binding_for_action(&TakeOverComputer, Some(APP_CONTEXT), window)
                    .map(|kbd| div().flex_none().text_xs().child(kbd.appearance(false))),
            )
            .on_click(cx.listener(|app, _, window, cx| app.take_over_computer(window, cx)));
        let row = h_flex()
            .id(SharedString::from(format!("handoff-bar-{id}")))
            .w_full()
            .min_w_0()
            .gap_3()
            .items_center()
            .px_3()
            .py_2()
            .rounded(theme.radius)
            .bg(theme.warning.opacity(0.12))
            .child(
                div()
                    .flex_none()
                    .text_color(theme.warning)
                    .child(Icon::new(IconName::Laptop).small()),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .line_clamp(2)
                            .child(handoff.reason.clone()),
                    ),
            )
            .child(open);
        Some(
            if taken {
                row.child(self.hand_back_button(id, cx))
            } else {
                row.child(
                    Button::new(SharedString::from(format!("handoff-decline-{id}")))
                        .control(Tier::Ghost, Scale::Compact, !connected, cx)
                        .label("Not now")
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.settle_handoff(bot_id, id, HandoffAction::Dismiss, window, cx)
                        })),
                )
            }
            .into_any_element(),
        )
    }

    /// Beside the expanded screen while the owner holds it: what the bot
    /// asked for, and Hand back.
    pub(super) fn render_handoff_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (bot_id, handoff) = self.open_handoff()?;
        if handoff.status != HandoffStatus::Taken || self.selected != Some(bot_id) {
            return None;
        }
        let name = self
            .bots
            .iter()
            .find(|b| b.id == bot_id)
            .map_or("The bot", |b| b.name.as_str())
            .to_owned();
        let theme = cx.theme();
        let since = handoff
            .taken_at
            .unwrap_or(handoff.created_at)
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();
        Some(
            v_flex()
                .id("handoff-panel")
                .flex_none()
                .w(px(280.))
                .h_full()
                .gap_3()
                .p_4()
                .border_l_1()
                .border_color(theme.border)
                .bg(theme.sidebar)
                .child(
                    h_flex()
                        .gap_2()
                        .items_start()
                        .child(
                            div()
                                .flex_none()
                                .mt(px(2.))
                                .text_color(theme.warning)
                                .child(Icon::new(IconName::Laptop).small()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!("{name} asked you to")),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .pl_2()
                        .border_l_2()
                        .border_color(theme.warning.opacity(0.5))
                        .child(handoff.reason.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "You took over at {since}. {name} waits until you hand it back."
                        )),
                )
                .child(div().flex_1())
                .child(self.hand_back_button(handoff.id, cx).w_full())
                .into_any_element(),
        )
    }

    pub(super) fn render_handoff_row(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self.view.as_ref() else {
            return div().into_any_element();
        };
        let Some(handoff) = view
            .conversation
            .as_ref()
            .and_then(|c| c.computer_handoff(id))
        else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let name = self
            .bots
            .iter()
            .find(|b| b.id == view.bot_id)
            .map_or("The bot", |b| b.name.as_str());
        let outcome = match handoff.status {
            HandoffStatus::Pending => "Waiting",
            HandoffStatus::Taken => "You have control",
            HandoffStatus::HandedBack => "Handed back",
            HandoffStatus::Dismissed => "Declined",
            HandoffStatus::Skipped | HandoffStatus::Cancelled => "Withdrawn",
        };
        let at = handoff
            .created_at
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();
        let head = h_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_none()
                    .text_color(if handoff.status.is_open() {
                        theme.warning
                    } else {
                        theme.muted_foreground
                    })
                    .child(Icon::new(IconName::Laptop).xsmall()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(format!("{name} asked you to take over its computer")),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(theme.muted_foreground)
                    .child(at),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(theme.muted_foreground)
                    .child(outcome),
            );
        v_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .px_3()
            .py_1p5()
            .rounded(theme.radius)
            .border_1()
            .border_color(if handoff.status.is_open() {
                theme.warning.opacity(0.4)
            } else {
                theme.border
            })
            .text_xs()
            .child(head)
            .child(
                div()
                    .ml(px(20.))
                    .pl_2()
                    .border_l_2()
                    .border_color(theme.border)
                    .text_color(if handoff.status.is_open() {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    })
                    .child(handoff.reason.clone()),
            )
            .into_any_element()
    }
}
