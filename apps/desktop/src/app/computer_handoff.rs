//! A bot's request that the owner take over its computer: a bar pinned
//! above the composer while it is pending, and a one-line row in the
//! transcript that records how it ended.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Icon, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use super::{APP_CONTEXT, ElseApp, TakeOverComputer};
use crate::api::{Bot, ClientError};
use crate::model::{ComputerHandoff, HandoffStatus};

impl ElseApp {
    fn pending_handoff(&self) -> Option<(Uuid, ComputerHandoff)> {
        let view = self.view.as_ref()?;
        let handoff = view.conversation.as_ref()?.pending_handoff()?.clone();
        Some((view.bot_id, handoff))
    }

    /// Open the bot's screen in control and record that the owner took over.
    pub(super) fn take_over_computer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((bot, handoff)) = self.pending_handoff() else {
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
        self.settle_handoff(bot, handoff.id, true, window, cx);
    }

    fn settle_handoff(
        &mut self,
        bot: Uuid,
        handoff: Uuid,
        take: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.settle_handoff(bot, handoff, take).await;
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

    pub(super) fn render_handoff_bar(
        &self,
        bot: &Bot,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (bot_id, handoff) = self.pending_handoff().filter(|(id, _)| *id == bot.id)?;
        let theme = cx.theme();
        let connected = self.connection == super::Connection::Connected;
        let id = handoff.id;
        Some(
            h_flex()
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
                                .child(format!("{} is waiting on you", bot.name)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(handoff.reason.clone()),
                        ),
                )
                .child(
                    Button::new(SharedString::from(format!("take-over-{id}")))
                        .focus_ring(false)
                        .when(connected, |button| button.cursor_pointer())
                        .label("Take over")
                        .primary()
                        .small()
                        .disabled(!connected)
                        .children(
                            Kbd::binding_for_action(&TakeOverComputer, Some(APP_CONTEXT), window)
                                .map(|kbd| {
                                    div().flex_none().text_xs().child(kbd.appearance(false))
                                }),
                        )
                        .on_click(
                            cx.listener(|app, _, window, cx| app.take_over_computer(window, cx)),
                        ),
                )
                .child(
                    Button::new(SharedString::from(format!("handoff-later-{id}")))
                        .focus_ring(false)
                        .when(connected, |button| button.cursor_pointer())
                        .label("Later")
                        .ghost()
                        .small()
                        .disabled(!connected)
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.settle_handoff(bot_id, id, false, window, cx)
                        })),
                )
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
            HandoffStatus::Taken => "You took over",
            HandoffStatus::Dismissed => "Dismissed",
            HandoffStatus::Skipped => "Withdrawn",
        };
        let at = handoff
            .created_at
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();
        h_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .items_center()
            .px_3()
            .py_1p5()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .text_xs()
            .child(
                div()
                    .flex_none()
                    .text_color(if handoff.status == HandoffStatus::Pending {
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
            )
            .into_any_element()
    }
}
