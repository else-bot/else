use super::*;
use crate::controls::{Control as _, Scale, Tier, text_field};
use else_wire::mcp::InstalledMcp;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::{Disableable as _, FocusableExt as _, Icon, Sizable as _, h_flex};
use std::time::Duration;

use super::loaded::Loaded;
use super::pane::PaneEvent;

/// Favicons by MCP origin: `None` while loading or when the service has
/// none, so each origin is fetched once per visit.
type Icons = HashMap<String, Option<Arc<Image>>>;

pub(super) struct View {
    client: Arc<Client>,
    entries: Loaded<Vec<InstalledMcp>>,
    icons: Icons,
    _icon_loads: Vec<Task<()>>,
    search: Entity<InputState>,
    removing: Option<String>,
    error: Option<String>,
    _search: Subscription,
    _poll: Task<()>,
    _remove: Option<Task<()>>,
}

impl EventEmitter<PaneEvent> for View {}

impl View {
    pub(super) fn new(client: Arc<Client>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let poll_client = client.clone();
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search installed MCPs"));
        let subscription = cx.subscribe_in(&search, window, |_, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let poll = cx.spawn_in(window, async move |this, cx| {
            // A removal outdates the list, so no fetch starts while one runs.
            while let Ok(attempt) = this.update(cx, |view, _| {
                view.removing.is_none().then(|| view.entries.begin())
            }) {
                if let Some(attempt) = attempt {
                    let result = poll_client.clone().installed_mcps().await;
                    let open = this.update(cx, |view, cx| {
                        if view
                            .entries
                            .settle(attempt, result.map_err(|error| error.to_string()))
                        {
                            view.load_icons(cx);
                            cx.notify();
                        }
                    });
                    if open.is_err() {
                        break;
                    }
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
            }
        });
        Self {
            client,
            entries: Loaded::default(),
            icons: HashMap::new(),
            _icon_loads: Vec::new(),
            search,
            removing: None,
            error: None,
            _search: subscription,
            _poll: poll,
            _remove: None,
        }
    }

    pub(super) fn has_unsaved_work(&self) -> bool {
        self.removing.is_some()
    }

    fn load_icons(&mut self, cx: &mut Context<Self>) {
        let websites: Vec<String> = self
            .entries
            .value()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.website.clone())
            .filter(|website| !self.icons.contains_key(website))
            .collect();
        for website in websites {
            self.icons.insert(website.clone(), None);
            let client = self.client.clone();
            self._icon_loads.push(cx.spawn(async move |this, cx| {
                let Some(png) = client.service_icon(website.clone()).await else {
                    return;
                };
                let _ = this.update(cx, |view, cx| {
                    view.icons.insert(
                        website,
                        Some(Arc::new(Image::from_bytes(ImageFormat::Png, png))),
                    );
                    cx.notify();
                });
            }));
        }
    }

    fn confirm_remove(&mut self, entry: InstalledMcp, window: &mut Window, cx: &mut Context<Self>) {
        if self.removing.is_some() {
            return;
        }
        let view = cx.weak_entity();
        let cancel = cx.focus_handle();
        let focus_once = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_once.replace(false) {
                cancel.focus(window, cx);
            }
            let view = view.clone();
            let id = entry.id.clone();
            dialog
                .w(px(440.))
                .title(format!("Remove {} for every bot?", entry.name))
                .close_button(false)
                .on_ok(|_, _, _| false)
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("This removes its shared skill, documentation and config. Every bot will lose access to this MCP through the shared library."),
                )
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("mcp-remove-cancel")
                                .control(Tier::Secondary, Scale::Standard, false, cx)
                                .label("Cancel")
                                .track_focus(&cancel)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("mcp-remove-confirm")
                                .control(Tier::Danger, Scale::Standard, false, cx)
                                .label("Remove for all bots")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = view.update(cx, |view, cx| {
                                        view.remove(id.clone(), window, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    fn remove(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.removing.is_some() {
            return;
        }
        self.removing = Some(id.clone());
        self.entries.supersede();
        self.error = None;
        let client = self.client.clone();
        self._remove = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.remove_mcp(id.clone()).await;
            let _ = this.update(cx, |view, cx| {
                view.removing = None;
                match result {
                    Ok(()) => {
                        if let Some(entries) = view.entries.value_mut() {
                            entries.retain(|entry| entry.id != id);
                        }
                    }
                    Err(error) => view.error = Some(error.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn render_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mut body = v_flex().gap_5().child(settings::section_heading(
            "MCP",
            "Installed once. Available to every bot.",
            cx,
        ));
        let error = self.error.as_ref().or(self.entries.error());
        if let Some(error) = error {
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
                        Button::new("mcp-retry")
                            .control(
                                Tier::Secondary,
                                Scale::Standard,
                                self.removing.is_some(),
                                cx,
                            )
                            .label("Retry")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(PaneEvent::Reload))),
                    ),
            );
        }
        let Some(entries) = self.entries.value() else {
            if error.is_none() {
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Loading installed MCPs…"),
                );
            }
            return body.into_any_element();
        };
        if entries.is_empty() {
            return body
                .flex_1()
                .child(settings::empty_state(
                    IconName::Plug,
                    "No MCPs installed yet",
                    "Ask a bot to install an MCP.",
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
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Skills, documentation and config are shared by all bots. A signed-in OAuth account serves every bot."),
        );
        body = body.child(
            div().max_w(px(340.)).child(
                text_field(Input::new(&self.search), cx)
                    .small()
                    .prefix(Icon::new(IconName::Search).size_3()),
            ),
        );
        let query = self.search.read(cx).value().trim().to_lowercase();
        let filtered = entries
            .iter()
            .filter(|entry| {
                format!("{} {}", entry.name, entry.description)
                    .to_lowercase()
                    .contains(&query)
            })
            .collect::<Vec<_>>();
        let mut list = v_flex().child(
            h_flex()
                .justify_between()
                .pb_2()
                .border_b_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!("Installed · {}", filtered.len()))
                .child(div().pr(px(50.)).child("Status")),
        );
        if filtered.is_empty() {
            list = list.child(
                v_flex()
                    .gap_2()
                    .py_8()
                    .child(div().text_sm().child(if entries.is_empty() {
                        "No MCPs installed yet"
                    } else {
                        "No matching MCPs"
                    }))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        if entries.is_empty() {
                            "Ask any bot to install an MCP. It will be available to every bot."
                        } else {
                            "Try a different name or capability."
                        },
                    )),
            );
        }
        for entry in filtered {
            let removal_entry = entry.clone();
            let initials = entry.name.chars().take(2).collect::<String>();
            let icon = entry
                .website
                .as_ref()
                .and_then(|website| self.icons.get(website).cloned().flatten());
            let installed = chrono::DateTime::parse_from_rfc3339(&entry.installed_at)
                .map(|at| format!(" · {}", at.format("%b %-d")))
                .unwrap_or_default();
            list =
                list.child(
                    h_flex()
                        .gap_3()
                        .py_4()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex_none()
                                .size_9()
                                .rounded_md()
                                .bg(theme.secondary)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(match icon {
                                    Some(icon) => img(icon)
                                        .size_5()
                                        .object_fit(ObjectFit::Contain)
                                        .into_any_element(),
                                    None => initials.into_any_element(),
                                }),
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
                                        .child(entry.description.clone()),
                                )
                                .child(div().text_xs().text_color(theme.muted_foreground).child(
                                    format!("Installed by {}{installed}", entry.installed_by),
                                )),
                        )
                        .child(
                            div()
                                .flex_none()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(theme.success.opacity(0.12))
                                .text_xs()
                                .text_color(theme.success)
                                .child("Installed"),
                        )
                        .child(
                            Button::new(SharedString::from(format!("mcp-remove-{}", entry.id)))
                                .focus_ring(false)
                                .cursor_pointer()
                                .ghost()
                                .small()
                                .icon(IconName::Trash)
                                .text_color(theme.danger)
                                .accessibility_label(format!("Remove {} for all bots", entry.name))
                                .disabled(self.removing.is_some())
                                .on_click(cx.listener(move |view, _, window, cx| {
                                    view.confirm_remove(removal_entry.clone(), window, cx);
                                })),
                        ),
                );
        }
        body.child(list)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Removing an MCP removes its shared package for every bot."),
            )
            .into_any_element()
    }
}

impl Render for View {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_page(cx)
    }
}
