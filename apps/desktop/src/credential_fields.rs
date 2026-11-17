//! Ephemeral masked fields shared by the Vault editor and login cards.
//! Values are never drafts. Owners clear them before discarding the form.
use crate::api::valid_secret_var;
use crate::controls::{Control as _, Scale, Tier, text_field};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct CredentialFields {
    fields: Vec<Field>,
    next_id: usize,
    pub busy: bool,
}

struct Field {
    id: usize,
    name: Entity<InputState>,
    value: Entity<InputState>,
    captured: Option<String>,
    kind: FieldKind,
}

#[derive(Clone, Copy)]
enum FieldKind {
    Required,
    Stored,
    Added,
}

pub(crate) struct FieldsChanged;
impl EventEmitter<FieldsChanged> for CredentialFields {}

impl CredentialFields {
    pub fn new(
        names: &[String],
        stored: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            fields: Vec::new(),
            next_id: 0,
            busy: false,
        };
        for name in names {
            this.push(
                name,
                if stored {
                    FieldKind::Stored
                } else {
                    FieldKind::Required
                },
                window,
                cx,
            );
        }
        if names.is_empty() {
            this.push("", FieldKind::Added, window, cx);
        }
        this
    }

    fn push(&mut self, name: &str, kind: FieldKind, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_id;
        self.next_id += 1;
        self.fields.push(Field {
            id,
            name: cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(name)
                    .placeholder("FIELD_NAME")
            }),
            value: cx.new(|cx| {
                InputState::new(window, cx).masked(true).placeholder(
                    if matches!(kind, FieldKind::Stored) {
                        "Leave blank to keep saved value"
                    } else {
                        "Paste or type the value"
                    },
                )
            }),
            captured: None,
            kind,
        });
    }

    pub fn values(&self, cx: &App) -> Result<BTreeMap<String, String>, String> {
        collect_fields(self.fields.iter().map(|field| {
            (
                field.name.read(cx).value().to_string(),
                field
                    .captured
                    .clone()
                    .unwrap_or_else(|| field.value.read(cx).value().to_string()),
                matches!(field.kind, FieldKind::Stored),
            )
        }))
    }

    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for field in &mut self.fields {
            field.captured = None;
            field
                .value
                .update(cx, |state, cx| state.set_value("", window, cx));
        }
        cx.notify();
    }
}

impl Render for CredentialFields {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut rows = v_flex().gap_3();
        for field in &self.fields {
            let id = field.id;
            let name = field.name.read(cx).value().to_string();
            let label = if matches!(field.kind, FieldKind::Added) {
                text_field(Input::new(&field.name).aria_label("Field name"), cx)
                    .disabled(self.busy)
                    .into_any_element()
            } else {
                div().text_xs().child(name.clone()).into_any_element()
            };
            let control = if let Some(text) = &field.captured {
                h_flex()
                    .gap_2()
                    .child(format!(
                        "Pasted {} lines ({} bytes), hidden.",
                        text.lines().count(),
                        text.len()
                    ))
                    .child(
                        Button::new(("credential-replace", id))
                            .control(Tier::Ghost, Scale::Compact, self.busy, cx)
                            .label("Replace")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let field =
                                    this.fields.iter_mut().find(|field| field.id == id).unwrap();
                                field.captured = None;
                                field.value.update(cx, |state, cx| state.focus(window, cx));
                                cx.emit(FieldsChanged);
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            } else {
                let this = cx.weak_entity();
                text_field(
                    Input::new(&field.value).aria_label(format!("{name} value")),
                    cx,
                )
                .disabled(self.busy)
                .on_paste(move |item, window, cx| {
                    let Some(text) = item.text().filter(|t| is_multiline(t)) else {
                        return false;
                    };
                    let _ = this.update(cx, |this, cx| {
                        if let Some(field) = this.fields.iter_mut().find(|field| field.id == id) {
                            field
                                .value
                                .update(cx, |state, cx| state.set_value("", window, cx));
                            field.captured = Some(text);
                            cx.emit(FieldsChanged);
                            cx.notify();
                        }
                    });
                    true
                })
                .into_any_element()
            };
            rows = rows.child(
                v_flex()
                    .gap_1p5()
                    .child(h_flex().gap_2().child(div().flex_1().child(label)).when(
                        !matches!(field.kind, FieldKind::Required),
                        |row| {
                            row.child(
                                Button::new(("credential-remove", id))
                                    .control(Tier::Ghost, Scale::Compact, self.busy, cx)
                                    .label("Remove field")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if let Some(ix) =
                                            this.fields.iter().position(|field| field.id == id)
                                        {
                                            let field = this.fields.remove(ix);
                                            field.value.update(cx, |state, cx| {
                                                state.set_value("", window, cx)
                                            });
                                            cx.emit(FieldsChanged);
                                            cx.notify();
                                        }
                                    })),
                            )
                        },
                    ))
                    .child(control),
            );
        }
        rows.child(
            Button::new("credential-add")
                .control(
                    Tier::Secondary,
                    Scale::Compact,
                    self.busy || self.fields.len() >= 16,
                    cx,
                )
                .label("Add field")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.push("", FieldKind::Added, window, cx);
                    cx.emit(FieldsChanged);
                    cx.notify();
                })),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(
                    "Values shorter than 4 bytes cannot be redacted from command or job output.",
                ),
        )
    }
}

fn collect_fields(
    fields: impl Iterator<Item = (String, String, bool)>,
) -> Result<BTreeMap<String, String>, String> {
    let mut seen = BTreeSet::new();
    let mut env = BTreeMap::new();
    for (name, value, stored) in fields {
        if !valid_secret_var(&name) {
            return Err("Use field names like PASSWORD or TOTP_SECRET. System variables such as PATH, HOME, LD_* and ELSE_* are reserved.".into());
        }
        if !seen.insert(name.clone()) {
            return Err(format!("{name} appears more than once."));
        }
        if seen.len() > 16 {
            return Err("Keep between 1 and 16 fields.".into());
        }
        if value.is_empty() {
            if stored {
                continue;
            }
            return Err(format!("Fill in {name} first."));
        }
        if value.len() > 4096 {
            return Err(format!("Keep {name} within 4096 bytes."));
        }
        env.insert(name, value);
    }
    if seen.is_empty() {
        return Err("Keep between 1 and 16 fields.".into());
    }
    Ok(env)
}

pub(crate) fn is_multiline(text: &str) -> bool {
    text.trim_end_matches(['\r', '\n']).contains(['\r', '\n'])
}

#[cfg(test)]
mod tests {
    use super::{collect_fields, is_multiline};

    fn fields(
        rows: &[(&str, &str, bool)],
    ) -> Result<std::collections::BTreeMap<String, String>, String> {
        collect_fields(
            rows.iter()
                .map(|(name, value, stored)| (name.to_string(), value.to_string(), *stored)),
        )
    }

    #[test]
    fn vault_fields_validate_required_added_and_retained_values() {
        assert!(fields(&[]).is_err());
        assert!(fields(&[("PASSWORD", "", false)]).is_err());
        assert!(fields(&[("PASSWORD", "a", false), ("PASSWORD", "b", false)]).is_err());
        assert!(fields(&[("PATH", "a", false)]).is_err());
        assert!(fields(&[("NOTES", &"a".repeat(4097), false)]).is_err());
        let values = fields(&[("PASSWORD", "", true), ("TOTP_SECRET", "seed", false)]).unwrap();
        assert!(!values.contains_key("PASSWORD"));
        assert_eq!(values["TOTP_SECRET"], "seed");
        assert_eq!(
            fields(&[("NOTES", "a\nb", false)]).unwrap()["NOTES"],
            "a\nb"
        );
        assert_eq!(
            fields(&[("PASSWORD", "new", true)]).unwrap()["PASSWORD"],
            "new"
        );
        let too_many: Vec<_> = (0..17)
            .map(|ix| (format!("KEY_{ix}"), "value".to_string(), false))
            .collect();
        assert!(collect_fields(too_many.into_iter()).is_err());
    }
    #[test]
    fn vault_fields_preserve_inner_line_breaks() {
        assert!(is_multiline("private\nkey\n"));
        assert!(is_multiline("a\r\nb"));
        assert!(!is_multiline("token\n"));
        assert!(!is_multiline("plain"));
    }
}
