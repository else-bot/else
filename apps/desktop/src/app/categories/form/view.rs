use super::*;
use crate::api::Bot;
use crate::avatar::bot_avatar;
use gpui_kit::component::Selectable as _;
use gpui_kit::component::checkbox::Checkbox;

fn category_name<'a>(bot: &Bot, categories: &'a [Category]) -> Option<&'a str> {
    categories
        .iter()
        .find(|category| Some(category.id) == bot.category_id)
        .map(|category| category.name.as_str())
}

impl CategoryFormHolder {
    fn visible_bots<'a>(&self, state: &'a ElseApp, cx: &App) -> Vec<&'a Bot> {
        let query = self.search.read(cx).value().trim().to_lowercase();
        state
            .active_bots()
            .filter(|bot| {
                let category = category_name(bot, &state.categories);
                (self.filter == BotFilter::All || category.is_none())
                    && (bot.name.to_lowercase().contains(&query)
                        || category
                            .unwrap_or("Unassigned")
                            .to_lowercase()
                            .contains(&query))
            })
            .collect()
    }

    pub(super) fn render_create(
        &self,
        holder: &Entity<Self>,
        state: &ElseApp,
        stacked: bool,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme();
        let selected: Vec<_> = state
            .active_bots()
            .filter(|bot| self.selected.contains(&bot.id))
            .collect();
        let name = self.name.read(cx).value();
        let mut preview = v_flex().gap_2().child(
            h_flex()
                .gap_2()
                .text_xs()
                .child(
                    Icon::new(IconName::ChevronDown)
                        .size_3()
                        .text_color(theme.muted_foreground),
                )
                .child(Icon::new(IconName::Folder).size_3p5())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::MEDIUM)
                        .child(if name.trim().is_empty() {
                            "New category".to_owned()
                        } else {
                            name.trim().to_owned()
                        }),
                )
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(selected.len().to_string()),
                ),
        );
        let mut preview_bots = v_flex()
            .id("category-preview-bots")
            .max_h(px(210.))
            .overflow_y_scroll()
            .pl_3()
            .gap_2();
        for bot in &selected {
            preview_bots = preview_bots.child(
                h_flex()
                    .gap_2()
                    .child(
                        bot_avatar(
                            SharedString::from(format!("category-preview-{}", bot.id)),
                            bot.avatar,
                            px(24.),
                        )
                        .still(),
                    )
                    .child(div().text_xs().min_w_0().truncate().child(bot.name.clone())),
            );
        }
        if selected.is_empty() {
            preview_bots = preview_bots.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Your bots will appear here."),
            );
        }
        preview = preview.child(preview_bots);
        let studio = v_flex()
            .flex_none()
            .gap_2()
            .when(stacked, |studio| studio.w_full().pb_5().border_b_1())
            .when(!stacked, |studio| studio.w(px(218.)).pr_6().border_r_1())
            .border_color(theme.border)
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .child("Category name"),
            )
            .child(
                Input::new(&self.name)
                    .focus_ring(false)
                    .aria_label("Category name")
                    .disabled(self.locked()),
            )
            .when(!stacked, |studio| {
                studio
                    .child(
                        div()
                            .mt_5()
                            .text_size(px(10.))
                            .text_color(theme.muted_foreground)
                            .child("SIDEBAR PREVIEW"),
                    )
                    .child(preview.bg(theme.sidebar).p_3().rounded(theme.radius))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("A little more order in your sidebar."),
                    )
            });
        let visible = self.visible_bots(state, cx);
        let can_pick =
            |bot: &Bot| Some(bot.id) != state.default_bot_id && !state.moving.contains(&bot.id);
        let available: Vec<_> = visible
            .iter()
            .filter(|bot| can_pick(bot))
            .map(|bot| bot.id)
            .collect();
        let all_selected =
            !available.is_empty() && available.iter().all(|id| self.selected.contains(id));
        let choose_all = holder.clone();
        let locked = self.locked() || state.connection != Connection::Connected;
        let header = h_flex()
            .gap_2()
            .justify_between()
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .child(div().font_weight(FontWeight::MEDIUM).child("Add bots"))
                    .child(div().text_color(theme.muted_foreground).child("Optional")),
            )
            .child(
                Button::new("category-select-shown")
                    .small()
                    .ghost()
                    .focus_ring(false)
                    .cursor_pointer()
                    .label(if all_selected {
                        "Deselect shown"
                    } else {
                        "Select shown"
                    })
                    .text_xs()
                    .text_color(theme.primary)
                    .disabled(locked || available.is_empty())
                    .on_click(move |_, window, cx| {
                        choose_all.update(cx, |form, _| {
                            for id in &available {
                                if all_selected {
                                    form.selected.remove(id);
                                } else {
                                    form.selected.insert(*id);
                                }
                            }
                        });
                        window.refresh();
                    }),
            );
        let mut filters = h_flex().gap_1();
        for (filter, id, label, count) in [
            (
                BotFilter::All,
                "category-all-bots",
                "All bots",
                state.active_bots().count(),
            ),
            (
                BotFilter::Unassigned,
                "category-unassigned-bots",
                "Unassigned",
                state
                    .active_bots()
                    .filter(|bot| category_name(bot, &state.categories).is_none())
                    .count(),
            ),
        ] {
            let choose = holder.clone();
            filters = filters.child(
                Button::new(id)
                    .small()
                    .ghost()
                    .focus_ring(false)
                    .cursor_pointer()
                    .label(format!("{label} {count}"))
                    .selected(self.filter == filter)
                    .disabled(self.busy)
                    .on_click(move |_, window, cx| {
                        choose.update(cx, |form, _| form.filter = filter);
                        window.refresh();
                    }),
            );
        }
        let mut list = v_flex()
            .id("category-bot-list")
            .h(px(300.))
            .overflow_y_scroll()
            .gap_1();
        for bot in &visible {
            let checked = self.selected.contains(&bot.id);
            let disabled = locked || !can_pick(bot);
            let choose = holder.clone();
            let id = bot.id;
            let category = category_name(bot, &state.categories);
            let subtitle = if Some(id) == state.default_bot_id {
                "Main assistant stays pinned".to_owned()
            } else if state.moving.contains(&id) && !self.busy {
                "Moving…".to_owned()
            } else {
                category
                    .map(|name| format!("In {name}"))
                    .unwrap_or_else(|| "Unassigned".to_owned())
            };
            list = list.child(
                Checkbox::new(SharedString::from(format!("category-bot-{id}")))
                    .checked(checked)
                    .focus_ring(false)
                    .disabled(disabled)
                    .w_full()
                    .p_2()
                    .items_center()
                    .flex_row_reverse()
                    .rounded(theme.radius)
                    .when(checked, |row| row.bg(theme.list_active))
                    .when(!disabled, |row| row.cursor_pointer())
                    .when(!disabled && !checked, |row| {
                        row.hover(|style| style.bg(theme.accent))
                    })
                    .accessibility_label(format!(
                        "{}, {}",
                        bot.name,
                        category.unwrap_or("Unassigned")
                    ))
                    .child(
                        h_flex()
                            .gap_2p5()
                            .min_w_0()
                            .child(
                                bot_avatar(
                                    SharedString::from(format!("category-picker-avatar-{id}")),
                                    bot.avatar,
                                    px(30.),
                                )
                                .still(),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::MEDIUM)
                                            .truncate()
                                            .child(bot.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(theme.muted_foreground)
                                            .truncate()
                                            .child(subtitle),
                                    ),
                            ),
                    )
                    .on_change(move |checked, window, cx| {
                        choose.update(cx, |form, _| {
                            if *checked {
                                form.selected.insert(id);
                            } else {
                                form.selected.remove(&id);
                            }
                        });
                        window.refresh();
                    }),
            );
        }
        if visible.is_empty() {
            list = list.child(
                div()
                    .py_6()
                    .text_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if state.active_bots().next().is_none() {
                        "No bots yet. You can add them later."
                    } else {
                        "No bots found. Try another search or filter."
                    }),
            );
        }
        let mut picker = v_flex()
            .flex_1()
            .min_w_0()
            .gap_2p5()
            .child(header)
            .child(
                Input::new(&self.search)
                    .focus_ring(false)
                    .aria_label("Search bots")
                    .prefix(
                        Icon::new(IconName::Search)
                            .size_3p5()
                            .text_color(theme.muted_foreground),
                    ),
            )
            .child(filters)
            .child(list);
        let moving: Vec<_> = selected
            .iter()
            .filter(|bot| category_name(bot, &state.categories).is_some())
            .collect();
        if !moving.is_empty() && !self.locked() {
            let subject = if moving.len() == 1 {
                format!("{} will move", moving[0].name)
            } else {
                format!("{} bots will move", moving.len())
            };
            let mut categories: Vec<_> = moving
                .iter()
                .filter_map(|bot| category_name(bot, &state.categories))
                .collect();
            categories.sort_unstable();
            categories.dedup();
            picker = picker.child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(IconName::ArrowRight).size_3p5().mt_0p5())
                    .child(div().flex_1().child(format!(
                        "{subject} from {} into this category.",
                        categories.join(", ")
                    ))),
            );
        }
        let columns = if stacked {
            v_flex()
        } else {
            h_flex().items_stretch()
        };
        columns
            .gap_6()
            .child(studio)
            .child(picker)
            .into_any_element()
    }
}
