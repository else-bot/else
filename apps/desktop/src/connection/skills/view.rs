use super::*;

impl View {
    pub(super) fn render_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut body = v_flex().gap_5().child(settings::section_heading(
            "Skills",
            "Installed once. Shared skills are available to every bot.",
            cx,
        ));
        let busy = self.busy || !self.connected;
        let Some(entries) = self.entries.value() else {
            return body.child("Loading installed skills…").into_any_element();
        };
        if entries.is_empty() {
            return body
                .flex_1()
                .child(settings::empty_state(
                    IconName::FileText,
                    "No skills installed yet",
                    "Ask a bot to install a skill from a GitHub link.",
                    Button::new("empty-open-chat")
                        .control(Tier::Secondary, Scale::Standard, false, cx)
                        .icon(IconName::MessageSquare)
                        .label("Ask a bot in chat")
                        .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Leave))),
                    cx,
                ))
                .into_any_element();
        }
        body = body.child(
            div().max_w(px(340.)).child(
                text_field(Input::new(&self.search), cx)
                    .aria_label("Search installed skills")
                    .small()
                    .prefix(Icon::new(IconName::Search).size_3()),
            ),
        );
        if let Some(error) = self.error.as_ref().or(self.entries.error()) {
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
                        Button::new("skills-retry")
                            .control(Tier::Secondary, Scale::Standard, busy, cx)
                            .label("Retry")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Reload))),
                    ),
            );
        }
        let query = self.search.read(cx).value().trim().to_lowercase();
        let filtered: Vec<_> = entries
            .iter()
            .filter(|entry| {
                format!("{} {}", entry.name, entry.description)
                    .to_lowercase()
                    .contains(&query)
            })
            .collect();
        let mut list = v_flex().child(
            h_flex()
                .justify_between()
                .pb_2()
                .border_b_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!("Installed · {}", filtered.len()))
                .child(div().pr(px(50.)).child("Access")),
        );
        if filtered.is_empty() {
            list = list.child(
                v_flex()
                    .gap_2()
                    .py_8()
                    .child(if entries.is_empty() {
                        "No skills installed yet"
                    } else {
                        "No matching skills"
                    })
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        if entries.is_empty() {
                            "Ask any bot to install a skill from a GitHub link."
                        } else {
                            "Try a different name or capability."
                        },
                    )),
            );
        }
        for entry in filtered {
            let expanded = self.expanded.as_ref() == Some(&entry.id);
            let expand = entry.clone();
            let remove = entry.clone();
            let mut access = h_flex().gap_2().child(chip(entry.access(), theme.primary));
            if !entry.env.is_empty() {
                access = access.child(chip(
                    if entry.key_stored {
                        "Key set"
                    } else {
                        "Key missing"
                    },
                    if entry.key_stored {
                        theme.success
                    } else {
                        theme.warning
                    },
                ));
            }
            let header = h_flex()
                .gap_3()
                .py_4()
                .child(
                    Button::new(SharedString::from(format!("skill-row-{}", entry.id)))
                        .focus_ring(false)
                        .cursor_pointer()
                        .ghost()
                        .flex_1()
                        .min_w_0()
                        .h_auto()
                        .py_1()
                        .disabled(busy)
                        .accessibility_label(format!(
                            "{} {}",
                            if expanded { "Collapse" } else { "Expand" },
                            entry.name
                        ))
                        .child(
                            h_flex()
                                .w_full()
                                .gap_3()
                                .child(
                                    Icon::new(if expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .size_3(),
                                )
                                .child(
                                    div()
                                        .size_9()
                                        .rounded_md()
                                        .bg(theme.secondary)
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(Icon::new(IconName::FileText).size_5()),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .gap_1()
                                        .child(
                                            div()
                                                .font_weight(FontWeight::MEDIUM)
                                                .child(entry.name.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .truncate()
                                                .child(entry.description.clone()),
                                        ),
                                )
                                .child(access),
                        )
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.expand(expand.clone(), window, cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("skill-remove-{}", entry.id)))
                        .focus_ring(false)
                        .cursor_pointer()
                        .ghost()
                        .small()
                        .icon(IconName::Trash)
                        .text_color(theme.danger)
                        .accessibility_label(format!("Delete {}", entry.name))
                        .disabled(busy)
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.confirm_remove(remove.clone(), window, cx)
                        })),
                );
            let mut row = v_flex()
                .border_b_1()
                .border_color(theme.border)
                .child(header);
            if expanded {
                let date = entry.installed_at.format("%b %-d, %Y").to_string();
                let mut detail = v_flex()
                    .gap_3()
                    .pl_8()
                    .pb_4()
                    .child(div().text_sm().child(entry.description.clone()))
                    .child(h_flex().child(source(&entry.source, cx)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "Installed by {} · {date}",
                                if entry.installed_by_name.is_empty() {
                                    entry.installed_by.to_string()
                                } else {
                                    entry.installed_by_name.clone()
                                }
                            )),
                    );
                if let Some(files) = self.files.get(&entry.id) {
                    let this = cx.weak_entity();
                    let id = entry.id.clone();
                    detail = detail.child(files.render(
                        &entry.id,
                        &entry.files,
                        move |path, _, cx| {
                            let _ = this.update(cx, |view, cx| {
                                if let Some(files) = view.files.get_mut(&id) {
                                    files.selected = Some(path);
                                    cx.notify();
                                }
                            });
                        },
                        cx,
                    ));
                }
                if !entry.env.is_empty() {
                    let replace = entry.clone();
                    let clear = entry.clone();
                    let save = entry.clone();
                    let mut key = v_flex()
                        .gap_2()
                        .p_3()
                        .rounded_md()
                        .bg(theme.secondary)
                        .child(
                            div()
                                .text_xs()
                                .font_semibold()
                                .child(if entry.env.len() == 1 { "Key" } else { "Keys" }),
                        );
                    if entry.key_stored && !self.replacing {
                        key = key.child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .text_sm()
                                        .child(format!("{} · set · ••••", entry.env.join(", "))),
                                )
                                .child(
                                    Button::new("skill-key-replace")
                                        .control(Tier::Secondary, Scale::Standard, busy, cx)
                                        .label("Replace")
                                        .on_click(cx.listener(move |view, _, window, cx| {
                                            view.edit_key(replace.clone(), window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("skill-key-clear")
                                        .control(Tier::Secondary, Scale::Standard, busy, cx)
                                        .label("Clear")
                                        .on_click(cx.listener(move |view, _, window, cx| {
                                            view.write_key(clear.clone(), false, window, cx)
                                        })),
                                ),
                        );
                    } else if let Some(keys) = &self.keys {
                        key = key.child(keys.clone()).child(
                            h_flex().justify_end().child(
                                Button::new("skill-key-save")
                                    .control(Tier::Primary, Scale::Standard, busy, cx)
                                    .label("Save")
                                    .on_click(cx.listener(move |view, _, window, cx| {
                                        view.write_key(save.clone(), true, window, cx)
                                    })),
                            ),
                        );
                    } else {
                        key = key.child(
                            Button::new("skill-key-edit")
                                .control(Tier::Secondary, Scale::Standard, busy, cx)
                                .label("Add key")
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.edit_key(replace.clone(), window, cx)
                                })),
                        );
                    }
                    detail = detail.child(key).child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "Saved as the task secret {}. Never shown in chat.",
                                entry.secret
                            )),
                    );
                }
                row = row.child(detail);
            }
            list = list.child(row);
        }
        body.child(list)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Removing a shared skill removes it for every bot."),
            )
            .into_any_element()
    }
}
