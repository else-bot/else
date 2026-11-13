use super::*;
use crate::api::InstalledMcp;
use crate::controls::{Control as _, Scale, Tier, text_field};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::{Disableable as _, FocusableExt as _, Icon, Sizable as _, h_flex};
use std::time::Duration;
use uuid::Uuid;

/// Favicons by MCP origin: `None` while loading or when the service has
/// none, so each origin is fetched once per visit.
type Icons = HashMap<String, Option<Arc<Image>>>;

pub(super) struct View {
    id: Uuid,
    client: Arc<Client>,
    entries: Option<Vec<InstalledMcp>>,
    icons: Icons,
    _icon_loads: Vec<Task<()>>,
    search: Entity<InputState>,
    revision: u64,
    removing: Option<String>,
    error: Option<String>,
    poll_error: Option<String>,
    _search: Subscription,
    _poll: Task<()>,
    _remove: Option<Task<()>>,
}

impl View {
    pub(super) fn has_unsaved_work(&self) -> bool {
        self.removing.is_some()
    }
}

impl Shell {
    pub(super) fn open_mcp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = &self.active else { return };
        let client = active.client.clone();
        let poll_client = client.clone();
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search installed MCPs"));
        let subscription = cx.subscribe_in(&search, window, |_, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let id = Uuid::new_v4();
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                let revision = this.update_in(cx, |shell, _, _| {
                    shell
                        .mcp
                        .as_ref()
                        .filter(|view| view.id == id)
                        .map(|view| view.revision)
                });
                let Ok(Some(revision)) = revision else { break };
                let result = poll_client.clone().installed_mcps().await;
                let active = this.update_in(cx, |shell, _, cx| {
                    let Some(view) = shell.mcp.as_mut().filter(|view| view.id == id) else {
                        return false;
                    };
                    if view.revision == revision && view.removing.is_none() {
                        match result {
                            Ok(entries) => {
                                view.entries = Some(entries);
                                view.poll_error = None;
                                Self::load_mcp_icons(view, cx);
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
        self.mcp = Some(View {
            id,
            client,
            entries: None,
            icons: HashMap::new(),
            _icon_loads: Vec::new(),
            search,
            revision: 0,
            removing: None,
            error: None,
            poll_error: None,
            _search: subscription,
            _poll: poll,
            _remove: None,
        });
        cx.notify();
    }

    fn load_mcp_icons(view: &mut View, cx: &mut Context<Self>) {
        let id = view.id;
        let websites: Vec<String> = view
            .entries
            .iter()
            .flatten()
            .filter_map(|entry| entry.website.clone())
            .filter(|website| !view.icons.contains_key(website))
            .collect();
        for website in websites {
            view.icons.insert(website.clone(), None);
            let client = view.client.clone();
            view._icon_loads.push(cx.spawn(async move |this, cx| {
                let Some(png) = client.service_icon(website.clone()).await else {
                    return;
                };
                let _ = this.update(cx, |shell, cx| {
                    if let Some(view) = shell.mcp.as_mut().filter(|view| view.id == id) {
                        view.icons.insert(
                            website,
                            Some(Arc::new(Image::from_bytes(ImageFormat::Png, png))),
                        );
                        cx.notify();
                    }
                });
            }));
        }
    }

    fn confirm_remove_mcp(
        &mut self,
        entry: InstalledMcp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.mcp.as_ref().filter(|view| view.removing.is_none()) else {
            return;
        };
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
                                    let _ = shell.update(cx, |shell, cx| {
                                        shell.remove_mcp(view_id, id.clone(), window, cx);
                                    });
                                }),
                        ),
                )
        });
    }

    fn remove_mcp(
        &mut self,
        view_id: Uuid,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self
            .mcp
            .as_mut()
            .filter(|view| view.id == view_id && view.removing.is_none())
        else {
            return;
        };
        view.removing = Some(id.clone());
        view.revision += 1;
        view.error = None;
        let client = view.client.clone();
        view._remove = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.remove_mcp(id.clone()).await;
            let _ = this.update_in(cx, |shell, _, cx| {
                let Some(view) = shell.mcp.as_mut().filter(|view| view.id == view_id) else {
                    return;
                };
                view.revision += 1;
                view.removing = None;
                match result {
                    Ok(()) => {
                        if let Some(entries) = &mut view.entries {
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

    pub(super) fn render_mcp(&self, _window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mut body = v_flex().gap_5().child(settings::section_heading(
            "MCP",
            "Installed once. Available to every bot.",
            cx,
        ));
        let Some(view) = &self.mcp else {
            return body.child("Loading installed MCPs…").into_any_element();
        };
        let error = view.error.as_ref().or(view.poll_error.as_ref());
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
                                view.removing.is_some(),
                                cx,
                            )
                            .label("Retry")
                            .on_click(
                                cx.listener(|shell, _, window, cx| shell.open_mcp(window, cx)),
                            ),
                    ),
            );
        }
        let Some(entries) = &view.entries else {
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
                        .on_click(cx.listener(|shell, _, window, cx| {
                            window.close_dialog(cx);
                            shell.cancel(window, cx);
                        })),
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
                text_field(Input::new(&view.search), cx)
                    .small()
                    .prefix(Icon::new(IconName::Search).size_3()),
            ),
        );
        let query = view.search.read(cx).value().trim().to_lowercase();
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
                .and_then(|website| view.icons.get(website).cloned().flatten());
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
                                .disabled(view.removing.is_some())
                                .on_click(cx.listener(move |shell, _, window, cx| {
                                    shell.confirm_remove_mcp(removal_entry.clone(), window, cx);
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
