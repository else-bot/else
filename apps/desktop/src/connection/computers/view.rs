use super::*;
use crate::ambient::looping;
use chrono::{DateTime, Utc};
use gpui_kit::component::tooltip::Tooltip;

impl View {
    pub(super) fn render_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (view, report, error) = if self.state.pending() {
            (None, None, None)
        } else {
            (Some(self), self.state.value(), self.state.error())
        };
        let available = match report {
            Some(Availability::Available(report)) => Some(report),
            _ => None,
        };
        let mut actions = Vec::new();
        if let (Some(view), Some(report)) = (
            view,
            available.filter(|report| !report.computers.is_empty()),
        ) {
            let queued = queue_active(report);
            let count = upgrade_count(report);
            let command = if queued {
                Command::CancelRemaining
            } else {
                Command::UpgradeAll
            };
            actions.push(
                Button::new("computers-upgrade-all")
                    .control(
                        if queued {
                            Tier::Secondary
                        } else {
                            Tier::Primary
                        },
                        Scale::Compact,
                        view.pending.is_some() || (!queued && count == 0),
                        cx,
                    )
                    .icon(if queued {
                        IconName::Close
                    } else {
                        IconName::ArrowUp
                    })
                    .label(if queued {
                        "Cancel remaining".into()
                    } else {
                        format!("Upgrade all ({count})")
                    })
                    .on_click(
                        cx.listener(move |view, _, window, cx| view.command(command, window, cx)),
                    )
                    .into_any_element(),
            );
        }
        let heading = crate::panel_kit::page_heading(
            "Computers",
            "Each bot's computer, what it uses, and whether it runs the latest image. Upgrading keeps the bot's files and installed packages.",
            actions,
            cx,
        );
        let mut body = v_flex().w_full().flex_1().gap_4().text_sm().child(heading);
        if let Some(error) = error {
            body = body.child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(format!("Couldn't refresh computers. {error}")),
                    )
                    .child(
                        Button::new("computers-refresh-retry")
                            .control(Tier::Secondary, Scale::Compact, false, cx)
                            .label("Retry")
                            .on_click(cx.listener(|view, _, window, cx| view.refresh(window, cx))),
                    ),
            );
        }
        let (Some(view), Some(report)) = (view, report) else {
            return body
                .when(error.is_none(), |body| body.child("Loading computers…"))
                .into_any_element();
        };
        if let Some((command, error)) = &view.error
            && !matches!(command, Command::Upgrade(_))
        {
            let command = *command;
            body = body.child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(error.clone()),
                    )
                    .child(
                        Button::new("computers-command-retry")
                            .control(Tier::Secondary, Scale::Compact, view.pending.is_some(), cx)
                            .label("Retry")
                            .on_click(cx.listener(move |view, _, window, cx| {
                                view.command(command, window, cx)
                            })),
                    ),
            );
        }
        let Availability::Available(report) = report else {
            return body
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Computers are not available on this server."),
                )
                .into_any_element();
        };
        if report.computers.is_empty() {
            return body
                .child(settings::empty_state(
                    IconName::Monitor,
                    "No computers yet",
                    "A bot gets a computer the first time it needs one.",
                    Button::new("computers-empty-chat")
                        .control(Tier::Secondary, Scale::Standard, false, cx)
                        .label("Back to chat")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Leave))),
                    cx,
                ))
                .into_any_element();
        }
        body = body.child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(summary(report)),
        );
        if let Some(line) = queue_line(report, |id| self.computer_name(id, cx)) {
            body = body.child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .p_2p5()
                    .rounded(theme.radius)
                    .bg(theme.info.opacity(0.08))
                    .child(super::super::updates::checking_icon(
                        "computers-queue-spinner",
                        px(12.),
                        theme.info,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(line),
                    ),
            );
        }
        body.child(
            v_flex().gap_2p5().children(
                report
                    .computers
                    .iter()
                    .map(|computer| self.computer_card(computer, report, view, cx)),
            ),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Stats refresh every 3 seconds while this page is open."),
        )
        .into_any_element()
    }

    fn computer_card(
        &self,
        computer: &Computer,
        report: &Computers,
        view: &View,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme().clone();
        let id = computer.bot_id;
        let name = self.computer_name(id, cx);
        let upgrading = matches!(action(computer), Action::Running);
        let (status, status_color) = status(computer, &theme);
        let avatar = self
            .app
            .read(cx)
            .bots()
            .iter()
            .find(|bot| bot.id == id)
            .map(|bot| bot.avatar)
            .unwrap_or_default();
        let latest_ref = computer
            .latest_image
            .as_deref()
            .or(report.latest_image.as_deref());
        let latest = latest_ref.map_or_else(|| "the latest image".into(), image_label);
        let subline = h_flex()
            .flex_wrap()
            .gap_1p5()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(
                h_flex()
                    .gap_1p5()
                    .text_color(status_color)
                    .child(div().size(px(6.)).rounded_full().bg(status_color))
                    .child(status),
            )
            .child("·")
            .child(last_used(computer, Utc::now()))
            .child("·")
            .child(
                div()
                    .id(SharedString::from(format!("computer-image-{id}")))
                    .min_w_0()
                    .truncate()
                    .font_family(theme.mono_font_family.clone())
                    .child(image_label(&computer.image))
                    .tooltip(full_ref(computer.image.clone())),
            )
            .when(computer.outdated(), |row| {
                row.child(
                    pill(
                        format!("↑ {latest}"),
                        theme.warning,
                        theme.warning.opacity(0.14),
                    )
                    .flex_shrink(1.)
                    .min_w_0()
                    .truncate()
                    .id(SharedString::from(format!("computer-latest-{id}")))
                    .when_some(latest_ref, |pill, image| {
                        pill.tooltip(full_ref(image.to_owned()))
                    }),
                )
            });
        let identity = h_flex()
            .id(SharedString::from(format!("computer-bot-{id}")))
            .flex_1()
            .min_w_0()
            .gap_2p5()
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(format!("Open {name}"))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(PaneEvent::OpenBot(id))))
            .child(
                crate::avatar::bot_avatar(
                    SharedString::from(format!("computer-avatar-{id}")),
                    avatar,
                    px(28.),
                )
                .still(),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .line_height(px(20.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(name.clone()),
                    )
                    .child(subline),
            );
        let control = match action(computer) {
            Action::Upgrade | Action::Retry => {
                Button::new(SharedString::from(format!("computer-upgrade-{id}")))
                    .control(Tier::Secondary, Scale::Compact, view.pending.is_some(), cx)
                    .icon(if matches!(action(computer), Action::Retry) {
                        IconName::RefreshCw
                    } else {
                        IconName::ArrowUp
                    })
                    .label(if matches!(action(computer), Action::Retry) {
                        "Retry"
                    } else {
                        "Upgrade"
                    })
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.command(Command::Upgrade(id), window, cx)
                    }))
                    .into_any_element()
            }
            Action::Queued(position) => pill(
                format!("Queued · {}", ordinal(position)),
                theme.muted_foreground,
                theme.accent,
            )
            .into_any_element(),
            Action::Waiting(reason) => pill(
                format!("Waiting: {}", short_reason(reason)),
                theme.warning,
                theme.warning.opacity(0.14),
            )
            .into_any_element(),
            Action::Running => div()
                .flex_none()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Upgrading…")
                .into_any_element(),
            Action::Current => div()
                .flex_none()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Up to date")
                .into_any_element(),
        };
        let mut card = v_flex()
            .w_full()
            .gap_3()
            .p(px(14.))
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(if upgrading {
                theme.info.opacity(0.6)
            } else {
                theme.border
            })
            .child(
                h_flex()
                    .gap_2p5()
                    .items_center()
                    .child(identity)
                    .child(control),
            );
        if upgrading {
            card = card.child(v_flex().gap_1p5().child(progress(id, theme.info))
                .child(div().text_xs().text_color(theme.muted_foreground).child(format!("Copying {name}'s files to a new computer on {latest}. The bot can't use its computer until this ends."))));
        }
        let memory_percent = computer
            .memory_used_bytes
            .and_then(|used| ratio(used, computer.memory_limit_bytes));
        let disk = computer
            .disk_used_bytes
            .map(|used| match computer.disk_size_bytes {
                Some(size) => format!("{} / {} GB", gb(used), gb(size)),
                None => format!("{} GB", gb(used)),
            })
            .unwrap_or_else(|| "—".into());
        let memory = computer
            .memory_used_bytes
            .map(|used| format!("{} / {} GB", gb(used), gb(computer.memory_limit_bytes)))
            .unwrap_or_else(|| "—".into());
        card = card.child(
            h_flex()
                .gap_4()
                .items_stretch()
                .child(meter(
                    "Disk",
                    disk,
                    computer
                        .disk_used_bytes
                        .zip(computer.disk_size_bytes)
                        .and_then(|(used, size)| ratio(used, size)),
                    false,
                    cx,
                ))
                .child(
                    meter(
                        "CPU",
                        computer
                            .cpu_percent
                            .map(crate::usage_ui::percent)
                            .unwrap_or_else(|| "—".into()),
                        computer.cpu_percent,
                        false,
                        cx,
                    )
                    .id(SharedString::from(format!("computer-cpu-{id}")))
                    .aria_label(format!("CPU use across {} virtual CPUs", computer.cpus)),
                )
                .child(meter(
                    "Memory",
                    memory,
                    memory_percent,
                    memory_percent.is_some_and(|percent| percent > 90.),
                    cx,
                )),
        );
        match &computer.upgrade {
            Some(ComputerUpgrade::Failed { error }) => card = card.child(notice(format!("Upgrade failed: {error} The bot stayed on its previous computer."), theme.danger)),
            Some(ComputerUpgrade::Waiting { reason }) => card = card.child(notice(format!("Waiting. {reason} The queue retries {name} after the others."), theme.warning)),
            Some(ComputerUpgrade::Completed { unrestored_packages }) if !unrestored_packages.is_empty() => card = card.child(notice(format!("Upgraded. These packages couldn't be installed again: {}. Ask {name} to install them.", unrestored_packages.join(", ")), theme.warning)),
            _ => {}
        }
        if let Some((Command::Upgrade(bot), error)) = &view.error
            && *bot == id
        {
            card = card.child(notice(error.clone(), theme.danger));
        }
        card
    }
}

fn full_ref(image: String) -> impl Fn(&mut Window, &mut App) -> AnyView {
    move |window, cx| Tooltip::new(image.clone()).build(window, cx)
}

fn pill(text: String, color: Hsla, background: Hsla) -> Div {
    div()
        .flex_none()
        .px_1p5()
        .py_0p5()
        .rounded_full()
        .text_size(px(11.))
        .text_color(color)
        .bg(background)
        .child(text)
}

fn notice(text: String, color: Hsla) -> Div {
    div()
        .p_2p5()
        .rounded_md()
        .text_xs()
        .text_color(color)
        .bg(color.opacity(0.09))
        .child(text)
}

fn short_reason(reason: &str) -> String {
    let text = reason.trim().trim_end_matches('.');
    let text = text.strip_prefix("The bot is ").unwrap_or(text);
    if text.chars().count() > 30 {
        format!("{}…", text.chars().take(29).collect::<String>())
    } else {
        text.into()
    }
}

fn ratio(used: u64, size: u64) -> Option<f64> {
    (size > 0).then(|| used as f64 / size as f64 * 100.)
}

fn meter(label: &'static str, value: String, percent: Option<f64>, warning: bool, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_1p5()
        .child(
            h_flex()
                .justify_between()
                .gap_1()
                .text_size(px(11.5))
                .child(div().text_color(theme.muted_foreground).child(label))
                .child(
                    div()
                        .text_color(if percent.is_none() {
                            theme.muted_foreground
                        } else {
                            theme.foreground
                        })
                        .child(value),
                ),
        )
        .child(
            crate::usage_ui::bar(
                [(
                    percent.unwrap_or(0.),
                    if warning {
                        theme.warning
                    } else {
                        theme.foreground.opacity(0.7)
                    },
                )],
                cx,
            )
            .h(px(4.)),
        )
}

fn progress(id: Uuid, color: Hsla) -> Div {
    div()
        .relative()
        .w_full()
        .h(px(3.))
        .rounded_sm()
        .overflow_hidden()
        .bg(color.opacity(0.18))
        .child(looping(move |moving| {
            let bar = div()
                .absolute()
                .h_full()
                .w(relative(0.35))
                .rounded_sm()
                .bg(color);
            if !moving {
                return bar.left(relative(0.325)).into_any_element();
            }
            bar.with_animation(
                SharedString::from(format!("computer-upgrade-progress-{id}")),
                Animation::new(Duration::from_millis(1600)).repeat(),
                |bar, delta| bar.left(relative(delta * 1.35 - 0.35)),
            )
            .into_any_element()
        }))
}

fn status(computer: &Computer, theme: &gpui_kit::component::Theme) -> (&'static str, Hsla) {
    match &computer.upgrade {
        Some(ComputerUpgrade::Running) => return ("Upgrading", theme.info),
        Some(ComputerUpgrade::Failed { .. }) => return ("Upgrade failed", theme.danger),
        _ => {}
    }
    match computer.state {
        ComputerStatus::Running => ("Running", theme.success),
        ComputerStatus::Stopped => ("Stopped", theme.muted_foreground),
        ComputerStatus::Starting | ComputerStatus::Creating | ComputerStatus::Resuming => {
            ("Starting", theme.info)
        }
        ComputerStatus::Stopping => ("Stopping", theme.info),
        ComputerStatus::Pausing | ComputerStatus::Paused => ("Paused", theme.muted_foreground),
        ComputerStatus::Missing | ComputerStatus::Error => ("Error", theme.danger),
    }
}

fn last_used(computer: &Computer, now: DateTime<Utc>) -> String {
    let Some(at) = computer.last_used_at else {
        return "Not used yet".into();
    };
    let seconds = (now - at).num_seconds().max(0);
    if seconds < 60
        && computer.state == ComputerStatus::Running
        && computer.desired_state == ComputerDesiredState::Auto
    {
        return "In use now".into();
    }
    if seconds < 60 {
        "Used just now".into()
    } else if seconds < 3600 {
        format!("Used {} min ago", seconds / 60)
    } else if seconds < 86400 {
        format!("Used {} h ago", seconds / 3600)
    } else {
        let days = seconds / 86400;
        format!("Used {days} {} ago", if days == 1 { "day" } else { "days" })
    }
}
