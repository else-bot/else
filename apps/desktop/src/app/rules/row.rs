//! The one-line note in the transcript when the bot edits a rule.

use super::{RulesTab, change_label, when};
use crate::app::conversation::on_app;
use else_wire::rules::RulesUpdate;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

impl crate::app::conversation::ConversationView {
    pub(in crate::app) fn render_rules_update(
        &self,
        item: Uuid,
        bot: Uuid,
        update: &RulesUpdate,
        stamp: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let author = self.inputs.bots.iter().find(|b| b.id == bot);
        let current = self.conversation.as_ref().map(|c| c.rules_version()) == Some(update.version)
            && update.undone_at.is_none();
        let restoring = self.inputs.restoring_rules;
        let section = update.section.clone();
        let shown = update.version;
        let view = crate::app::action_card::event_action(
            SharedString::from(format!("rules-view-{item}")),
            "View",
            false,
            cx,
        )
        .accessibility_label(format!("View the change to {}", update.section))
        .on_click(on_app(&self.app, move |app, _, window, cx| {
            app.open_rules(
                bot,
                Some(section.clone()),
                RulesTab::History,
                Some(shown),
                window,
                cx,
            )
        }));
        let previous = update.version - 1;
        let undo = current.then(|| {
            crate::app::action_card::event_action(
                SharedString::from(format!("rules-undo-{item}")),
                "Undo",
                restoring,
                cx,
            )
            .loading(restoring)
            .accessibility_label(format!("Undo the change to {}", update.section))
            .on_click(on_app(&self.app, move |app, _, window, cx| {
                app.restore_rules(bot, previous, shown, window, cx)
            }))
        });
        let undone = update.undone_at;
        let line = crate::app::action_card::event_row(&theme)
            .id(SharedString::from(format!("message-{item}")))
            .child(self.event_faces(&format!("rules-{item}"), author.map(|b| b.id)))
            .child(
                crate::app::action_card::event_verb(&theme, change_label(update.change))
                    .when(undone.is_some(), |verb| verb.line_through()),
            )
            .child(
                crate::app::action_card::event_object(&theme, update.section.clone())
                    .when(undone.is_some(), |object| {
                        object.line_through().text_color(theme.muted_foreground)
                    }),
            )
            .when_some(undone, |row, at| {
                row.child(
                    div()
                        .flex_none()
                        .text_size(px(12.5))
                        .text_color(crate::app::message_row::meta_color(&theme))
                        .child(format!("Undone · {}", when(at))),
                )
            })
            .child(crate::app::action_card::event_end(
                &theme,
                crate::app::action_card::EventEnd::Actions(
                    std::iter::once(view).chain(undo).collect(),
                ),
                crate::app::action_card::Disclosure::None,
            ));
        let _ = stamp;
        line.into_any_element()
    }
}
