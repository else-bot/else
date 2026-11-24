//! Owner-only Vault UI. Inputs and revealed values are transient and cleared
//! on hide, navigation, server switching, and connection loss.
use super::*;
use crate::api::{
    PatchSecret, SaveSecret, valid_secret_name, validate_secret_metadata, vault_last_used,
};
use crate::controls::{Control as _, Scale, Tier, text_field};
use crate::credential_fields::{CredentialFields, Form};
use else_wire::secrets::VaultEntry;
use gpui_kit::assets::IconName;
use gpui_kit::base::SelectableText;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::{
    Disableable as _, FocusableExt as _, Icon, Sizable as _, StyledExt as _, Theme, h_flex,
};
use std::collections::BTreeMap;
use std::time::Duration;

use super::loaded::{Guard, Loaded};

pub(super) struct View {
    client: Arc<Client>,
    entries: Loaded<Vec<VaultEntry>>,
    search: Entity<InputState>,
    _search: Subscription,
    phase: Phase,
    error: Option<String>,
    notice: Option<String>,
    task: Option<Task<()>>,
    /// The list load, kept apart from `task` so starting an action never
    /// drops a refresh that is still in flight.
    load: Option<Task<()>>,
}

enum Phase {
    Closed,
    Browsing,
    Editing(Editor),
    Saving(Editor, Guard),
    Revealing(String),
    Revealed {
        name: String,
        env: BTreeMap<String, String>,
        /// The timer lives with its label, so another copy or a hide drops a
        /// stale reset.
        copied: Option<(String, Task<()>)>,
    },
    Deleting {
        name: String,
        _operation: Guard,
    },
    Unconfirmed(String),
}

impl Phase {
    /// The one lookup rows and Copy read, so a hidden entry exposes neither
    /// values nor a Copy action.
    fn revealed(&self, entry: &str) -> Option<(&BTreeMap<String, String>, Option<&str>)> {
        match self {
            Phase::Revealed { name, env, copied } if name == entry => {
                Some((env, copied.as_ref().map(|(field, _)| field.as_str())))
            }
            _ => None,
        }
    }

    fn copyable(&self, entry: &str, field: &str) -> Option<&str> {
        self.revealed(entry)?.0.get(field).map(String::as_str)
    }
}

struct Editor {
    original: Option<VaultEntry>,
    name: Entity<InputState>,
    description: Entity<InputState>,
    website: Entity<InputState>,
    fields: Entity<CredentialFields>,
}

impl View {
    pub(super) fn new(client: Arc<Client>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search by title, slug, website or field")
        });
        // A filter can hide the revealed row and its Hide button, so a new
        // query hides the values instead of keeping them for when it clears.
        let subscription = cx.subscribe_in(
            &search,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if !matches!(event, InputEvent::Change) {
                    return;
                }
                if matches!(this.phase, Phase::Revealing(_) | Phase::Revealed { .. }) {
                    this.clear(window, cx);
                }
                cx.notify();
            },
        );
        let mut this = Self {
            client,
            entries: Loaded::default(),
            search,
            _search: subscription,
            phase: Phase::Browsing,
            error: None,
            notice: None,
            task: None,
            load: None,
        };
        this.refresh(window, cx);
        this
    }

    pub(super) fn has_unsaved_work(&self) -> bool {
        matches!(
            self.phase,
            Phase::Editing(_) | Phase::Saving(..) | Phase::Deleting { .. }
        )
    }

    pub(super) fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.clear(window, cx);
        self.load = None;
        self.phase = Phase::Closed;
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.task = None;
        if let Phase::Editing(editor) | Phase::Saving(editor, _) = &self.phase {
            editor
                .fields
                .update(cx, |fields, cx| fields.clear(window, cx));
        }
        self.phase = Phase::Browsing;
        cx.notify();
    }

    fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(attempt) = self.entries.begin() else {
            return;
        };
        let client = self.client.clone();
        self.load = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.vault().await;
            let _ = this.update(cx, |this, cx| {
                let loaded = result.is_ok();
                if !this
                    .entries
                    .settle(attempt, result.map_err(|error| error.to_string()))
                {
                    return;
                }
                this.error = None;
                if loaded && matches!(this.phase, Phase::Unconfirmed(_)) {
                    this.phase = Phase::Browsing;
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
            fields: cx.new(|cx| CredentialFields::new(&env, Form::Vault, window, cx)),
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
                    .value()
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
                .map(|old| removed(&old.env, &names))
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
        let short = short_fields(&secret.env);
        let Phase::Editing(editor) = std::mem::replace(&mut self.phase, Phase::Browsing) else {
            unreachable!()
        };
        editor.fields.update(cx, |fields, cx| {
            fields.busy = true;
            cx.notify();
        });
        self.phase = Phase::Saving(editor, self.entries.operate());
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
                client.create_secret(name, secret).await.map(|saved| saved.not_redacted)
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let Phase::Saving(editor, operation) = std::mem::replace(&mut this.phase, Phase::Browsing) else {
                    return;
                };
                drop(operation);
                match settle(result) {
                    Settled::Saved(short) => {
                        editor.fields.update(cx, |fields, cx| fields.clear(window, cx));
                        this.notice = Some(if short.is_empty() {
                            "Saved in the Vault.".into()
                        } else {
                            format!("Saved. These fields have values shorter than 4 bytes and cannot be redacted from output: {}.", short.join(", "))
                        });
                        this.refresh(window, cx);
                    }
                    Settled::Unknown(error) => {
                        editor.fields.update(cx, |fields, cx| fields.clear(window, cx));
                        this.phase = Phase::Unconfirmed("The server may have saved this entry. Refresh the Vault to check before making changes.".into());
                        this.error = Some(error.to_string());
                    }
                    Settled::Rejected(error) => {
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
        let (title, body, confirm, tier) = if reveal {
            (
                format!("Reveal {name}?"),
                "The saved values will be visible on this screen until you hide them or leave the Vault.",
                "Reveal values",
                Tier::Primary,
            )
        } else {
            (
                format!("Delete {name} for every bot?"),
                "Every bot will lose access to this entry. This cannot be undone.",
                "Delete for all bots",
                Tier::Danger,
            )
        };
        crate::panel_kit::confirm_dialog(
            window,
            cx,
            title,
            body,
            confirm,
            tier,
            move |window, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.perform(name.clone(), reveal, window, cx)
                });
            },
        );
    }

    fn perform(&mut self, name: String, reveal: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Browsing | Phase::Revealed { .. }) {
            return;
        }
        self.clear(window, cx);
        self.phase = if reveal {
            Phase::Revealing(name.clone())
        } else {
            Phase::Deleting {
                name: name.clone(),
                _operation: self.entries.operate(),
            }
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
                    Phase::Revealing(active) | Phase::Deleting { name: active, .. } if active == &name => {}
                    _ => return,
                }
                this.phase = Phase::Browsing;
                match result {
                    Ok(Some(env)) => {
                        this.phase = Phase::Revealed {
                            name,
                            env,
                            copied: None,
                        }
                    }
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

    fn copy(&mut self, entry: &str, field: &str, cx: &mut Context<Self>) {
        let Some(value) = self.phase.copyable(entry, field) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(value.to_owned()));
        let reset = cx.spawn(async |this, cx| {
            cx.background_executor().timer(COPIED_LABEL).await;
            let _ = this.update(cx, |this, cx| {
                if let Phase::Revealed { copied, .. } = &mut this.phase {
                    *copied = None;
                    cx.notify();
                }
            });
        });
        if let Phase::Revealed { copied, .. } = &mut self.phase {
            *copied = Some((field.to_owned(), reset));
        }
        cx.notify();
    }
}

const COPIED_LABEL: Duration = Duration::from_secs(2);

impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let busy = matches!(
            self.phase,
            Phase::Saving(..) | Phase::Revealing(_) | Phase::Deleting { .. }
        );
        let blocked = busy || matches!(self.phase, Phase::Unconfirmed(_));
        let editing = matches!(self.phase, Phase::Editing(_) | Phase::Saving(..));
        let actions = vec![
            Button::new("vault-refresh")
                .control(Tier::Secondary, Scale::Standard, busy || editing, cx)
                .icon(IconName::RefreshCw)
                .label("Refresh")
                .on_click(cx.listener(|this, _, window, cx| {
                    if !matches!(this.phase, Phase::Unconfirmed(_)) {
                        this.clear(window, cx);
                    }
                    this.refresh(window, cx);
                }))
                .into_any_element(),
            Button::new("vault-add")
                .control(
                    Tier::Primary,
                    Scale::Standard,
                    blocked || editing || self.entries.value().is_none(),
                    cx,
                )
                .icon(IconName::Plus)
                .label("Add entry")
                .on_click(cx.listener(|this, _, window, cx| this.edit(None, window, cx)))
                .into_any_element(),
        ];
        let mut body = v_flex().gap_5().child(crate::panel_kit::page_heading(
            "Vault",
            "Save once. Every bot on this server can use these credentials.",
            actions,
            cx,
        ));
        if let Phase::Unconfirmed(message) = &self.phase {
            body = body.child(div().text_sm().child(message.clone()));
        }
        if let Some(error) = self.error.as_ref().or(self.entries.error()) {
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
        if let Phase::Editing(editor) | Phase::Saving(editor, _) = &self.phase {
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
        let Some(entries) = self.entries.value() else {
            return body
                .child(if self.entries.pending() {
                    "Loading Vault…"
                } else {
                    "Could not load the Vault. Try Refresh."
                })
                .into_any_element();
        };
        if entries.is_empty() {
            return body
                .child("No saved entries. Add a login or save one when a bot asks.")
                .into_any_element();
        }
        body = body.child(
            div().max_w(px(340.)).child(
                text_field(Input::new(&self.search), cx)
                    .aria_label("Search the Vault")
                    .small()
                    .prefix(Icon::new(IconName::Search).size_3()),
            ),
        );
        let query = self.search.read(cx).value().trim().to_lowercase();
        let filtered: Vec<_> = entries.iter().filter(|e| matches(e, &query)).collect();
        let mut list = v_flex().child(
            div()
                .pb_2()
                .border_b_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!("Saved · {}", filtered.len())),
        );
        if filtered.is_empty() {
            list = list.child(
                div()
                    .py_8()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No matching entries. Try a title, slug, website or field name."),
            );
        }
        for entry in filtered {
            list = list.child(self.row(entry, blocked, &theme, cx));
        }
        body.child(list).into_any_element()
    }
}

impl View {
    /// One entry on one line. Its actions appear on hover and stay shown
    /// while its values are revealed.
    fn row(&self, entry: &VaultEntry, blocked: bool, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let revealed = self.phase.revealed(&entry.name);
        let edit = entry.clone();
        let reveal = entry.name.clone();
        let delete = entry.name.clone();
        let action = |id: &str, icon: IconName, label: String| {
            Button::new(SharedString::from(format!("vault-{id}-{}", entry.name)))
                .focus_ring(false)
                .cursor_pointer()
                .ghost()
                .xsmall()
                .icon(icon)
                .accessibility_label(label)
                .disabled(blocked)
                .when(revealed.is_none(), |button| {
                    button
                        .opacity(0.)
                        .group_hover("vault-row", |style| style.opacity(1.))
                        .focus(|style| style.opacity(1.))
                })
        };
        let actions =
            h_flex()
                .flex_none()
                .gap_1()
                .child(
                    action("edit", IconName::PencilLine, format!("Edit {}", entry.name)).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.edit(Some(edit.clone()), window, cx)
                        }),
                    ),
                )
                .child(
                    action(
                        "reveal",
                        if revealed.is_some() {
                            IconName::EyeOff
                        } else {
                            IconName::Eye
                        },
                        format!(
                            "{} {}",
                            if revealed.is_some() { "Hide" } else { "Reveal" },
                            entry.name
                        ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if matches!(&this.phase, Phase::Revealed { name, .. } if name == &reveal) {
                            this.clear(window, cx);
                        } else {
                            this.confirm(reveal.clone(), true, window, cx);
                        }
                    })),
                )
                .child(div().w_px().h_4().mx_1().bg(theme.border).when(
                    revealed.is_none(),
                    |divider| {
                        divider
                            .opacity(0.)
                            .group_hover("vault-row", |style| style.opacity(1.))
                    },
                ))
                .child(
                    action("delete", IconName::Trash, format!("Delete {}", entry.name))
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm(delete.clone(), false, window, cx)
                        })),
                );
        let title = if entry.description.is_empty() {
            &entry.name
        } else {
            &entry.description
        };
        let detail = std::iter::once(entry.name.as_str())
            .chain(entry.website.as_deref())
            .collect::<Vec<_>>()
            .join(" · ");
        let fields = entry.env.join(", ");
        let mut row = v_flex().border_b_1().border_color(theme.border).child(
            h_flex()
                .group("vault-row")
                .gap_3()
                .py_1p5()
                .child(
                    div()
                        .min_w_0()
                        .max_w(px(260.))
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .truncate()
                        .child(title.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(if fields.is_empty() {
                            detail
                        } else {
                            format!("{fields} · {detail}")
                        }),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(vault_last_used(
                            entry.last_used.as_ref(),
                            chrono::Utc::now(),
                        )),
                )
                .child(actions),
        );
        if let Some((env, copied)) = revealed {
            row = row.child(
                div()
                    .pb_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "Updated {}{}",
                        entry.updated_at.format("%b %-d, %Y"),
                        entry
                            .last_used
                            .as_ref()
                            .map(|last| format!(" · Used in {}", last.r#use.as_str()))
                            .unwrap_or_default()
                    )),
            );
            for (var, value) in env {
                let done = copied == Some(var.as_str());
                let (name, field) = (entry.name.clone(), var.clone());
                row = row.child(
                    v_flex()
                        .gap_1()
                        .pb_2()
                        .child(div().text_xs().child(var.clone()))
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(div().flex_1().min_w_0().text_sm().child(
                                    SelectableText::new(
                                        SharedString::from(format!(
                                            "vault-value-{}-{var}",
                                            entry.name
                                        )),
                                        value.clone(),
                                    ),
                                ))
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "vault-copy-{}-{var}",
                                        entry.name
                                    )))
                                    .control(Tier::Ghost, Scale::Compact, false, cx)
                                    .flex_none()
                                    .icon(if done {
                                        IconName::Check
                                    } else {
                                        IconName::Copy
                                    })
                                    .label(if done { "Copied" } else { "Copy" })
                                    .accessibility_label(format!("Copy {var}"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.copy(&name, &field, cx)
                                    })),
                                ),
                        ),
                );
            }
        }
        row
    }
}

/// Whether an entry's title, slug, website or a field name contains `query`,
/// which the caller has trimmed and lowercased.
fn matches(entry: &VaultEntry, query: &str) -> bool {
    [&entry.description, &entry.name]
        .into_iter()
        .chain(entry.website.as_ref())
        .chain(&entry.env)
        .any(|text| text.to_lowercase().contains(query))
}

/// Values shorter than this are not redacted from output (the server's
/// `MIN_REDACTED`), so the owner is told which fields they are.
const MIN_REDACTED: usize = 4;

/// Fields an edit drops: in the stored entry but no longer in the form.
fn removed(old: &[String], kept: &[String]) -> Vec<String> {
    old.iter()
        .filter(|name| !kept.contains(name))
        .cloned()
        .collect()
}

fn short_fields(env: &BTreeMap<String, String>) -> Vec<String> {
    env.iter()
        .filter(|(_, value)| value.len() < MIN_REDACTED)
        .map(|(name, _)| name.clone())
        .collect()
}

/// How a save ended. `Unknown` keeps the owner from repeating a write the
/// server may have applied; `Rejected` keeps the editor and its values.
enum Settled<T> {
    Saved(T),
    Rejected(ClientError),
    Unknown(ClientError),
}

fn settle<T>(result: Result<T, ClientError>) -> Settled<T> {
    match result {
        Ok(value) => Settled::Saved(value),
        Err(error) if error.outcome_unknown() => Settled::Unknown(error),
        Err(error) => Settled::Rejected(error),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BTreeMap, ClientError, Phase, Settled, VaultEntry, matches, removed, settle, short_fields,
    };

    fn revealed() -> Phase {
        Phase::Revealed {
            name: "github_login".into(),
            env: BTreeMap::from([
                ("USERNAME".to_owned(), "octocat".to_owned()),
                ("PASSWORD".to_owned(), " hunter2 with spaces ".to_owned()),
            ]),
            copied: None,
        }
    }

    #[test]
    fn copy_takes_the_exact_value_of_each_revealed_field() {
        let phase = revealed();
        assert_eq!(
            phase.copyable("github_login", "PASSWORD"),
            Some(" hunter2 with spaces ")
        );
        assert_eq!(phase.copyable("github_login", "USERNAME"), Some("octocat"));
        assert_eq!(phase.copyable("github_login", "TOTP"), None);
    }

    #[test]
    fn hidden_entries_show_no_values_and_copy_nothing() {
        for phase in [
            Phase::Browsing,
            Phase::Revealing("github_login".into()),
            Phase::Unconfirmed("github_login".into()),
        ] {
            assert!(phase.revealed("github_login").is_none());
            assert_eq!(phase.copyable("github_login", "PASSWORD"), None);
        }
        let phase = revealed();
        assert!(phase.revealed("other_login").is_none());
        assert_eq!(phase.copyable("other_login", "PASSWORD"), None);
    }

    fn api(status: u16, code: &str) -> ClientError {
        ClientError::Api {
            status,
            code: code.into(),
            message: String::new(),
            retryable: false,
        }
    }

    #[test]
    fn a_save_settles_as_saved_rejected_or_unknown() {
        assert!(matches!(settle(Ok::<_, ClientError>(1)), Settled::Saved(1)));
        for rejected in [
            api(400, "invalid_secret"),
            api(409, "secret_exists"),
            api(503, "secrets_unavailable"),
        ] {
            assert!(matches!(settle::<()>(Err(rejected)), Settled::Rejected(_)));
        }
        for unknown in [
            api(502, "bad_gateway"),
            ClientError::Unreachable {
                origin: "http://x".into(),
                cause: "reset".into(),
            },
        ] {
            assert!(matches!(settle::<()>(Err(unknown)), Settled::Unknown(_)));
        }
    }

    #[test]
    fn an_edit_removes_only_the_fields_dropped_from_the_form() {
        let old = [
            "PASSWORD".to_owned(),
            "USERNAME".to_owned(),
            "TOTP".to_owned(),
        ];
        let kept = ["USERNAME".to_owned(), "API_KEY".to_owned()];
        assert_eq!(removed(&old, &kept), ["PASSWORD", "TOTP"]);
        assert!(removed(&old, &old).is_empty());
    }

    #[test]
    fn values_under_four_bytes_are_flagged_as_unredacted() {
        let env = BTreeMap::from([
            ("PIN".to_owned(), "123".to_owned()),
            ("PASSWORD".to_owned(), "long-enough".to_owned()),
        ]);
        assert_eq!(short_fields(&env), ["PIN"]);
    }

    #[test]
    fn search_matches_title_slug_website_and_field_names() {
        let entry = VaultEntry {
            name: "x_login".into(),
            description: "X, @owner".into(),
            website: Some("x.com".into()),
            env: vec!["PASSWORD".into(), "TOTP_SECRET".into()],
            updated_at: chrono::Utc::now(),
            last_used: None,
        };
        for query in ["", "@owner", "x_log", "x.com", "totp"] {
            assert!(matches(&entry, query), "{query}");
        }
        assert!(!matches(&entry, "github"));
    }
}
