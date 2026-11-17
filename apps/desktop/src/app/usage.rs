//! The Usage row of a bot's details: one button that opens Settings > Usage
//! on this bot.

use super::ElseApp;
use crate::controls::{Control as _, Scale, Tier};
use gpui_kit::component::button::Button;
use gpui_kit::component::h_flex;
use gpui_kit::*;

impl ElseApp {
    pub(super) fn render_bot_usage(&self, cx: &mut Context<Self>) -> AnyElement {
        let open = Button::new("open-bot-usage")
            .control(Tier::Secondary, Scale::Standard, false, cx)
            .label("Open")
            .accessibility_label("Open this bot's usage in Settings")
            .on_click(cx.listener(|app, _, window, cx| {
                app.sidebar_focus.focus(window, cx);
                window.dispatch_action(Box::new(crate::connection::OpenUsageSettings), cx);
            }));
        super::settings::section_frame(cx)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .child(crate::appearance::section_label("Usage", cx).flex_1())
                    .child(open),
            )
            .into_any_element()
    }
}
