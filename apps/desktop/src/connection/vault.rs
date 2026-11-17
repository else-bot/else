//! Owner-only Vault UI. Inputs and revealed values are transient and cleared
//! on hide, navigation, server switching, and connection loss.
use super::*;
use crate::api::{
    PatchSecret, SaveSecret, VaultEntry, valid_secret_name, validate_secret_metadata,
    vault_last_used,
};
use crate::controls::{Control as _, Scale, Tier, text_field};
use crate::credential_fields::CredentialFields;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::Input;
use gpui_kit::component::{StyledExt as _, h_flex};
use std::collections::BTreeMap;

pub(super) struct View {
    client: Arc<Client>,
    entries: Option<Vec<VaultEntry>>,
    phase: Phase,
    error: Option<String>,
    notice: Option<String>,
    task: Option<Task<()>>,
}

enum Phase {
    Closed,
    Browsing,
    Editing(Editor),
    Saving(Editor),
    Revealing(String),
    Revealed {
        name: String,
        env: BTreeMap<String, String>,
    },
    Deleting(String),
    Unconfirmed(String),
}

struct Editor {
    original: Option<VaultEntry>,
    name: Entity<InputState>,
    description: Entity<InputState>,
    website: Entity<InputState>,
    fields: Entity<CredentialFields>,
}

impl Shell {
    pub(super) fn open_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = &self.active else { return };
        let client = active.client.clone();
        self.vault = Some(cx.new(|cx| View::new(client, window, cx)));
    }

    pub(super) fn clear_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.vault.take() {
            view.update(cx, |view, cx| view.close(window, cx));
        }
    }
}

impl View {
    fn new(client: Arc<Client>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            client,
            entries: None,
            phase: Phase::Browsing,
            error: None,
            notice: None,
            task: None,
        };
        this.refresh(window, cx);
        this
    }

    pub(super) fn has_unsaved_work(&self) -> bool {
        matches!(
            self.phase,
            Phase::Editing(_) | Phase::Saving(_) | Phase::Deleting(_)
        )
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear(window, cx);
        self.phase = Phase::Closed;
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.task = None;
        if let Phase::Editing(editor) | Phase::Saving(editor) = &self.phase {
            editor
                .fields
                .update(cx, |fields, cx| fields.clear(window, cx));
        }
        self.phase = Phase::Browsing;
        cx.notify();
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.vault().await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(entries) => {
                        this.entries = Some(entries);
                        this.error = None;
                    }
                    Err(error) => this.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
    }

    fn edit(&mut self, entry: Option<VaultEntry>, window: &mut Window, cx: &mut Context<Self>) {
        self.clear(window, cx);
        let name = entry.as_ref().map(|e| e.name.as_str()).unwrap_or_default();
        let description = entry
            .as_ref()
            .map(|e| e.description.as_str())
            .unwrap_or_default();
        let website = entry
            .as_ref()
            .and_then(|e| e.website.as_deref())
            .unwrap_or_default();
        let env = entry.as_ref().map(|e| e.env.clone()).unwrap_or_default();
        let editor = Editor {
            name: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(name)
                    .placeholder("Stable slug, such as x_login")
            }),
            description: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(description)
                    .placeholder("Description, such as X, @username")
            }),
            website: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(website)
                    .placeholder("Website host, such as x.com")
            }),
            fields: cx.new(|cx| CredentialFields::new(&env, entry.is_some(), window, cx)),
            original: entry,
        };
        self.phase = Phase::Editing(editor);
        self.error = None;
        self.notice = None;
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Phase::Editing(editor) = &self.phase else {
            return;
        };
        let prepared = (|| {
            let name = editor.name.read(cx).value().to_string();
            if !valid_secret_name(&name) {
                return Err("Use a slug starting with a letter, with lowercase letters, digits, _ or -, up to 40 characters.".into());
            }
            if editor.original.is_none()
                && self
                    .entries
                    .as_ref()
                    .is_some_and(|entries| entries.iter().any(|e| e.name == name))
            {
                return Err(
                    "That slug already exists. Edit its entry to change saved fields.".into(),
                );
            }
            let description = editor.description.read(cx).value().to_string();
            let website = validate_secret_metadata(&description, &editor.website.read(cx).value())?;
            let env = editor.fields.read(cx).values(cx)?;
            let names = editor.fields.read(cx).names(cx);
            let remove = editor
                .original
                .as_ref()
                .map(|old| {
                    old.env
                        .iter()
                        .filter(|name| !names.contains(name))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            Ok::<_, String>((
                name,
                SaveSecret {
                    env,
                    description,
                    website,
                },
                remove,
                editor.original.is_some(),
            ))
        })();
        let (name, secret, remove, editing) = match prepared {
            Ok(value) => value,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let short: Vec<_> = secret
            .env
            .iter()
            .filter(|(_, value)| value.len() < 4)
            .map(|(name, _)| name.clone())
            .collect();
        let Phase::Editing(editor) = std::mem::replace(&mut self.phase, Phase::Browsing) else {
            unreachable!()
        };
        editor.fields.update(cx, |fields, cx| {
            fields.busy = true;
            cx.notify();
        });
        self.phase = Phase::Saving(editor);
        self.error = None;
        let client = self.client.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = if editing {
                client.patch_secret(name, PatchSecret {
                    description: secret.description,
                    website: secret.website,
                    set: secret.env,
                    remove,
                }).await
                    .map(|_| short)
            } else {
                client.put_secret(name, secret).await.map(|saved| saved.not_redacted)
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let Phase::Saving(editor) = std::mem::replace(&mut this.phase, Phase::Browsing) else {
                    return;
                };
                match result {
                    Ok(short) => {
                        editor.fields.update(cx, |fields, cx| fields.clear(window, cx));
                        this.notice = Some(if short.is_empty() {
                            "Saved in the Vault.".into()
                        } else {
                            format!("Saved. These fields have values shorter than 4 bytes and cannot be redacted from output: {}.", short.join(", "))
                        });
                        this.refresh(window, cx);
                    }
                    Err(error) if error.outcome_unknown() => {
                        editor.fields.update(cx, |fields, cx| fields.clear(window, cx));
                        this.phase = Phase::Unconfirmed("The server may have saved this entry. Refresh the Vault to check before making changes.".into());
                        this.error = Some(error.to_string());
                    }
                    Err(error) => {
                        editor.fields.update(cx, |fields, cx| {
                            fields.busy = false;
                            cx.notify();
                        });
                        this.phase = Phase::Editing(editor);
                        this.error = Some(error.to_string());
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn confirm(&mut self, name: String, reveal: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Browsing | Phase::Revealed { .. }) {
            return;
        }
        let this = cx.weak_entity();
        let cancel = cx.focus_handle();
        let once = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if once.replace(false) {
                cancel.focus(window, cx);
            }
            let this = this.clone();
            let name = name.clone();
            dialog.w(px(440.))
                .title(if reveal {
                    format!("Reveal {name}?")
                } else {
                    format!("Delete {name} for every bot?")
                })
                .close_button(false)
                .on_ok(|_, _, _| false)
                .child(div().text_sm().child(if reveal {
                    "The saved values will be visible on this screen until you hide them or leave the Vault."
                } else {
                    "Every bot will lose access to this entry. This cannot be undone."
                }))
                .footer(
                    h_flex().w_full().justify_end().gap_2()
                        .child(
                            Button::new("vault-confirm-cancel")
                                .control(Tier::Secondary, Scale::Standard, false, cx)
                                .label("Cancel")
                                .track_focus(&cancel)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("vault-confirm")
                                .control(if reveal { Tier::Primary } else { Tier::Danger }, Scale::Standard, false, cx)
                                .label(if reveal { "Reveal values" } else { "Delete for all bots" })
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = this.update(cx, |this, cx| this.perform(name.clone(), reveal, window, cx));
                                }),
                        ),
                )
        });
    }

    fn perform(&mut self, name: String, reveal: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Browsing | Phase::Revealed { .. }) {
            return;
        }
        self.clear(window, cx);
        self.phase = if reveal {
            Phase::Revealing(name.clone())
        } else {
            Phase::Deleting(name.clone())
        };
        self.error = None;
        let client = self.client.clone();
        self.task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = if reveal {
                client
                    .reveal_secret(name.clone())
                    .await
                    .map(|result| Some(result.env))
            } else {
                client.delete_secret(name.clone()).await.map(|()| None)
            };
            let _ = this.update_in(cx, |this, window, cx| {
                match &this.phase {
                    Phase::Revealing(active) | Phase::Deleting(active) if active == &name => {}
                    _ => return,
                }
                this.phase = Phase::Browsing;
                match result {
                    Ok(Some(env)) => this.phase = Phase::Revealed { name, env },
                    Ok(None) => {
                        this.notice = Some("Deleted from the Vault.".into());
                        this.refresh(window, cx);
                    }
                    Err(error) => {
                        if !reveal && error.outcome_unknown() {
                            this.phase = Phase::Unconfirmed("The server may have deleted this entry. Refresh the Vault to check before making changes.".into());
                        }
                        this.error = Some(error.to_string());
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let busy = matches!(
            self.phase,
            Phase::Saving(_) | Phase::Revealing(_) | Phase::Deleting(_)
        );
        let mut body = v_flex().gap_5().child(settings::section_heading(
            "Vault",
            "Save once. Every bot on this server can use these credentials.",
            cx,
        ));
        let blocked = busy || matches!(self.phase, Phase::Unconfirmed(_));
        if let Phase::Unconfirmed(message) = &self.phase {
            body = body.child(div().text_sm().child(message.clone()));
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        if let Some(notice) = &self.notice {
            body = body.child(div().text_sm().child(notice.clone()));
        }
        if let Phase::Editing(editor) | Phase::Saving(editor) = &self.phase {
            body = body.child(
                v_flex()
                    .gap_3()
                    .child(div().font_semibold().child(if editor.original.is_some() {
                        "Edit entry"
                    } else {
                        "Add entry"
                    }))
                    .child(
                        div()
                            .text_xs()
                            .child("Slug. Bots reference this name, so it cannot change."),
                    )
                    .child(
                        text_field(Input::new(&editor.name).aria_label("Stable Vault slug"), cx)
                            .disabled(busy || editor.original.is_some()),
                    )
                    .child(div().text_xs().child("Description"))
                    .child(
                        text_field(
                            Input::new(&editor.description).aria_label("Vault description"),
                            cx,
                        )
                        .disabled(busy),
                    )
                    .child(div().text_xs().child("Website host"))
                    .child(
                        text_field(Input::new(&editor.website).aria_label("Website host"), cx)
                            .disabled(busy),
                    )
                    .child(editor.fields.clone())
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("vault-edit-cancel")
                                    .control(Tier::Secondary, Scale::Standard, busy, cx)
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.clear(window, cx);
                                        this.error = None;
                                    })),
                            )
                            .child(
                                Button::new("vault-save")
                                    .control(Tier::Primary, Scale::Standard, busy, cx)
                                    .label(if busy { "Saving…" } else { "Save entry" })
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.save(window, cx)),
                                    ),
                            ),
                    ),
            );
            return body.into_any_element();
        }
        body = body.child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("vault-add")
                        .control(
                            Tier::Primary,
                            Scale::Standard,
                            blocked || self.entries.is_none(),
                            cx,
                        )
                        .label("Add entry")
                        .on_click(cx.listener(|this, _, window, cx| this.edit(None, window, cx))),
                )
                .child(
                    Button::new("vault-refresh")
                        .control(Tier::Secondary, Scale::Standard, busy, cx)
                        .label("Refresh")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.clear(window, cx);
                            this.refresh(window, cx);
                        })),
                ),
        );
        let Some(entries) = &self.entries else {
            return body
                .child(if self.error.is_some() {
                    "Could not load the Vault. Try Refresh."
                } else {
                    "Loading Vault…"
                })
                .into_any_element();
        };
        if entries.is_empty() {
            body = body.child("No saved entries. Add a login or save one when a bot asks.");
        }
        for entry in entries {
            let edit = entry.clone();
            let reveal = entry.name.clone();
            let delete = entry.name.clone();
            let mut row =
                v_flex()
                    .gap_2()
                    .py_4()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .font_semibold()
                            .child(if entry.description.is_empty() {
                                entry.name.clone()
                            } else {
                                entry.description.clone()
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "{}{}",
                                entry.name,
                                entry
                                    .website
                                    .as_ref()
                                    .map(|site| format!(" · {site}"))
                                    .unwrap_or_default()
                            )),
                    )
                    .child(div().text_xs().child(entry.env.join(", ")))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        vault_last_used(entry.last_used.as_ref(), chrono::Utc::now()),
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "Updated {}{}",
                                entry.updated_at.format("%b %-d, %Y"),
                                entry
                                    .last_used
                                    .as_ref()
                                    .map(|last| format!(" · Used in {}", last.r#use.label()))
                                    .unwrap_or_default()
                            )),
                    );
            let revealed =
                matches!(&self.phase, Phase::Revealed { name, .. } if name == &entry.name);
            if let Phase::Revealed { name, env } = &self.phase
                && name == &entry.name
            {
                for (var, value) in env {
                    row = row.child(
                        v_flex()
                            .gap_1()
                            .child(div().text_xs().child(var.clone()))
                            .child(div().text_sm().whitespace_normal().child(value.clone())),
                    );
                }
            }
            row = row.child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("vault-edit-{}", entry.name)))
                            .control(Tier::Secondary, Scale::Compact, blocked, cx)
                            .label("Edit")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit(Some(edit.clone()), window, cx)
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("vault-reveal-{}", entry.name)))
                            .control(Tier::Secondary, Scale::Compact, blocked, cx)
                            .label(if revealed { "Hide" } else { "Reveal" })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if revealed {
                                    this.clear(window, cx);
                                } else {
                                    this.confirm(reveal.clone(), true, window, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("vault-delete-{}", entry.name)))
                            .control(Tier::Danger, Scale::Compact, blocked, cx)
                            .label("Delete")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.confirm(delete.clone(), false, window, cx)
                            })),
                    ),
            );
            body = body.child(row);
        }
        body.into_any_element()
    }
}
