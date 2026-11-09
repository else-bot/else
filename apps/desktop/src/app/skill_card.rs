use std::collections::HashSet;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Selectable as _, Sizable as _,
    StyledExt as _, h_flex, v_flex,
};
use gpui_kit::*;
use uuid::Uuid;

use super::{Connection, ElseApp, RowKey};
use crate::api::ClientError;
use crate::model::{SkillInstall, SkillInstallStatus, SkillScope};
use crate::skill_ui::{SkillFiles, SkillKeys, chip, source};

pub(super) struct SkillCard {
    scope: SkillScope,
    keys: Option<Entity<SkillKeys>>,
    files: SkillFiles,
    submitting: bool,
    error: Option<String>,
}

impl ElseApp {
    pub(super) fn sync_skill_cards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connection != Connection::Connected {
            self.clear_skill_cards(window, cx);
            return;
        }
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let installs: Vec<_> = view
            .conversation
            .iter()
            .flat_map(|c| c.turns_in_order())
            .flat_map(|t| &t.skill_installs)
            .cloned()
            .collect();
        let ids: HashSet<_> = installs.iter().map(|i| i.id).collect();
        view.skill_cards.retain(|id, card| {
            if ids.contains(id) {
                return true;
            }
            if let Some(keys) = card.keys.take() {
                keys.update(cx, |keys, cx| keys.clear(window, cx));
            }
            false
        });
        let mut unopened = Vec::new();
        for install in installs {
            if install.status == SkillInstallStatus::Pending
                && view
                    .skill_cards
                    .get(&install.id)
                    .is_none_or(|card| card.files.selected.is_none())
                && install.files.iter().any(|file| file.path == "SKILL.md")
            {
                unopened.push(install.id);
            }
            let card = view
                .skill_cards
                .entry(install.id)
                .or_insert_with(|| SkillCard {
                    scope: SkillScope::All,
                    keys: None,
                    files: SkillFiles::default(),
                    submitting: false,
                    error: None,
                });
            if install.status == SkillInstallStatus::Pending {
                if card.keys.is_none() && !install.env.is_empty() {
                    card.keys = Some(cx.new(|cx| SkillKeys::new(&install.env, window, cx)));
                }
            } else if let Some(keys) = card.keys.take() {
                keys.update(cx, |keys, cx| keys.clear(window, cx));
            }
        }
        for id in unopened {
            self.select_skill_file(id, "SKILL.md".into(), window, cx);
        }
    }

    pub(super) fn clear_skill_cards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.view.as_mut() {
            for (_, card) in view.skill_cards.drain() {
                if let Some(keys) = card.keys {
                    keys.update(cx, |keys, cx| keys.clear(window, cx));
                }
            }
        }
    }

    fn select_skill_file(
        &mut self,
        id: Uuid,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let bot = view.bot_id;
        let Some(card) = view.skill_cards.get_mut(&id) else {
            return;
        };
        card.files.selected = Some(path);
        if card.files.contents.is_some() || card.files.loading {
            self.remeasure_row(RowKey::SkillInstall(id), cx);
            return;
        }
        card.files.loading = true;
        card.files.error = None;
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.skill_install(bot, id).await;
            let _ = this.update_in(cx, |app, _, cx| {
                let Some(card) = app
                    .view
                    .as_mut()
                    .filter(|v| v.bot_id == bot)
                    .and_then(|v| v.skill_cards.get_mut(&id))
                else {
                    return;
                };
                card.files.loading = false;
                match result {
                    Ok(detail) => card.files.contents = Some(detail.contents),
                    Err(err) => card.files.error = Some(err.to_string()),
                }
                app.remeasure_row(RowKey::SkillInstall(id), cx);
            });
        })
        .detach();
        self.remeasure_row(RowKey::SkillInstall(id), cx);
    }

    fn answer_skill(
        &mut self,
        id: Uuid,
        approve: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connection != Connection::Connected {
            return;
        }
        let Some(view) = self.view.as_mut() else {
            return;
        };
        if !view
            .conversation
            .as_ref()
            .and_then(|c| c.skill_install(id))
            .is_some_and(|install| install.status == SkillInstallStatus::Pending)
        {
            return;
        }
        let bot = view.bot_id;
        let Some(card) = view
            .skill_cards
            .get_mut(&id)
            .filter(|card| !card.submitting)
        else {
            return;
        };
        let env = if approve {
            match card
                .keys
                .as_ref()
                .map(|keys| keys.read(cx).values(true, cx))
                .transpose()
            {
                Ok(env) => env.flatten(),
                Err(error) => {
                    card.error = Some(error);
                    self.remeasure_row(RowKey::SkillInstall(id), cx);
                    return;
                }
            }
        } else {
            if let Some(keys) = &card.keys {
                keys.update(cx, |keys, cx| keys.clear(window, cx));
            }
            None
        };
        let approval = approve.then_some((card.scope, env));
        card.submitting = true;
        card.error = None;
        if let Some(keys) = &card.keys {
            keys.update(cx, |keys, cx| {
                keys.busy = true;
                cx.notify();
            });
        }
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.answer_skill_install(bot, id, approval).await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.skill_answered(bot, id, result, window, cx)
            });
        })
        .detach();
        self.remeasure_row(RowKey::SkillInstall(id), cx);
    }

    fn skill_answered(
        &mut self,
        bot: Uuid,
        id: Uuid,
        result: Result<SkillInstall, ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(&result, Err(ClientError::Unauthorized)) {
            self.connection = Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
            self.clear_secret_forms(window, cx);
            cx.notify();
            return;
        }
        let Some(view) = self.view.as_mut().filter(|v| v.bot_id == bot) else {
            return;
        };
        if let Some(card) = view.skill_cards.get_mut(&id) {
            card.submitting = false;
            match result {
                Ok(install) => {
                    if let Some(keys) = card.keys.take() {
                        keys.update(cx, |keys, cx| keys.clear(window, cx));
                    }
                    if let Some(c) = view.conversation.as_mut() {
                        c.apply_skill_install(install);
                    }
                }
                Err(error) => {
                    card.error = Some(error.to_string());
                    if let Some(keys) = &card.keys {
                        keys.update(cx, |keys, cx| {
                            keys.busy = false;
                            cx.notify();
                        });
                    }
                }
            }
        }
        self.remeasure_row(RowKey::SkillInstall(id), cx);
    }

    pub(super) fn render_skill_card(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = &self.view else {
            return div().into_any_element();
        };
        let Some(install) = view.conversation.as_ref().and_then(|c| c.skill_install(id)) else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let bot = self
            .bots
            .iter()
            .find(|b| b.id == view.bot_id)
            .map(|b| b.name.as_str())
            .unwrap_or("Bot");
        let mut body = v_flex()
            .w_full()
            .gap_3()
            .p_4()
            .rounded(theme.radius_lg)
            .bg(theme.secondary)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{bot} wants to install a skill")),
            )
            .child(div().text_lg().font_semibold().child(install.name.clone()))
            .child(div().text_sm().child(install.description.clone()))
            .child(h_flex().child(source(&install.source, cx)));
        if install.status != SkillInstallStatus::Pending {
            let (label, color) = match install.status {
                SkillInstallStatus::Installed => ("Installed", theme.success),
                SkillInstallStatus::Declined => ("Declined", theme.muted_foreground),
                _ => ("The request ended", theme.muted_foreground),
            };
            let mut settled = h_flex().gap_2().flex_wrap().child(chip(label, color));
            if install.status == SkillInstallStatus::Installed {
                settled = settled
                    .child(div().text_xs().child(format!(
                        "{} · {}",
                        if install.scope == Some(SkillScope::All) {
                            "All bots".into()
                        } else {
                            format!("Only {bot}")
                        },
                        if install.key_stored {
                            "key saved"
                        } else {
                            "key not set"
                        }
                    )))
                    .child(
                        Button::new(SharedString::from(format!("skill-settings-{id}")))
                            .focus_ring(false)
                            .cursor_pointer()
                            .small()
                            .ghost()
                            .label("Open in Settings")
                            .on_click(cx.listener(|app, _, window, cx| {
                                // A click leaves nothing focused, and an action
                                // without focus never reaches the Shell's handler.
                                app.sidebar_focus.focus(window, cx);
                                window.dispatch_action(
                                    Box::new(crate::connection::OpenSkillsSettings),
                                    cx,
                                )
                            })),
                    );
            }
            return body.child(settled).into_any_element();
        }
        let Some(card) = view.skill_cards.get(&id) else {
            return body.child("Connecting…").into_any_element();
        };
        let busy = card.submitting || self.connection != Connection::Connected;
        let scripts = install.files.iter().filter(|file| file.script).count();
        let mut chips = h_flex().gap_2().flex_wrap().child(chip(
            format!(
                "{} {} · {}",
                install.files.len(),
                if install.files.len() == 1 {
                    "file"
                } else {
                    "files"
                },
                crate::skill_ui::size(install.files.iter().map(|file| file.size).sum::<u64>())
            ),
            theme.muted_foreground,
        ));
        if scripts > 0 {
            chips = chips.child(chip(
                format!(
                    "{scripts} {} runs code",
                    if scripts == 1 { "script" } else { "scripts" }
                ),
                theme.warning,
            ));
        }
        for var in &install.env {
            chips = chips.child(chip(format!("Needs {var}"), theme.muted_foreground));
        }
        let app = cx.weak_entity();
        body = body
            .child(chips)
            .child(card.files.render(
                &id.to_string(),
                &install.files,
                move |path, window, cx| {
                    let _ = app.update(cx, |app, cx| app.select_skill_file(id, path, window, cx));
                },
                cx,
            ))
            .child(div().text_xs().font_semibold().child("Who can use it"));
        let mut scopes = h_flex().gap_2();
        for (scope, label) in [
            (SkillScope::All, "All bots".to_owned()),
            (SkillScope::Bot, format!("Only {bot}")),
        ] {
            scopes = scopes.child(
                Button::new(SharedString::from(format!("skill-scope-{id}-{scope:?}")))
                    .focus_ring(false)
                    .cursor_pointer()
                    .small()
                    .flex_1()
                    .label(label)
                    .selected(card.scope == scope)
                    .disabled(busy)
                    .on_click(cx.listener(move |app, _, _, cx| {
                        if let Some(card) =
                            app.view.as_mut().and_then(|v| v.skill_cards.get_mut(&id))
                        {
                            card.scope = scope;
                        }
                        app.remeasure_row(RowKey::SkillInstall(id), cx);
                    })),
            );
        }
        body = body.child(scopes);
        if let Some(keys) = &card.keys {
            body = body.child(keys.clone()).child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "Saved as the task secret {}. Never shown in chat.",
                        install.secret
                    )),
            );
        }
        body = body.child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("You can skip the key and add it later in Settings → Skills."),
        );
        if let Some(error) = &card.error {
            body = body.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        let label = match card.scope {
            SkillScope::All => "Install for all bots".to_owned(),
            SkillScope::Bot => format!("Install for {bot}"),
        };
        body.child(
            h_flex()
                .justify_end()
                .gap_2()
                .child(
                    Button::new(SharedString::from(format!("skill-decline-{id}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .small()
                        .ghost()
                        .label("Decline Esc")
                        .disabled(busy)
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.answer_skill(id, false, window, cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("skill-approve-{id}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .small()
                        .primary()
                        .label(format!(
                            "{label} {}↵",
                            if cfg!(target_os = "macos") {
                                "⌘"
                            } else {
                                "Ctrl+"
                            }
                        ))
                        .disabled(busy)
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.answer_skill(id, true, window, cx)
                        })),
                ),
        )
        .on_key_down(cx.listener(move |app, event: &KeyDownEvent, window, cx| {
            if event.keystroke.key == "escape" {
                // Declining is final; Esc while typing a key would lose it.
                let typing = app
                    .view
                    .as_ref()
                    .and_then(|view| view.skill_cards.get(&id))
                    .and_then(|card| card.keys.as_ref())
                    .is_some_and(|keys| !keys.read(cx).is_empty(cx));
                if typing {
                    return;
                }
                app.answer_skill(id, false, window, cx);
                cx.stop_propagation();
            } else if event.keystroke.key == "enter"
                && (if cfg!(target_os = "macos") {
                    event.keystroke.modifiers.platform
                } else {
                    event.keystroke.modifiers.control
                })
            {
                app.answer_skill(id, true, window, cx);
                cx.stop_propagation();
            }
        }))
        .into_any_element()
    }
}
