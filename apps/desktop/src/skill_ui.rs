use std::collections::BTreeMap;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Icon, Sizable as _, StyledExt as _,
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::model::{SkillFile, SkillSource};

pub(crate) struct SkillKeys {
    fields: Vec<KeyField>,
    pub busy: bool,
}

struct KeyField {
    var: String,
    input: Entity<InputState>,
    captured: Option<String>,
}

impl SkillKeys {
    pub fn new(env: &[String], window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            fields: env
                .iter()
                .map(|var| KeyField {
                    var: var.clone(),
                    input: cx.new(|cx| {
                        InputState::new(window, cx)
                            .masked(true)
                            .placeholder(format!("Paste or type {var}"))
                    }),
                    captured: None,
                })
                .collect(),
            busy: false,
        }
    }

    pub fn values(
        &self,
        optional: bool,
        cx: &App,
    ) -> Result<Option<BTreeMap<String, String>>, String> {
        let env: BTreeMap<_, _> = self
            .fields
            .iter()
            .map(|field| {
                (
                    field.var.clone(),
                    field
                        .captured
                        .clone()
                        .unwrap_or_else(|| field.input.read(cx).value().to_string()),
                )
            })
            .collect();
        if optional && env.values().all(|value| value.is_empty()) {
            return Ok(None);
        }
        if env.values().any(|value| value.is_empty()) {
            return Err("Fill in every key, or leave them all empty to add them later.".into());
        }
        Ok(Some(env))
    }

    /// Whether every field is still empty, so nothing typed would be lost.
    pub fn is_empty(&self, cx: &App) -> bool {
        self.fields
            .iter()
            .all(|field| field.captured.is_none() && field.input.read(cx).value().is_empty())
    }

    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for field in &mut self.fields {
            field.captured = None;
            field
                .input
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }
}

impl Render for SkillKeys {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut fields = v_flex().gap_2();
        for (ix, field) in self.fields.iter().enumerate() {
            let control = if let Some(text) = &field.captured {
                h_flex()
                    .gap_2()
                    .child(format!(
                        "Pasted {} lines ({} bytes), hidden.",
                        text.lines().count(),
                        text.len()
                    ))
                    .child(
                        Button::new(("skill-key-replace", ix))
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Replace")
                            .xsmall()
                            .ghost()
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.fields[ix].captured = None;
                                this.fields[ix]
                                    .input
                                    .update(cx, |input, cx| input.focus(window, cx));
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            } else {
                let this = cx.weak_entity();
                Input::new(&field.input)
                    .focus_ring(false)
                    .aria_label(field.var.clone())
                    .disabled(self.busy)
                    .on_paste(move |item, window, cx| {
                        let Some(text) = item.text().filter(|text| {
                            text.trim_end_matches(['\r', '\n']).contains(['\r', '\n'])
                        }) else {
                            return false;
                        };
                        let _ = this.update(cx, |this, cx| {
                            this.fields[ix]
                                .input
                                .update(cx, |input, cx| input.set_value("", window, cx));
                            this.fields[ix].captured = Some(text);
                            cx.notify();
                        });
                        true
                    })
                    .into_any_element()
            };
            fields = fields.child(
                v_flex()
                    .gap_1()
                    .child(div().text_xs().font_semibold().child(field.var.clone()))
                    .child(control),
            );
        }
        fields
    }
}

#[derive(Default)]
pub(crate) struct SkillFiles {
    pub selected: Option<String>,
    pub contents: Option<BTreeMap<String, String>>,
    pub loading: bool,
    pub error: Option<String>,
}

impl SkillFiles {
    pub fn render(
        &self,
        id: &str,
        files: &[SkillFile],
        select: impl Fn(String, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme();
        let select = std::rc::Rc::new(select);
        let mut list = v_flex().w_full();
        let mut paths: Vec<_> = files.iter().collect();
        paths.sort_by_key(|file| &file.path);
        for file in paths {
            let path = file.path.clone();
            let pick = select.clone();
            let selected = self.selected.as_ref() == Some(&file.path);
            list = list.child(
                h_flex()
                    .id(SharedString::from(format!("{id}-{}", file.path)))
                    .w_full()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |row| row.bg(theme.secondary))
                    .hover(|row| row.bg(theme.secondary.opacity(0.6)))
                    .child(
                        Icon::new(if file.script {
                            IconName::Terminal
                        } else {
                            IconName::FileText
                        })
                        .size_3()
                        .text_color(theme.muted_foreground),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(if selected {
                                theme.foreground
                            } else {
                                theme.muted_foreground
                            })
                            .child(file.path.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(crate::attachments::byte_size(file.size)),
                    )
                    .on_click(move |_, window, cx| pick(path.clone(), window, cx)),
            );
        }
        let text = self.error.clone().unwrap_or_else(|| {
            if self.loading {
                return "Loading file…".into();
            }
            self.selected.as_ref().map_or_else(
                || "Choose a file to read it.".into(),
                |path| {
                    self.contents
                        .as_ref()
                        .and_then(|contents| contents.get(path))
                        .cloned()
                        .unwrap_or_else(|| "File contents are unavailable.".into())
                },
            )
        });
        v_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .child(
                div()
                    .id(SharedString::from(format!("{id}-tree")))
                    .w_full()
                    .max_h(px(168.))
                    .p_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .overflow_y_scrollbar()
                    .child(list),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "{} · Read-only",
                        self.selected.as_deref().unwrap_or("Files")
                    )),
            )
            .child(
                div()
                    .id(SharedString::from(format!("{id}-contents")))
                    .w_full()
                    .h(px(260.))
                    .p_3()
                    .rounded_md()
                    .bg(theme.background)
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .child(text),
                    ),
            )
            .into_any_element()
    }
}

pub(crate) fn source(source: &SkillSource, cx: &App) -> AnyElement {
    let url = source.url.clone();
    Button::new(SharedString::from(format!("skill-source-{}", source.url)))
        .focus_ring(false)
        .cursor_pointer()
        .ghost()
        .xsmall()
        .label(source.label())
        .text_color(cx.theme().primary)
        .on_click(move |_, _, cx| cx.open_url(&url))
        .into_any_element()
}

pub(crate) fn chip(text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .flex_none()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(color.opacity(0.12))
        .text_xs()
        .text_color(color)
        .child(text.into())
}
