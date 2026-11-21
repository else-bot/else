use super::ElseApp;
use super::task_reply::state::format_elapsed;
use crate::avatar::bot_avatar;
use crate::controls::{Control as _, Scale, Tier};
use crate::model::Wait;
use chrono::{DateTime, Utc};
use else_wire::delegations::{Delegation, DelegationStatus};
use else_wire::waits::{ComputerJobWait, JobStatus, McpWait};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

/// About three rows; more scroll inside the tray so the composer stays put.
const MAX_ROWS_HEIGHT: f32 = 240.;
/// Characters of a job's last result shown in its details.
const SHOWN_RESULT: usize = 600;

/// The first non-empty line of a task's latest message, as plain text.
fn first_line(text: &str) -> Option<String> {
    else_plain_text::lines(text).into_iter().next()
}

fn one_line(task: &str) -> String {
    task.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// "2 jobs and 1 reply": what the tray's header says the bot waits on.
fn waiting_summary(waits: &[Wait<'_>]) -> String {
    let replies = waits.iter().filter(|w| matches!(w, Wait::Task(_))).count();
    let jobs = waits.len() - replies;
    let count =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    match (jobs, replies) {
        (0, replies) => count(replies, "reply", "replies"),
        (jobs, 0) => count(jobs, "job", "jobs"),
        (jobs, replies) => format!(
            "{} and {}",
            count(jobs, "job", "jobs"),
            count(replies, "reply", "replies")
        ),
    }
}

/// The row's label: what the bot said it waits for.
fn wait_title(wait: &Wait<'_>) -> String {
    let purpose = match wait {
        Wait::Mcp(wait) if !wait.purpose.trim().is_empty() => one_line(&wait.purpose),
        Wait::Mcp(wait) => format!("{} job", wait.mcp_id),
        Wait::Job(job) => job
            .purpose
            .as_deref()
            .map(one_line)
            .filter(|purpose| !purpose.is_empty())
            .unwrap_or_else(|| "Computer job".to_owned()),
        Wait::Task(task) => one_line(&task.task),
    };
    format!("Waiting: {purpose}")
}

/// Where a job stands: its checks and last readable status.
fn job_status(wait: &Wait<'_>) -> Option<String> {
    match wait {
        Wait::Mcp(wait) => {
            let checks = match wait.checks {
                0 => None,
                1 => Some("1 check".to_owned()),
                n => Some(format!("{n} checks")),
            };
            let parts: Vec<String> = checks.into_iter().chain(wait.last_status.clone()).collect();
            (!parts.is_empty()).then(|| parts.join(" · "))
        }
        Wait::Job(job) => Some(
            job.job_status
                .as_ref()
                .unwrap_or(&JobStatus::Running)
                .as_str()
                .to_owned(),
        ),
        Wait::Task(_) => None,
    }
}

/// The first web link in `text`, such as a PR or thread URL in a status.
fn first_link(text: &str) -> Option<String> {
    let start = text.find("https://").or_else(|| text.find("http://"))?;
    let link: String = text[start..]
        .chars()
        .take_while(|c| {
            !c.is_whitespace()
                && !matches!(c, '"' | '\'' | '\\' | '<' | '>' | ')' | ']' | '}' | ',')
        })
        .collect();
    let link = link.trim_end_matches(['.', ';', ':']);
    (link.len() > "https://".len()).then(|| link.to_owned())
}

fn elapsed_since(at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    at.map(|at| format_elapsed((now - at).num_seconds().max(0)))
}

impl ElseApp {
    pub(super) fn toggle_waiting_tray(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.view.as_mut() {
            view.tray_expanded.toggle();
        }
        cx.notify();
    }

    fn toggle_wait(&mut self, wait: Uuid, cx: &mut Context<Self>) {
        if let Some(view) = self.view.as_mut()
            && !view.expanded_waits.remove(&wait)
        {
            view.expanded_waits.insert(wait);
        }
        cx.notify();
    }

    /// Stop one wait. The turn update that settles it arrives on the stream.
    fn stop_wait(&mut self, bot: Uuid, wait: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut().filter(|v| v.bot_id == bot) else {
            return;
        };
        if !view.stopping_waits.insert(wait) {
            return;
        }
        cx.notify();
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.cancel_wait(bot, wait).await;
            let _ = this.update_in(cx, |app, window, cx| {
                if let Some(view) = app.view_of(bot) {
                    view.stopping_waits.remove(&wait);
                }
                match result {
                    Ok(()) => {}
                    Err(err) if app.unauthorized(&err, window, cx) => {}
                    // It ended first; the stream shows how.
                    Err(err) if err.code() == Some("wait_ended") => {}
                    Err(err) => app.notice = Some(err.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// What this bot waits on, docked above the composer: one row per job
    /// or task, each stoppable on its own.
    pub(super) fn render_waiting_tray(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let view = self.view.as_ref()?;
        let waits = view.conversation.as_ref()?.waiting_on();
        if waits.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let sender = self.bots.iter().find(|bot| bot.id == view.bot_id);
        let sender_name = sender.map_or_else(|| "This bot".to_owned(), |b| b.name.clone());
        let collapsed = !view.tray_expanded;
        let summary = format!("{sender_name} is waiting on {}", waiting_summary(&waits));
        let tasks = waits.iter().filter_map(|wait| match wait {
            Wait::Task(task) => Some(task.target_bot_id),
            _ => None,
        });

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
            .children(self.face_stack("tray-stack", tasks, px(16.)))
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
                    .control(Tier::Ghost, Scale::Compact, false, cx)
                    .label(if collapsed { "Show" } else { "Hide" })
                    .accessibility_label(if collapsed {
                        "Show what this bot is waiting on"
                    } else {
                        "Hide what this bot is waiting on"
                    })
                    .on_click(cx.listener(|app, _, _, cx| app.toggle_waiting_tray(cx))),
            );

        let rows = (!collapsed).then(|| {
            waits
                .iter()
                .enumerate()
                .map(|(ix, wait)| {
                    let row = match wait {
                        Wait::Task(task) => self.render_task_row(task, &sender_name, cx),
                        _ => self.render_job_row(wait, cx),
                    };
                    div()
                        .w_full()
                        .when(ix > 0, |row| {
                            row.border_t_1().border_color(theme.border.opacity(0.4))
                        })
                        .child(row)
                })
                .collect::<Vec<_>>()
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

    fn stop_button(&self, wait: &Wait<'_>, label: &str, cx: &mut Context<Self>) -> Option<Button> {
        let view = self.view.as_ref()?;
        if !wait.stoppable() {
            return None;
        }
        let (bot, id) = (view.bot_id, wait.id());
        let stopping = view.stopping_waits.contains(&id);
        Some(
            Button::new(SharedString::from(format!("tray-stop-{id}")))
                .control(Tier::Ghost, Scale::Compact, stopping, cx)
                .label(if stopping { "Stopping…" } else { "Stop" })
                .accessibility_label(format!("Stop {label}"))
                .on_click(cx.listener(move |app, _, window, cx| {
                    cx.stop_propagation();
                    app.stop_wait(bot, id, window, cx);
                })),
        )
    }

    fn render_job_row(&self, wait: &Wait<'_>, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let id = wait.id();
        let expanded = self
            .view
            .as_ref()
            .is_some_and(|view| view.expanded_waits.contains(&id));
        let title = wait_title(wait);
        let source = match wait {
            Wait::Mcp(wait) if wait.tool.is_empty() => wait.mcp_id.clone(),
            Wait::Mcp(wait) => format!("{} · {}", wait.mcp_id, wait.tool),
            _ => "Computer job".to_owned(),
        };
        let elapsed = elapsed_since(wait.created_at(), Utc::now());
        let status = job_status(wait);
        let details = expanded.then(|| match wait {
            Wait::Mcp(wait) => mcp_details(wait),
            Wait::Job(job) => job_details(job),
            Wait::Task(_) => Vec::new(),
        });
        let link = match wait {
            Wait::Mcp(wait) => wait
                .last_result
                .as_deref()
                .and_then(first_link)
                .or_else(|| first_link(&wait.purpose)),
            Wait::Job(job) => job
                .purpose
                .as_deref()
                .and_then(first_link)
                .or_else(|| first_link(&job.command)),
            Wait::Task(_) => None,
        };
        v_flex()
            .id(SharedString::from(format!("tray-row-{id}")))
            .w_full()
            .min_w_0()
            .px_2()
            .py_2()
            .gap_1p5()
            .rounded(theme.radius)
            .cursor_pointer()
            .hover(|row| row.bg(theme.foreground.opacity(0.03)))
            .on_click(cx.listener(move |app, _, _, cx| app.toggle_wait(id, cx)))
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .items_start()
                    .gap_2p5()
                    .child(
                        div()
                            .flex_none()
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::new(IconName::Hourglass)
                                    .small()
                                    .text_color(theme.muted_foreground),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                div()
                                    .w_full()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title.clone()),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .truncate()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(source),
                            )
                            .child(
                                h_flex()
                                    .w_full()
                                    .min_w_0()
                                    .items_center()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(theme.muted_foreground.opacity(0.8))
                                    .child(
                                        div()
                                            .size(px(7.))
                                            .flex_none()
                                            .rounded_full()
                                            .bg(theme.primary),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .child(status.unwrap_or_else(|| "Running".to_owned())),
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_none()
                            .items_end()
                            .gap_1()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_1()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .children(elapsed)
                                    .child(
                                        Icon::new(if expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        })
                                        .xsmall(),
                                    ),
                            )
                            .children(self.stop_button(wait, &title, cx)),
                    ),
            )
            .when_some(details, |row, details| {
                row.child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .ml(px(38.))
                        .gap_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .children(details.into_iter().map(|(label, value, mono)| {
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .items_start()
                                .gap_2()
                                .child(div().flex_none().w(px(76.)).child(label))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .whitespace_normal()
                                        .text_color(theme.foreground)
                                        .when(mono, |value| {
                                            value.font_family(theme.mono_font_family.clone())
                                        })
                                        .child(value),
                                )
                        }))
                        .children(link.map(|url| {
                            Button::new(SharedString::from(format!("tray-link-{id}")))
                                .control(Tier::Ghost, Scale::Compact, false, cx)
                                .self_start()
                                .icon(IconName::ExternalLink)
                                .label("Open link")
                                .accessibility_label(format!("Open {url}"))
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    cx.open_url(&url);
                                })
                        })),
                )
            })
            .into_any_element()
    }

    fn render_task_row(
        &self,
        d: &Delegation,
        sender_name: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let bot = |id| self.bots.iter().find(|bot| bot.id == id);
        let target = d.target_bot_id;
        let name = bot(target).map_or(d.target_name.clone(), |b| b.name.clone());
        let now = Utc::now();
        let (badge, color) = match d.status {
            DelegationStatus::Queued => ("Queued", theme.muted_foreground),
            DelegationStatus::Running => ("Working", theme.primary),
            DelegationStatus::Completed => ("Replied", theme.success),
            DelegationStatus::Failed => ("Failed", theme.danger),
            DelegationStatus::Cancelled => ("Stopped", theme.muted_foreground),
            DelegationStatus::Unknown => ("Status unavailable", theme.muted_foreground),
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
        let stop = self.stop_button(&Wait::Task(d), &format!("the task for {name}"), cx);
        h_flex()
            .id(SharedString::from(format!("tray-row-{}", d.id)))
            .w_full()
            .min_w_0()
            .items_start()
            .gap_2p5()
            .px_2()
            .py_2()
            .rounded(theme.radius)
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
                                    .child(name.clone()),
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
                    .child(
                        h_flex()
                            .gap_1()
                            .children(bot(target).map(|b| {
                                Button::new(SharedString::from(format!("tray-open-{}", d.id)))
                                    .control(Tier::Ghost, Scale::Compact, false, cx)
                                    .label("Open chat")
                                    .accessibility_label(format!("Open chat with {}", b.name))
                                    .on_click(cx.listener(move |app, _, window, cx| {
                                        app.open_bot(target, window, cx)
                                    }))
                            }))
                            .children(stop),
                    ),
            )
            .into_any_element()
    }
}

/// Label, value, and whether the value is code.
type Detail = (&'static str, String, bool);

fn mcp_details(wait: &McpWait) -> Vec<Detail> {
    let mut details = vec![("Purpose", wait.purpose.clone(), false)];
    if !wait.tool.is_empty() {
        let every = if wait.interval_seconds > 0 {
            format!(" every {}", format_elapsed(wait.interval_seconds))
        } else {
            String::new()
        };
        details.push((
            "Checks",
            format!("{} on {}{every}", wait.tool, wait.mcp_id),
            false,
        ));
    }
    if let Some(deadline) = wait.deadline_at.as_deref().and_then(|at| at.parse().ok()) {
        details.push(("Gives up", super::message_row::short_stamp(deadline), false));
    }
    if let Some(result) = wait.last_result.as_deref() {
        let shown: String = result.chars().take(SHOWN_RESULT).collect();
        let cut = if shown.len() < result.len() {
            "…"
        } else {
            ""
        };
        details.push(("Last result", format!("{shown}{cut}"), true));
    }
    details
}

fn job_details(job: &ComputerJobWait) -> Vec<Detail> {
    job.purpose
        .iter()
        .map(|purpose| ("Purpose", purpose.clone(), false))
        .chain([
            ("Command", job.command.clone(), true),
            ("Job", job.job_id.to_string(), true),
        ])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{first_line, first_link, job_status, wait_title, waiting_summary};
    use crate::model::Wait;
    use else_wire::waits::{ComputerJobWait, McpWait};
    use uuid::Uuid;

    fn mcp(purpose: &str, checks: i64, status: Option<&str>) -> McpWait {
        McpWait {
            id: Uuid::from_u128(1),
            mcp_id: "t3-code".into(),
            tool: "task_status".into(),
            purpose: purpose.into(),
            interval_seconds: 30,
            deadline_at: None,
            checks,
            last_status: status.map(str::to_owned),
            last_result: None,
            created_at: None,
        }
    }

    #[test]
    fn progress_shows_the_first_line_as_plain_text() {
        assert_eq!(
            first_line("\n**Opened** Search Console\nmore"),
            Some("Opened Search Console".to_owned())
        );
        assert_eq!(first_line("  \n"), None);
        assert_eq!(
            first_line("~~Drafting~~ Sent https://example.com/report"),
            Some("Drafting Sent https://example.com/report".to_owned())
        );
    }

    #[test]
    fn a_wait_reads_as_its_purpose_and_status() {
        let wait = mcp("T3 thread for #465\n(else_status)", 2, Some("in_progress"));
        assert_eq!(
            wait_title(&Wait::Mcp(&wait)),
            "Waiting: T3 thread for #465 (else_status)"
        );
        assert_eq!(
            job_status(&Wait::Mcp(&wait)).as_deref(),
            Some("2 checks · in_progress")
        );
        let job = ComputerJobWait {
            id: Uuid::from_u128(2),
            job_id: Uuid::from_u128(3),
            command: "for i in $(seq 1 50); do curl -s https://api.github.com/x; done".into(),
            purpose: Some("CI checks on PR #482".into()),
            job_status: None,
            created_at: None,
        };
        assert_eq!(
            wait_title(&Wait::Job(&job)),
            "Waiting: CI checks on PR #482"
        );
        assert_eq!(job_status(&Wait::Job(&job)).as_deref(), Some("running"));
        let unlabeled = ComputerJobWait {
            purpose: None,
            ..job
        };
        assert_eq!(
            wait_title(&Wait::Job(&unlabeled)),
            "Waiting: Computer job",
            "never the raw command"
        );
    }

    #[test]
    fn the_header_counts_jobs_and_replies() {
        let one = mcp("a", 1, None);
        let two = mcp("b", 1, None);
        assert_eq!(waiting_summary(&[Wait::Mcp(&one)]), "1 job");
        assert_eq!(
            waiting_summary(&[Wait::Mcp(&one), Wait::Mcp(&two)]),
            "2 jobs"
        );
    }

    #[test]
    fn a_link_in_a_status_is_found_without_trailing_punctuation() {
        assert_eq!(
            first_link(r#"{"pr":"https://github.com/else-bot/else/pull/464","state":"open"}"#)
                .as_deref(),
            Some("https://github.com/else-bot/else/pull/464")
        );
        assert_eq!(
            first_link("See https://t3.example/t/42.").as_deref(),
            Some("https://t3.example/t/42")
        );
        assert_eq!(
            first_link(r#"{"text":"{\"link\":\"https://t3.example/t/465\"}"}"#).as_deref(),
            Some("https://t3.example/t/465"),
            "a link inside escaped JSON"
        );
        assert_eq!(first_link("no link here"), None);
    }
}
