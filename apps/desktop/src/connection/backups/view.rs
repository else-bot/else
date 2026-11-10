use super::*;
use chrono::{DateTime, Local, Utc};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable as _, FocusableExt as _, Icon, Sizable as _, Theme, h_flex};

pub(super) fn bytes(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.2} GB", value as f64 / 1_000_000_000.)
    } else if value >= 1_000_000 {
        format!("{:.1} MB", value as f64 / 1_000_000.)
    } else if value >= 1000 {
        format!("{:.1} KB", value as f64 / 1000.)
    } else {
        format!("{value} B")
    }
}

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

fn metric(label: &'static str, value: String, detail: String, theme: &Theme) -> Div {
    v_flex()
        .flex_1()
        .min_w_0()
        .gap_1p5()
        .p_3()
        .rounded(theme.radius)
        .bg(theme.secondary.opacity(0.45))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(label),
        )
        .child(
            div()
                .text_lg()
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

fn message(text: String, color: Hsla, theme: &Theme) -> Div {
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
                    "Remote backups",
                    "Keep a remote copy of your workspace.",
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
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Try again")
                                .on_click(cx.listener(|shell, _, window, cx| {
                                    shell.open_backups(window, cx)
                                })),
                        )
                    },
                )
                .into_any_element();
        };
        let busy = view.busy();
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
        let mut content = v_flex()
            .gap_5()
            .child(super::super::settings::section_heading(
                "Remote backups",
                "Your workspace, backed up off this server.",
                cx,
            ))
            .child(
                h_flex()
                    .justify_between()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .text_color(tone)
                            .child(div().size_1p5().rounded_full().bg(tone))
                            .child(status),
                    )
                    .child(
                        Switch::new("remote-backups-enabled")
                            .focus_ring(false)
                            .cursor_pointer()
                            .small()
                            .checked(snapshot.enabled)
                            .disabled(
                                busy || unavailable || !has_destination || view.editor.is_some(),
                            )
                            .accessibility_label("Automatic remote backups")
                            .tooltip(if snapshot.enabled {
                                "Pause automatic remote backups"
                            } else {
                                "Enable automatic remote backups"
                            })
                            .on_click(cx.listener(|shell, enabled: &bool, window, cx| {
                                shell.toggle_remote_backups(*enabled, window, cx)
                            })),
                    ),
            )
            .when_some(snapshot.unavailable_reason.clone(), |content, reason| {
                content.child(message(reason, theme.warning, &theme))
            })
            .when_some(snapshot.last_error.clone(), |content, reason| {
                content.child(message(reason, theme.warning, &theme))
            })
            .when(view.editor.is_none(), |content| {
                content.child(
                    h_flex()
                        .items_stretch()
                        .gap_3()
                        .when(stacked, |row| row.flex_col())
                        .child(metric(
                            "Next backup",
                            next_backup(snapshot),
                            interval(snapshot.interval_secs),
                            &theme,
                        ))
                        .child(metric(
                            "Uploaded last run",
                            latest
                                .and_then(|run| run.uploaded_bytes.or(run.bytes))
                                .map(bytes)
                                .unwrap_or_else(|| "None yet".into()),
                            latest
                                .and_then(|run| run.finished_at.as_deref())
                                .map(timestamp)
                                .unwrap_or_else(|| "After your first remote backup".into()),
                            &theme,
                        ))
                        .child(metric(
                            "Remote copies",
                            format!("{} copies", snapshot.remote_count),
                            format!(
                                "Keep target {} · {} in the bucket",
                                snapshot.keep,
                                bytes(snapshot.remote_bytes)
                            ),
                            &theme,
                        )),
                )
            });
        if let Some(destination) = &snapshot.destination {
            content = content.when(
                !matches!(view.editor, Some(Editor::Destination(_))),
                |content| {
                    content.child(
                        h_flex()
                            .items_center()
                            .gap_3()
                            .pb_4()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(
                                Icon::new(IconName::CloudUpload)
                                    .size_5()
                                    .text_color(theme.muted_foreground),
                            )
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
                                    .focus_ring(false)
                                    .cursor_pointer()
                                    .label("Edit destination")
                                    .icon(IconName::Pencil)
                                    .secondary()
                                    .disabled(busy || view.editor.is_some())
                                    .on_click(cx.listener(|shell, _, window, cx| {
                                        shell.edit_backup_destination(window, cx)
                                    })),
                            ),
                    )
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
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Configure destination")
                                .primary()
                                .disabled(busy || view.editor.is_some())
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
                content.child(
                    h_flex()
                        .items_center()
                        .gap_3()
                        .pb_4()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            Icon::new(IconName::Clock)
                                .size_5()
                                .text_color(theme.muted_foreground),
                        )
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
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Edit policy")
                                .icon(IconName::Pencil)
                                .secondary()
                                .disabled(busy || view.editor.is_some())
                                .on_click(cx.listener(|shell, _, window, cx| {
                                    shell.edit_backup_policy(window, cx)
                                })),
                        ),
                )
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
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(snapshot.estimate.as_ref().map(|e| bytes(e.bytes)).unwrap_or_else(|| "Not available yet".into())))
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
                                    (Some(uploaded), _) => format!("{} uploaded", bytes(uploaded)),
                                    (None, Some(size)) => bytes(size),
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
        let mut footer = h_flex()
            .flex_none()
            .px_6()
            .py_3()
            .gap_3()
            .border_t_1()
            .border_color(theme.border);
        if let Some(editor) = &view.editor {
            if let Editor::Destination(form) = editor {
                footer = footer
                    .child(
                        Button::new("backup-test-connection")
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Test connection")
                            .loading(view.operation == Operation::Testing)
                            .disabled(busy)
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.test_backup_destination(window, cx)
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("backup-save-destination")
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Save changes")
                            .primary()
                            .loading(view.operation == Operation::Saving)
                            .disabled(busy || !form.verified)
                            .on_click(cx.listener(|shell, _, window, cx| {
                                shell.save_backup_edit(window, cx)
                            })),
                    );
            } else {
                footer = footer.child(div().flex_1()).child(
                    Button::new("backup-save-policy")
                        .focus_ring(false)
                        .cursor_pointer()
                        .label("Save changes")
                        .primary()
                        .loading(view.operation == Operation::Saving)
                        .disabled(busy)
                        .on_click(
                            cx.listener(|shell, _, window, cx| shell.save_backup_edit(window, cx)),
                        ),
                );
            }
            footer = footer.child(
                Button::new("backup-cancel-edit")
                    .focus_ring(false)
                    .cursor_pointer()
                    .label("Cancel")
                    .disabled(busy)
                    .on_click(cx.listener(|shell, _, _, cx| shell.cancel_backup_edit(cx))),
            );
        } else {
            footer = footer
                .child(
                    div()
                        .flex_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(if snapshot.running {
                            "You can leave settings while this backup runs."
                        } else {
                            "Old copies are removed after a new backup is verified."
                        }),
                )
                .child(
                    Button::new("backup-run-now")
                        .focus_ring(false)
                        .cursor_pointer()
                        .label(if snapshot.running {
                            "Backing up…"
                        } else {
                            "Back up now"
                        })
                        .primary()
                        .loading(view.operation == Operation::Starting)
                        .disabled(busy || snapshot.running || !has_destination || unavailable)
                        .on_click(
                            cx.listener(|shell, _, window, cx| shell.run_remote_backup(window, cx)),
                        ),
                );
        }
        v_flex()
            .size_full()
            .min_h_0()
            .child(
                div()
                    .id("backup-settings-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .child(content.p_6()),
            )
            .when_some(view.error.clone(), |content, text| {
                content.child(
                    div()
                        .px_6()
                        .py_2()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(text),
                )
            })
            .when_some(view.feedback.clone(), |content, text| {
                content.child(
                    div()
                        .px_6()
                        .py_2()
                        .text_xs()
                        .text_color(theme.success)
                        .child(text),
                )
            })
            .child(footer)
            .into_any_element()
    }
}
