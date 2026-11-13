use super::*;
use crate::connection::settings::settings_column;
use crate::controls::{Control as _, Scale, Tier, toggle};
use chrono::{DateTime, Local, Utc};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Icon, Theme, h_flex};

fn interval(seconds: u64) -> String {
    match seconds {
        0 => "Manual only".into(),
        s if s % 86_400 == 0 => format!(
            "Every {} day{}",
            s / 86_400,
            if s == 86_400 { "" } else { "s" }
        ),
        s if s % 3600 == 0 => format!("Every {} hours", s / 3600),
        s if s % 60 == 0 => format!("Every {} minutes", s / 60),
        s => format!("Every {s} seconds"),
    }
}

fn timestamp(value: &str) -> String {
    DateTime::parse_from_rfc3339(value)
        .map(|at| at.with_timezone(&Local).format("%b %-d, %H:%M").to_string())
        .unwrap_or_else(|_| value.to_owned())
}

fn next_backup(snapshot: &BackupSnapshot) -> String {
    if snapshot.running {
        return "In progress".into();
    }
    if !snapshot.enabled {
        return "Paused".into();
    }
    if snapshot.interval_secs == 0 {
        return "Manual".into();
    }
    match snapshot
        .next_backup_at
        .as_deref()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
    {
        Some(at) => {
            let remaining = at.signed_duration_since(Utc::now()).num_seconds();
            if remaining <= 0 {
                "Due now".into()
            } else if remaining < 3600 {
                format!("In {} min", (remaining + 59) / 60)
            } else {
                format!("In {}h", (remaining + 3599) / 3600)
            }
        }
        None => "Not scheduled".into(),
    }
}

pub(super) fn metric(label: &'static str, value: String, detail: String, theme: &Theme) -> Div {
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_1()
        .px_4()
        .py_3()
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(
            div()
                .text_base()
                .font_weight(FontWeight::SEMIBOLD)
                .child(value),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(detail),
        )
}

/// Backup figures in one bordered row, separated by hairlines, so they read
/// as one summary instead of three floating tiles.
pub(super) fn metrics(cells: [Div; 3], stacked: bool, theme: &Theme) -> Div {
    let mut row = h_flex()
        .w_full()
        .items_stretch()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(theme.border)
        .when(stacked, |row| row.flex_col());
    for (index, cell) in cells.into_iter().enumerate() {
        row = row.child(cell.when(index > 0, |cell| {
            if stacked {
                cell.border_t_1().border_color(theme.border)
            } else {
                cell.border_l_1().border_color(theme.border)
            }
        }));
    }
    row
}

pub(super) fn message(text: String, color: Hsla, theme: &Theme) -> Div {
    div()
        .p_3()
        .rounded(theme.radius)
        .bg(theme.secondary.opacity(0.45))
        .text_xs()
        .text_color(color)
        .child(text)
}

impl Shell {
    pub(in crate::connection) fn render_backups(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(view) = &self.backups else {
            return div()
                .p_8()
                .child("Loading backup settings…")
                .into_any_element();
        };
        let Some(snapshot) = &view.snapshot else {
            return v_flex()
                .p_8()
                .gap_5()
                .child(super::super::settings::section_heading(
                    "Backups",
                    "Copies of every bot’s computer, projects and memory.",
                    cx,
                ))
                .child(
                    div().text_sm().text_color(theme.muted_foreground).child(
                        view.error
                            .clone()
                            .or_else(|| view.poll_error.clone())
                            .unwrap_or_else(|| "Loading backup settings…".into()),
                    ),
                )
                .when(
                    view.error.is_some() || view.poll_error.is_some(),
                    |content| {
                        content.child(
                            Button::new("backups-retry")
                                .control(Tier::Secondary, Scale::Standard, false, cx)
                                .label("Try again")
                                .on_click(cx.listener(|shell, _, window, cx| {
                                    shell.open_backups(window, cx)
                                })),
                        )
                    },
                )
                .into_any_element();
        };
        let busy = view.busy() || view.local_dirty(cx);
        let unavailable = snapshot.unavailable_reason.is_some();
        let stacked = window.viewport_size().width < px(850.);
        let has_destination = snapshot.destination.is_some();
        let status = if snapshot.running {
            "Backing up workspace…"
        } else if snapshot.last_error.is_some() {
            "Backup needs attention"
        } else if !has_destination {
            "No remote destination"
        } else if !snapshot.enabled {
            "Automatic backups paused"
        } else if snapshot.interval_secs == 0 {
            "Manual backups enabled"
        } else {
            "Remote backups enabled"
        };
        let tone = if snapshot.last_error.is_some() || unavailable {
            theme.warning
        } else if snapshot.enabled && has_destination {
            theme.success
        } else {
            theme.muted_foreground
        };
        let latest = snapshot
            .recent
            .iter()
            .find(|run| run.remote_status.as_deref() == Some("completed"));
        // Groups sit 32 px apart; rows inside a group 20 px.
        let page = v_flex()
            .gap_8()
            .child(super::super::settings::section_heading(
                "Backups",
                "Copies of every bot’s computer, projects and memory.",
                cx,
            ))
            .when_some(snapshot.local.as_ref(), |content, local| {
                content.child(self.render_local_backups(view, snapshot, local, stacked, cx))
            });
        let run_now = Button::new("backup-run-now")
            .control(
                Tier::Primary,
                Scale::Standard,
                busy || snapshot.running || !has_destination || unavailable,
                cx,
            )
            .label(if snapshot.running {
                "Backing up…"
            } else {
                "Back up now"
            })
            .loading(view.operation == Operation::Starting)
            .on_click(cx.listener(|shell, _, window, cx| shell.run_remote_backup(window, cx)));
        let mut content = v_flex()
            .gap_5()
            .child(super::super::settings::section_heading(
                "Remote",
                "Your workspace, backed up off this server.",
                cx,
            ))
            // The remote summary: its state, the run action beside it, and
            // the automatic switch.
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(div().text_sm().text_color(tone).child(status))
                            .when(snapshot.running, |summary| {
                                summary.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("You can leave settings while this backup runs."),
                                )
                            }),
                    )
                    .when(view.editor.is_none(), |row| row.child(run_now))
                    .child(
                        toggle(
                            "remote-backups-enabled",
                            snapshot.enabled,
                            busy || unavailable || !has_destination || view.editor.is_some(),
                            cx,
                        )
                        .accessibility_label("Automatic remote backups")
                        .tooltip(if snapshot.enabled {
                            "Pause automatic remote backups"
                        } else {
                            "Enable automatic remote backups"
                        })
                        .on_click(cx.listener(
                            |shell, enabled: &bool, window, cx| {
                                shell.toggle_remote_backups(*enabled, window, cx)
                            },
                        )),
                    ),
            )
            .when_some(snapshot.unavailable_reason.clone(), |content, reason| {
                content.child(message(reason, theme.warning, &theme))
            })
            .when_some(snapshot.last_error.clone(), |content, reason| {
                content.child(message(reason, theme.warning, &theme))
            })
            .when(view.editor.is_none(), |content| {
                content.child(metrics(
                    [
                        metric(
                            "Next backup",
                            next_backup(snapshot),
                            interval(snapshot.interval_secs),
                            &theme,
                        ),
                        metric(
                            "Uploaded last run",
                            latest
                                .and_then(|run| run.uploaded_bytes.or(run.bytes))
                                .map(byte_size)
                                .unwrap_or_else(|| "None yet".into()),
                            latest
                                .and_then(|run| run.finished_at.as_deref())
                                .map(timestamp)
                                .unwrap_or_else(|| "After your first remote backup".into()),
                            &theme,
                        ),
                        metric(
                            "Remote copies",
                            format!("{} copies", snapshot.remote_count),
                            format!(
                                "Keep target {} · {} in the bucket",
                                snapshot.keep,
                                byte_size(snapshot.remote_bytes)
                            ),
                            &theme,
                        ),
                    ],
                    stacked,
                    &theme,
                ))
            });
        if let Some(destination) = &snapshot.destination {
            content = content.when(
                !matches!(view.editor, Some(Editor::Destination(_))),
                |content| {
                    content.child(crate::panel_kit::grouped_list(
                        [crate::panel_kit::list_row()
                            .child(crate::panel_kit::row_icon(
                                Icon::new(IconName::CloudUpload),
                                cx,
                            ))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(destination.service.label()),
                                    )
                                    .child(
                                        div().text_xs().text_color(theme.muted_foreground).child(
                                            format!(
                                                "{}/{}",
                                                destination.bucket, destination.prefix
                                            ),
                                        ),
                                    ),
                            )
                            .child(
                                Button::new("backup-edit-destination")
                                    .control(
                                        Tier::Secondary,
                                        Scale::Standard,
                                        busy || view.editor.is_some(),
                                        cx,
                                    )
                                    .label("Edit destination")
                                    .icon(IconName::Pencil)
                                    .on_click(cx.listener(|shell, _, window, cx| {
                                        shell.edit_backup_destination(window, cx)
                                    })),
                            )
                            .into_any_element()],
                        cx,
                    ))
                },
            );
        } else {
            content =
                content.child(
                    v_flex()
                        .gap_3()
                        .p_4()
                        .rounded(theme.radius)
                        .bg(theme.secondary.opacity(0.45))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child("Connect a backup destination"),
                        )
                        .child(div().text_xs().text_color(theme.muted_foreground).child(
                            "Use Cloudflare R2, Amazon S3, or another S3-compatible bucket.",
                        ))
                        .child(
                            Button::new("backup-configure-destination")
                                .control(
                                    Tier::Primary,
                                    Scale::Standard,
                                    busy || view.editor.is_some(),
                                    cx,
                                )
                                .label("Configure destination")
                                .on_click(cx.listener(|shell, _, window, cx| {
                                    shell.edit_backup_destination(window, cx)
                                })),
                        ),
                );
        }
        if let Some(Editor::Destination(form)) = &view.editor {
            content = content.child(
                form.render(busy, stacked, cx)
                    .p_4()
                    .rounded(theme.radius)
                    .bg(theme.secondary.opacity(0.3)),
            );
        }
        content = content.when(
            !matches!(view.editor, Some(Editor::Destination(_))),
            |content| {
                content.child(crate::panel_kit::grouped_list(
                    [crate::panel_kit::list_row()
                        .child(crate::panel_kit::row_icon(Icon::new(IconName::Clock), cx))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(interval(snapshot.interval_secs)),
                                )
                                .child(div().text_xs().text_color(theme.muted_foreground).child(
                                    format!(
                                        "Keep the newest {} complete remote backups",
                                        snapshot.keep
                                    ),
                                )),
                        )
                        .child(
                            Button::new("backup-edit-policy")
                                .control(
                                    Tier::Secondary,
                                    Scale::Standard,
                                    busy || view.editor.is_some(),
                                    cx,
                                )
                                .label("Edit policy")
                                .icon(IconName::Pencil)
                                .on_click(cx.listener(|shell, _, window, cx| {
                                    shell.edit_backup_policy(window, cx)
                                })),
                        )
                        .into_any_element()],
                    cx,
                ))
            },
        );
        if let Some(Editor::Policy(form)) = &view.editor {
            content = content.child(
                form.render(snapshot, busy, stacked, cx)
                    .p_4()
                    .rounded(theme.radius)
                    .bg(theme.secondary.opacity(0.3)),
            );
        }
        content = content.when(view.editor.is_none(), |content| content.child(v_flex().gap_2().p_4().rounded(theme.radius).bg(theme.secondary.opacity(0.45))
            .child(div().text_xs().text_color(theme.muted_foreground).child("Estimated first upload"))
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(snapshot.estimate.as_ref().map(|e| byte_size(e.bytes)).unwrap_or_else(|| "Not available yet".into())))
            .when_some(snapshot.estimate.as_ref(), |content, estimate| content
                .child(div().text_xs().text_color(theme.muted_foreground).child(estimate.basis.clone()))
                .child(div().text_xs().text_color(theme.muted_foreground).child("Later backups upload only what changed since the previous one. Copies share unchanged data in the bucket.")))));
        let mut history = v_flex().gap_3().child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child("Recent remote backups"),
        );
        if snapshot.recent.is_empty() {
            history = history.child(div().text_xs().text_color(theme.muted_foreground).child(
                "No remote backups yet. Configure a destination, then back up your workspace.",
            ));
        }
        for run in snapshot.recent.iter().take(5) {
            let verified = run.remote_status.as_deref() == Some("completed");
            let state = match run.remote_status.as_deref() {
                Some("completed") => "Verified",
                Some("failed") => "Upload failed",
                Some("retired") => "Removed by retention",
                Some(_) => "Upload pending",
                None if run.local_status == "failed" => "Capture failed",
                None if run.local_status == "running" => "Capturing…",
                None => "Local copy only",
            };
            history = history.child(
                v_flex()
                    .gap_1()
                    .pb_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                div().flex_1().text_xs().child(
                                    run.finished_at
                                        .as_deref()
                                        .map(timestamp)
                                        .unwrap_or_else(|| run.set_id.clone()),
                                ),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                match (run.uploaded_bytes, run.bytes) {
                                    (Some(uploaded), _) => {
                                        format!("{} uploaded", byte_size(uploaded))
                                    }
                                    (None, Some(size)) => byte_size(size),
                                    (None, None) => "Size pending".into(),
                                },
                            ))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if verified {
                                        theme.success
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .child(state),
                            ),
                    )
                    .when_some(run.error.clone(), |row, err| {
                        row.child(div().text_xs().text_color(theme.warning).child(err))
                    }),
            );
        }
        content = content.when(view.editor.is_none(), |content| content.child(history))
            .when_some(view.poll_error.clone(), |content, err| {
                content.child(message(err, theme.warning, &theme))
            })
            .child(h_flex().gap_2().items_start().text_xs().text_color(theme.muted_foreground).child(Icon::new(IconName::Lock).size_3()).child("Backups are encrypted before upload. Keep your installation's recovery key safe."));
        // Only an open editor keeps a footer bar, for its save and cancel.
        let mut footer = None;
        if let Some(editor) = &view.editor {
            let mut bar = h_flex().w_full().gap_3().items_center();
            if let Editor::Destination(form) = editor {
                bar = bar
                    .child(
                        Button::new("backup-test-connection")
                            .control(Tier::Secondary, Scale::Standard, busy, cx)
                            .label("Test connection")
                            .loading(view.operation == Operation::Testing)
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.test_backup_destination(window, cx)
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("backup-save-destination")
                            .control(Tier::Primary, Scale::Standard, busy || !form.verified, cx)
                            .label("Save changes")
                            .loading(view.operation == Operation::Saving)
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.save_backup_edit(window, cx)
                            })),
                    );
            } else {
                bar = bar.child(div().flex_1()).child(
                    Button::new("backup-save-policy")
                        .control(Tier::Primary, Scale::Standard, busy, cx)
                        .label("Save changes")
                        .loading(view.operation == Operation::Saving)
                        .on_click(
                            cx.listener(|shell, _, window, cx| shell.save_backup_edit(window, cx)),
                        ),
                );
            }
            bar = bar.child(
                Button::new("backup-cancel-edit")
                    .control(Tier::Secondary, Scale::Standard, busy, cx)
                    .label("Cancel")
                    .on_click(cx.listener(|shell, _, _, cx| shell.cancel_backup_edit(cx))),
            );
            footer = Some(bar);
        }
        let status_line = |text: String, color: Hsla| {
            div().flex_none().pt_2().child(settings_column(
                div().text_xs().text_color(color).child(text),
            ))
        };
        v_flex()
            .size_full()
            .min_h_0()
            .child(
                div().flex_1().min_h_0().child(crate::panel_kit::faded(
                    div()
                        .id("backup-settings-scroll")
                        .size_full()
                        .overflow_y_scrollbar()
                        .child(div().py_8().child(settings_column(page.child(content)))),
                    theme.popover,
                )),
            )
            .when_some(view.error.clone(), |content, text| {
                content.child(status_line(text, theme.warning))
            })
            .when_some(view.feedback.clone(), |content, text| {
                content.child(status_line(text, theme.success))
            })
            .when_some(footer, |content, footer| {
                content.child(
                    div()
                        .flex_none()
                        .py_3()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(settings_column(footer)),
                )
            })
            .into_any_element()
    }
}
