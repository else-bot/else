use super::*;
use crate::api::InstalledSkill;
use crate::model::SkillScope;
use crate::skill_ui::{SkillFiles, SkillKeys, chip, source};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::{
    Disableable as _, FocusableExt as _, Icon, Sizable as _, StyledExt as _, h_flex,
};
use std::time::Duration;

pub(super) struct View {
    id: Uuid,
    client: Arc<Client>,
    entries: Option<Vec<InstalledSkill>>,
    search: Entity<InputState>,
    expanded: Option<String>,
    files: HashMap<String, SkillFiles>,
    keys: Option<Entity<SkillKeys>>,
    replacing: bool,
    revision: u64,
    busy: bool,
    error: Option<String>,
    poll_error: Option<String>,
    _search: Subscription,
    _poll: Task<()>,
}

impl View {
    pub(super) fn has_unsaved_work(&self, cx: &App) -> bool {
        self.busy
            || self
                .keys
                .as_ref()
                .is_some_and(|keys| !keys.read(cx).is_empty(cx))
    }

    fn clear_keys(&mut self, window: &mut Window, cx: &mut App) {
        if let Some(keys) = self.keys.take() {
            keys.update(cx, |keys, cx| keys.clear(window, cx));
        }
        self.replacing = false;
    }
}

impl Shell {
    pub(super) fn clear_skill_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.skills.as_mut() {
            view.clear_keys(window, cx);
        }
    }

    pub(super) fn clear_skills(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(mut view) = self.skills.take() {
            view.clear_keys(window, cx);
        }
    }

    pub(super) fn open_skills(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear_skills(window, cx);
        let Some(active) = &self.active else { return };
        let client = active.client.clone();
        let poll_client = client.clone();
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Search installed skills"));
        let subscription = cx.subscribe_in(&search, window, |_, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let id = Uuid::new_v4();
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                let revision = this.update_in(cx, |shell, window, cx| {
                    let connected = shell
                        .active
                        .as_ref()
                        .is_some_and(|active| active.app.read(cx).connected());
                    let view = shell.skills.as_mut().filter(|view| view.id == id)?;
                    if !connected {
                        view.clear_keys(window, cx);
                    }
                    Some(view.revision)
                });
                let Ok(Some(revision)) = revision else { break };
                let result = poll_client.clone().installed_skills().await;
                let active = this.update_in(cx, |shell, window, cx| {
                    let connected = shell
                        .active
                        .as_ref()
                        .is_some_and(|active| active.app.read(cx).connected());
                    let Some(view) = shell.skills.as_mut().filter(|view| view.id == id) else {
                        return false;
                    };
                    if view.revision == revision && !view.busy {
                        match result {
                            Ok(entries) => {
                                view.files.retain(|id, _| {
                                    view.entries
                                        .iter()
                                        .flatten()
                                        .find(|old| old.id == *id)
                                        .zip(entries.iter().find(|new| new.id == *id))
                                        .is_some_and(|(old, new)| {
                                            old.installed_at == new.installed_at
                                                && old.source == new.source
                                        })
                                });
                                if let Some(expanded) = view.expanded.clone() {
                                    let replaced = view
                                        .entries
                                        .iter()
                                        .flatten()
                                        .find(|old| old.id == expanded)
                                        .zip(entries.iter().find(|new| new.id == expanded))
                                        .is_some_and(|(old, new)| {
                                            old.installed_at != new.installed_at
                                                || old.source != new.source
                                        });
                                    if replaced {
                                        view.clear_keys(window, cx);
                                        view.expanded = None;
                                    }
                                }
                                if let Some(expanded) = &view.expanded {
                                    let current =
                                        entries.iter().find(|entry| entry.id == *expanded);
                                    if current.is_none()
                                        || current.is_some_and(|entry| entry.key_stored)
                                            && !view.replacing
                                    {
                                        view.clear_keys(window, cx);
                                    }
                                    if connected
                                        && let Some(entry) = current.filter(|entry| {
                                            !entry.key_stored && !entry.env.is_empty()
                                        })
                                        && view.keys.is_none()
                                    {
                                        view.keys = Some(
                                            cx.new(|cx| SkillKeys::new(&entry.env, window, cx)),
                                        );
                                    }
                                    if current.is_none() {
                                        view.expanded = None;
                                    }
                                }
                                view.entries = Some(entries);
                                view.poll_error = None;
                            }
                            Err(error) => view.poll_error = Some(error.to_string()),
                        }
                        cx.notify();
                    }
                    true
                });
                if !matches!(active, Ok(true)) {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        });
        self.skills = Some(View {
            id,
            client,
            entries: None,
            search,
            expanded: None,
            files: HashMap::new(),
            keys: None,
            replacing: false,
            revision: 0,
            busy: false,
            error: None,
            poll_error: None,
            _search: subscription,
            _poll: poll,
        });
        cx.notify();
    }

    fn expand_skill(&mut self, entry: InstalledSkill, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.skills.as_mut().filter(|view| !view.busy) else {
            return;
        };
        view.clear_keys(window, cx);
        if view.expanded.as_ref() == Some(&entry.id) {
            view.expanded = None;
            cx.notify();
            return;
        }
        view.expanded = Some(entry.id.clone());
        view.error = None;
        if !entry.env.is_empty() && !entry.key_stored {
            view.keys = Some(cx.new(|cx| SkillKeys::new(&entry.env, window, cx)));
        }
        let files = view.files.entry(entry.id.clone()).or_default();
        if files.selected.is_none() {
            files.selected = entry
                .files
                .iter()
                .find(|file| file.path == "SKILL.md")
                .or(entry.files.first())
                .map(|file| file.path.clone());
        }
        if files.contents.is_some() || files.loading {
            cx.notify();
            return;
        }
        files.loading = true;
        files.error = None;
        let view_id = view.id;
        let client = view.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.skill_detail(entry.id.clone()).await;
            let _ = this.update_in(cx, |shell, _, cx| {
                let Some(view) = shell.skills.as_mut().filter(|view| view.id == view_id) else {
                    return;
                };
                if !view.entries.iter().flatten().any(|current| {
                    current.id == entry.id
                        && current.installed_at == entry.installed_at
                        && current.source == entry.source
                }) {
                    return;
                }
                let Some(files) = view.files.get_mut(&entry.id) else {
                    return;
                };
                files.loading = false;
                match result {
                    Ok(detail) => {
                        files.contents = Some(detail.contents);
                    }
                    Err(error) => files.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn edit_skill_key(
        &mut self,
        entry: InstalledSkill,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.skills.as_mut().filter(|view| !view.busy) else {
            return;
        };
        view.clear_keys(window, cx);
        view.replacing = true;
        view.keys = Some(cx.new(|cx| SkillKeys::new(&entry.env, window, cx)));
        cx.notify();
    }

    fn write_skill_key(
        &mut self,
        entry: InstalledSkill,
        save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.skills.as_mut().filter(|view| !view.busy) else {
            return;
        };
        let env = if save {
            let Some(keys) = &view.keys else { return };
            match keys.read(cx).values(false, cx) {
                Ok(env) => env,
                Err(error) => {
                    view.error = Some(error);
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        view.busy = true;
        view.revision += 1;
        view.error = None;
        if let Some(keys) = &view.keys {
            keys.update(cx, |keys, cx| {
                keys.busy = true;
                cx.notify();
            });
        }
        let view_id = view.id;
        let client = view.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.save_skill_key(entry.secret, env).await;
            let _ = this.update_in(cx, |shell, window, cx| {
                let Some(view) = shell.skills.as_mut().filter(|view| view.id == view_id) else {
                    return;
                };
                view.busy = false;
                view.revision += 1;
                match result {
                    Ok(()) => {
                        view.clear_keys(window, cx);
                        if let Some(current) = view
                            .entries
                            .iter_mut()
                            .flatten()
                            .find(|current| current.id == entry.id)
                        {
                            current.key_stored = save;
                        }
                        if !save {
                            view.keys = Some(cx.new(|cx| SkillKeys::new(&entry.env, window, cx)));
                        }
                    }
                    Err(error) => {
                        view.error = Some(error.to_string());
                        view.clear_keys(window, cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn confirm_remove_skill(
        &mut self,
        entry: InstalledSkill,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.skills.as_mut().filter(|view| !view.busy) else {
            return;
        };
        view.clear_keys(window, cx);
        let view_id = view.id;
        let shell = cx.weak_entity();
        let cancel = cx.focus_handle();
        let focus_once = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_once.replace(false) {
                cancel.focus(window, cx);
            }
            let shell = shell.clone();
            let id = entry.id.clone();
            let shared = entry.scope == SkillScope::All;
            let title = if shared {
                format!("Delete {} for every bot?", entry.name)
            } else {
                format!("Delete {}?", entry.name)
            };
            dialog
                .w(px(440.))
                .title(title)
                .close_button(false)
                .on_ok(|_, _, _| false)
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("This deletes the skill's files and its stored key. Bots using it lose access. You can ask a bot to install it again later."),
                )
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("skill-remove-cancel")
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Cancel")
                                                                .track_focus(&cancel)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("skill-remove-confirm")
                                .focus_ring(false)
                                .cursor_pointer()
                                .danger()
                                .label(if shared { "Delete for all bots" } else { "Delete" })
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = shell.update(cx, |shell, cx| {
                                        shell.remove_skill(view_id, id.clone(), window, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    fn remove_skill(
        &mut self,
        view_id: Uuid,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .skills
            .as_mut()
            .filter(|view| view.id == view_id && !view.busy)
        else {
            return;
        };
        view.busy = true;
        view.revision += 1;
        view.error = None;
        let client = view.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.remove_skill(id.clone()).await;
            let _ = this.update_in(cx, |shell, window, cx| {
                let Some(view) = shell.skills.as_mut().filter(|view| view.id == view_id) else {
                    return;
                };
                view.busy = false;
                view.revision += 1;
                match result {
                    Ok(()) => {
                        if let Some(entries) = &mut view.entries {
                            entries.retain(|entry| entry.id != id);
                        }
                        view.files.remove(&id);
                        if view.expanded.as_ref() == Some(&id) {
                            view.clear_keys(window, cx);
                            view.expanded = None;
                        }
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_skills(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut body = v_flex().gap_5().child(settings::section_heading(
            "Skills",
            "Installed once. Shared skills are available to every bot.",
            cx,
        ));
        let connected = self
            .active
            .as_ref()
            .is_some_and(|active| active.app.read(cx).connected());
        let Some(view) = &self.skills else {
            return body.child("Loading installed skills…").into_any_element();
        };
        let busy = view.busy || !connected;
        let Some(entries) = &view.entries else {
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
                        .focus_ring(false)
                        .cursor_pointer()
                        .label("Open chat")
                        .on_click(cx.listener(|shell, _, window, cx| {
                            window.close_dialog(cx);
                            shell.cancel(window, cx);
                        })),
                    cx,
                ))
                .into_any_element();
        }
        body = body.child(
            div().max_w(px(340.)).child(
                Input::new(&view.search)
                    .focus_ring(false)
                    .aria_label("Search installed skills")
                    .small()
                    .prefix(Icon::new(IconName::Search).size_3()),
            ),
        );
        if let Some(error) = view.error.as_ref().or(view.poll_error.as_ref()) {
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
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Retry")
                            .disabled(busy)
                            .on_click(
                                cx.listener(|shell, _, window, cx| shell.open_skills(window, cx)),
                            ),
                    ),
            );
        }
        let query = view.search.read(cx).value().trim().to_lowercase();
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
            let expanded = view.expanded.as_ref() == Some(&entry.id);
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
                        .on_click(cx.listener(move |shell, _, window, cx| {
                            shell.expand_skill(expand.clone(), window, cx)
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
                        .on_click(cx.listener(move |shell, _, window, cx| {
                            shell.confirm_remove_skill(remove.clone(), window, cx)
                        })),
                );
            let mut row = v_flex()
                .border_b_1()
                .border_color(theme.border)
                .child(header);
            if expanded {
                let date = chrono::DateTime::parse_from_rfc3339(&entry.installed_at)
                    .map(|date| date.format("%b %-d, %Y").to_string())
                    .unwrap_or_else(|_| entry.installed_at.clone());
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
                if let Some(files) = view.files.get(&entry.id) {
                    let shell = cx.weak_entity();
                    let id = entry.id.clone();
                    detail = detail.child(files.render(
                        &entry.id,
                        &entry.files,
                        move |path, _, cx| {
                            let _ = shell.update(cx, |shell, cx| {
                                if let Some(files) = shell
                                    .skills
                                    .as_mut()
                                    .and_then(|view| view.files.get_mut(&id))
                                {
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
                    if entry.key_stored && !view.replacing {
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
                                        .focus_ring(false)
                                        .cursor_pointer()
                                        .label("Replace")
                                        .disabled(busy)
                                        .on_click(cx.listener(move |shell, _, window, cx| {
                                            shell.edit_skill_key(replace.clone(), window, cx)
                                        })),
                                )
                                .child(
                                    Button::new("skill-key-clear")
                                        .focus_ring(false)
                                        .cursor_pointer()
                                        .label("Clear")
                                        .disabled(busy)
                                        .on_click(cx.listener(move |shell, _, window, cx| {
                                            shell.write_skill_key(clear.clone(), false, window, cx)
                                        })),
                                ),
                        );
                    } else if let Some(keys) = &view.keys {
                        key = key.child(keys.clone()).child(
                            h_flex().justify_end().child(
                                Button::new("skill-key-save")
                                    .focus_ring(false)
                                    .cursor_pointer()
                                    .primary()
                                    .label("Save")
                                    .disabled(busy)
                                    .on_click(cx.listener(move |shell, _, window, cx| {
                                        shell.write_skill_key(save.clone(), true, window, cx)
                                    })),
                            ),
                        );
                    } else {
                        key = key.child(
                            Button::new("skill-key-edit")
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Add key")
                                .disabled(busy)
                                .on_click(cx.listener(move |shell, _, window, cx| {
                                    shell.edit_skill_key(replace.clone(), window, cx)
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
