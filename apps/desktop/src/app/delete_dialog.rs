use super::ElseApp;
use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::*;
use std::rc::Rc;

pub(super) enum DeleteTarget {
    Bot(String),
    Category { name: String, bots: usize },
    Routine(String),
    Reminder(String),
    Trigger(String),
    Webhook(String),
}

struct Consequence {
    icon: IconName,
    title: SharedString,
    detail: SharedString,
}

impl Consequence {
    fn new(
        icon: IconName,
        title: impl Into<SharedString>,
        detail: impl Into<SharedString>,
    ) -> Self {
        Self {
            icon,
            title: title.into(),
            detail: detail.into(),
        }
    }

    fn render(&self, cx: &App) -> impl IntoElement {
        h_flex()
            .items_start()
            .gap_3()
            .child(
                Icon::new(self.icon)
                    .size_4()
                    .mt_0p5()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(self.title.clone()),
                    )
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child(self.detail.clone()),
                    ),
            )
    }
}

impl ElseApp {
    pub(super) fn open_delete_confirmation(
        &mut self,
        target: DeleteTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
        delete: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let (name, kind, consequences, kept) = match target {
            DeleteTarget::Bot(name) => (
                name,
                "bot",
                [
                    Consequence::new(
                        IconName::Pause,
                        "Current work stops",
                        "Any work in progress will stop.",
                    ),
                    Consequence::new(
                        IconName::MessageSquareOff,
                        "The bot leaves the sidebar",
                        "You won't be able to message it.",
                    ),
                ],
                Some("History, memory, and files are kept."),
            ),
            DeleteTarget::Category { name, bots } => (
                name,
                "category",
                [
                    Consequence::new(
                        IconName::Folder,
                        "The category is removed",
                        "Only the grouping is deleted.",
                    ),
                    Consequence::new(
                        IconName::ArrowRight,
                        match bots {
                            0 => "No bots are affected",
                            1 => "Its bot moves to Unassigned",
                            _ => "Bots move to Unassigned",
                        },
                        match bots {
                            0 => "This category has no bots.".to_owned(),
                            1 => "Its bot is kept and can still be messaged.".to_owned(),
                            n => format!("Its {n} bots are kept and can still be messaged."),
                        },
                    ),
                ],
                Some("Bots, conversations, memory, and files are kept."),
            ),
            DeleteTarget::Webhook(name) => (
                name,
                "webhook",
                [
                    Consequence::new(
                        IconName::Pause,
                        "Its URL stops working",
                        "Calls to it are refused and won't wake the bot again.",
                    ),
                    Consequence::new(
                        IconName::Clock,
                        "Current work finishes",
                        "Work already in progress won't be stopped.",
                    ),
                ],
                None,
            ),
            DeleteTarget::Trigger(name) => (
                name,
                "trigger",
                [
                    Consequence::new(
                        IconName::Pause,
                        "No more automatic reactions",
                        "This trigger leaves the list and won't wake the bot again.",
                    ),
                    Consequence::new(
                        IconName::Clock,
                        "Current work finishes",
                        "Work already in progress won't be stopped.",
                    ),
                ],
                Some("The connection stays available when you ask the bot to use it."),
            ),
            DeleteTarget::Reminder(name) => (
                name,
                "reminder",
                [
                    Consequence::new(
                        IconName::CalendarX,
                        "It won't run",
                        "The reminder leaves this list and its task won't run.",
                    ),
                    Consequence::new(
                        IconName::Clock,
                        "A current run finishes",
                        "A run already in progress won't be stopped.",
                    ),
                ],
                None,
            ),
            DeleteTarget::Routine(name) => (
                name,
                "routine",
                [
                    Consequence::new(
                        IconName::CalendarX,
                        "No more scheduled runs",
                        "The routine leaves this list and won't run again.",
                    ),
                    Consequence::new(
                        IconName::Clock,
                        "A current run finishes",
                        "A run already in progress won't be stopped.",
                    ),
                ],
                None,
            ),
        };
        let app = cx.entity().downgrade();
        let cancel = cx.focus_handle();
        let confirm = cx.focus_handle();
        let focus_once = std::cell::Cell::new(true);
        let delete = Rc::new(delete);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_once.replace(false) {
                cancel.focus(window, cx);
            }
            let theme = cx.theme();
            let [first, second] = &consequences;
            let mut body = v_flex()
                .gap_5()
                .text_xs()
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(format!("Here's what happens when you delete this {kind}.")),
                )
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(theme.muted_foreground)
                                .child("WHAT CHANGES"),
                        )
                        .child(first.render(cx))
                        .child(
                            div()
                                .pt_3()
                                .border_t_1()
                                .border_color(theme.border)
                                .child(second.render(cx)),
                        ),
                );
            if let Some(kept) = kept {
                body = body.child(
                    h_flex()
                        .items_start()
                        .gap_3()
                        .pt_3()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(
                            Icon::new(IconName::Check)
                                .size_4()
                                .mt_0p5()
                                .text_color(theme.success),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child("What stays saved"),
                                )
                                .child(div().text_color(theme.muted_foreground).child(kept)),
                        ),
                );
            }
            let app_ok = app.clone();
            let delete_ok = delete.clone();
            let confirm_ok = confirm.clone();
            let app_click = app.clone();
            let delete_click = delete.clone();
            dialog
                .w(px(488.))
                .p_6()
                .pb_0()
                .gap_5()
                .close_button(false)
                .title(
                    h_flex()
                        .gap_2p5()
                        .items_start()
                        .child(
                            Icon::new(IconName::Trash)
                                .size_4()
                                .mt_1()
                                .text_color(theme.danger),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_lg()
                                .child(format!("Delete {name}?")),
                        )
                        .child(
                            Button::new("delete-close")
                                .focus_ring(false)
                                .cursor_pointer()
                                .small()
                                .ghost()
                                .icon(IconName::Close)
                                .accessibility_label("Close")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    if confirm_ok.is_focused(window) {
                        let _ = app_ok.update(cx, |app, cx| delete_ok(app, window, cx));
                    }
                    true
                })
                .child(body)
                .footer(
                    h_flex()
                        .mx(-px(24.))
                        .px_6()
                        .py_3p5()
                        .border_t_1()
                        .border_color(theme.border)
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("delete-cancel")
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Cancel")
                                .ghost()
                                .track_focus(&cancel)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("delete-confirm")
                                .focus_ring(false)
                                .cursor_pointer()
                                .label(format!("Delete {kind}"))
                                .danger()
                                .track_focus(&confirm)
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = app_click
                                        .update(cx, |app, cx| delete_click(app, window, cx));
                                }),
                        ),
                )
        });
    }
}
