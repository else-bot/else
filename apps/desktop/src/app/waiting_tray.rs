use super::ElseApp;
use super::task_reply::state::format_elapsed;
use crate::avatar::bot_avatar;
use crate::model::{Delegation, DelegationStatus};
use chrono::Utc;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// About three rows; more scroll inside the tray so the composer stays put.
const MAX_ROWS_HEIGHT: f32 = 240.;

/// The first non-empty line of a task's latest message, as plain text.
fn first_line(text: &str) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    let plain = markdown::to_mdast(line, &markdown::ParseOptions::default())
        .ok()?
        .to_string();
    let plain = plain.trim();
    (!plain.is_empty()).then(|| plain.to_owned())
}

fn one_line(task: &str) -> String {
    task.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl ElseApp {
    pub(super) fn toggle_waiting_tray(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.view.as_mut() {
            view.tray_expanded = !view.tray_expanded;
        }
        cx.notify();
    }

    /// The tasks this bot waits on, docked above the composer.
    pub(super) fn render_waiting_tray(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let view = self.view.as_ref()?;
        let tasks = view.conversation.as_ref()?.waiting_on();
        if tasks.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let bot = |id| self.bots.iter().find(|bot| bot.id == id);
        let sender = bot(view.bot_id);
        let sender_name = sender.map_or_else(|| "This bot".to_owned(), |b| b.name.clone());
        let name =
            |d: &Delegation| bot(d.target_bot_id).map_or(d.target_name.clone(), |b| b.name.clone());
        let collapsed = !view.tray_expanded;
        let now = Utc::now();
        let count = tasks.len();
        let summary = format!(
            "{sender_name} is waiting on {count} {}",
            if count == 1 { "reply" } else { "replies" }
        );

        let header = h_flex()
            .w_full()
            .min_w_0()
            .items_center()
            .gap_2()
            .px_2()
            .pb_1p5()
            .text_xs()
            .text_color(theme.muted_foreground)
            .when(!collapsed, |row| {
                row.border_b_1().border_color(theme.border.opacity(0.6))
            })
            .child(
                h_flex()
                    .flex_none()
                    .children(tasks.iter().take(3).enumerate().filter_map(|(ix, d)| {
                        bot(d.target_bot_id).map(|b| {
                            div().when(ix > 0, |d| d.ml(px(-5.))).child(
                                bot_avatar(
                                    SharedString::from(format!("tray-stack-{}", d.id)),
                                    b.avatar,
                                    px(16.),
                                )
                                .still(),
                            )
                        })
                    })),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(summary),
            )
            .when(!collapsed, |row| {
                row.child(div().flex_none().child("· continues as each one lands"))
            })
            .child(div().flex_1())
            .child(
                Button::new("waiting-tray-toggle")
                    .ghost()
                    .xsmall()
                    .focus_ring(false)
                    .cursor_pointer()
                    .label(if collapsed { "Show" } else { "Hide" })
                    .accessibility_label(if collapsed {
                        "Show the tasks this bot is waiting on"
                    } else {
                        "Hide the tasks this bot is waiting on"
                    })
                    .on_click(cx.listener(|app, _, _, cx| app.toggle_waiting_tray(cx))),
            );

        let rows = (!collapsed).then(|| {
            tasks.iter().enumerate().map(|(ix, &d)| {
                let target = d.target_bot_id;
                let (badge, color) = match d.status {
                    DelegationStatus::Queued => ("Queued", theme.muted_foreground),
                    DelegationStatus::Running => ("Working", theme.primary),
                    DelegationStatus::Completed => ("Replied", theme.success),
                    DelegationStatus::Failed => ("Failed", theme.danger),
                    DelegationStatus::Cancelled => ("Stopped", theme.muted_foreground),
                };
                let end = d.finished_at.filter(|_| d.status.settled()).unwrap_or(now);
                let elapsed = format_elapsed((end - d.created_at).num_seconds().max(0));
                let status = match d.status {
                    DelegationStatus::Queued => "Waiting to start".to_owned(),
                    DelegationStatus::Running => {
                        let line = d
                            .progress
                            .as_deref()
                            .and_then(first_line)
                            .unwrap_or_else(|| "Working on it".to_owned());
                        match d.updates {
                            0 | 1 => line,
                            n => format!("{line} · {n} updates"),
                        }
                    }
                    _ => format!("{sender_name} reads this next"),
                };
                let avatar = bot(target).map(|b| {
                    let avatar = bot_avatar(
                        SharedString::from(format!("tray-{}", d.id)),
                        b.avatar,
                        px(28.),
                    );
                    if d.status == DelegationStatus::Running {
                        avatar.working()
                    } else {
                        avatar.still()
                    }
                });
                let dot = div().size(px(7.)).flex_none().rounded_full();
                let dot = if d.status == DelegationStatus::Queued {
                    dot.border_1().border_color(color.opacity(0.8))
                } else {
                    dot.bg(color)
                };
                h_flex()
                    .id(SharedString::from(format!("tray-row-{}", d.id)))
                    .w_full()
                    .min_w_0()
                    .items_start()
                    .gap_2p5()
                    .px_2()
                    .py_2()
                    .rounded(theme.radius)
                    .when(ix > 0, |row| {
                        row.border_t_1().border_color(theme.border.opacity(0.4))
                    })
                    .child(div().flex_none().size(px(28.)).children(avatar))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .min_w_0()
                                    .items_center()
                                    .gap_2()
                                    .text_sm()
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(name(d)),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .px_1p5()
                                            .rounded_full()
                                            .border_1()
                                            .border_color(color.opacity(0.5))
                                            .text_xs()
                                            .text_color(color)
                                            .child(badge),
                                    ),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .truncate()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(one_line(&d.task)),
                            )
                            .child(
                                h_flex()
                                    .w_full()
                                    .min_w_0()
                                    .items_center()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(theme.muted_foreground.opacity(0.8))
                                    .child(dot)
                                    .child(div().min_w_0().truncate().child(status)),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_none()
                            .items_end()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(elapsed),
                            )
                            .children(bot(target).map(|b| {
                                Button::new(SharedString::from(format!("tray-open-{}", d.id)))
                                    .ghost()
                                    .xsmall()
                                    .focus_ring(false)
                                    .cursor_pointer()
                                    .label("Open chat")
                                    .accessibility_label(format!("Open chat with {}", b.name))
                                    .on_click(cx.listener(move |app, _, window, cx| {
                                        app.open_bot(target, window, cx)
                                    }))
                            })),
                    )
            })
        });

        Some(
            v_flex()
                .id("waiting-tray")
                .w_full()
                .pt_1p5()
                .px_1()
                .pb_1()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border.opacity(0.6))
                .bg(theme.sidebar.opacity(0.6))
                .child(header)
                .when_some(rows, |tray, rows| {
                    tray.child(
                        v_flex()
                            .id("waiting-tray-rows")
                            .w_full()
                            .max_h(px(MAX_ROWS_HEIGHT))
                            .overflow_y_scroll()
                            .children(rows),
                    )
                })
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::first_line;

    #[test]
    fn progress_shows_the_first_line_as_plain_text() {
        assert_eq!(
            first_line("\n**Opened** Search Console\nmore"),
            Some("Opened Search Console".to_owned())
        );
        assert_eq!(first_line("  \n"), None);
    }
}
