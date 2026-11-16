use super::*;
use crate::api::{Attribution, BotUsage, ProviderUsage, UsageReport, UsageWindow};
use crate::avatar::{Avatar, bot_avatar};
use crate::controls::{Control as _, Scale, Tier, field_style, segment, segment_track};
use crate::usage_ui::{Loaded, bar, color, dot, local_time, percent, resets, tokens, turns};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::select::{Select, SelectState};
use gpui_kit::component::{FocusableExt as _, IndexPath, h_flex};
use std::time::Duration;

#[derive(Clone, PartialEq, Eq)]
struct BotChoice {
    id: Option<Uuid>,
    name: SharedString,
}

impl SearchableListItem for BotChoice {
    type Value = Option<Uuid>;
    fn title(&self) -> SharedString {
        self.name.clone()
    }
    fn value(&self) -> &Self::Value {
        &self.id
    }
}

pub(super) struct View {
    id: Uuid,
    state: Loaded<UsageReport>,
    provider: Option<String>,
    filter: Entity<SelectState<Vec<BotChoice>>>,
    expanded: Option<Uuid>,
    _filter: Subscription,
    _poll: Task<()>,
}

impl Shell {
    pub(super) fn open_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = &self.active else { return };
        let client = active.client.clone();
        let filter = cx.new(|cx| {
            SelectState::new(
                vec![BotChoice {
                    id: None,
                    name: "All bots".into(),
                }],
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });
        let subscription = cx.observe(&filter, |_, _, cx| cx.notify());
        let id = Uuid::new_v4();
        let poll = cx.spawn_in(window, async move |this, cx| {
            while let Ok(Some(attempt)) = this.update(cx, |shell, _| shell.begin_usage(id)) {
                let result = client.clone().usage().await;
                let open = this.update_in(cx, |shell, window, cx| {
                    shell.apply_usage(id, attempt, result, window, cx)
                });
                if !matches!(open, Ok(true)) {
                    break;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(10))
                    .await;
            }
        });
        self.usage = Some(View {
            id,
            state: Loaded::Loading(Uuid::nil()),
            provider: None,
            filter,
            expanded: None,
            _filter: subscription,
            _poll: poll,
        });
        cx.notify();
    }

    /// Starts a fetch for the open usage view with this `id`, if it is
    /// still open.
    fn begin_usage(&mut self, id: Uuid) -> Option<Uuid> {
        let view = self.usage.as_mut().filter(|view| view.id == id)?;
        Some(view.state.begin())
    }

    /// Applies the reply to `attempt` to the usage view with this `id`;
    /// false once that view is gone.
    fn apply_usage(
        &mut self,
        id: Uuid,
        attempt: Uuid,
        result: Result<UsageReport, crate::api::ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let shell = self;
        let Some(view) = shell.usage.as_mut().filter(|view| view.id == id) else {
            return false;
        };
        if !view.state.awaits(attempt) {
            return true;
        }
        if let Ok(report) = &result {
            if !report
                .providers
                .iter()
                .any(|provider| Some(&provider.provider_id) == view.provider.as_ref())
            {
                let wanted = view.filter.read(cx).selected_value().copied().flatten();
                view.provider = wanted
                    .and_then(|bot| {
                        report
                            .providers
                            .iter()
                            .find(|provider| provider.windows.iter().any(|w| w.bot(bot).is_some()))
                    })
                    .or_else(|| report.default_provider())
                    .map(|provider| provider.provider_id.clone());
                view.expanded = None;
            }
            let mut bots: std::collections::BTreeMap<_, _> = shell
                .active
                .as_ref()
                .map(|active| {
                    active
                        .app
                        .read(cx)
                        .bots()
                        .iter()
                        .map(|bot| (bot.id, bot.name.clone()))
                        .collect()
                })
                .unwrap_or_default();
            for bot in report
                .providers
                .iter()
                .flat_map(|p| &p.windows)
                .flat_map(|w| &w.bots)
            {
                bots.insert(bot.bot_id, bot.name.clone());
            }
            let choices = std::iter::once(BotChoice {
                id: None,
                name: "All bots".into(),
            })
            .chain(bots.into_iter().map(|(id, name)| BotChoice {
                id: Some(id),
                name: name.into(),
            }))
            .collect();
            view.filter
                .update(cx, |filter, cx| filter.set_items(choices, window, cx));
        }
        view.state
            .settle(attempt, result.map_err(|error| error.to_string()));
        cx.notify();
        true
    }

    /// Fetches the report once, keeping the page as it is until it arrives.
    fn refetch_usage(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(client), Some(id)) = (
            self.active.as_ref().map(|active| active.client.clone()),
            self.usage.as_ref().map(|view| view.id),
        ) else {
            return;
        };
        let Some(attempt) = self.begin_usage(id) else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = client.usage().await;
            this.update_in(cx, |shell, window, cx| {
                shell.apply_usage(id, attempt, result, window, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn filter_usage(
        &mut self,
        bot: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self.active.as_ref().and_then(|active| {
            active
                .app
                .read(cx)
                .bots()
                .iter()
                .find(|entry| Some(entry.id) == bot)
                .map(|bot| bot.name.clone())
        });
        if let Some(view) = &mut self.usage {
            view.filter.update(cx, |filter, cx| {
                if let Some(name) = name {
                    filter.set_items(
                        vec![
                            BotChoice {
                                id: None,
                                name: "All bots".into(),
                            },
                            BotChoice {
                                id: bot,
                                name: name.into(),
                            },
                        ],
                        window,
                        cx,
                    );
                }
                filter.set_selected_value(&bot, window, cx);
            });
        }
    }

    pub(super) fn render_usage(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut body = v_flex().w_full().gap_4().text_sm();
        let heading = v_flex()
            .gap_1()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(px(15.))
                    .child("Usage"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Your AI subscriptions, shared by your bots and your own use."),
            );
        let Some(view) = &self.usage else {
            return body
                .child(heading)
                .child("Loading usage…")
                .into_any_element();
        };
        let (report, error) = match &view.state {
            Loaded::Loading(_) => {
                return body
                    .child(heading)
                    .child("Loading usage…")
                    .into_any_element();
            }
            Loaded::Failed(error) => (None, Some(error)),
            Loaded::Ready { value, error, .. } => (Some(value), error.as_ref()),
        };
        if let Some(error) = error {
            body = body.child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(format!("Couldn't refresh usage. {error}")),
                    )
                    .child(
                        Button::new("usage-retry")
                            .control(Tier::Secondary, Scale::Compact, false, cx)
                            .label("Retry")
                            .on_click(
                                cx.listener(|shell, _, window, cx| shell.refetch_usage(window, cx)),
                            ),
                    ),
            );
        }
        let Some(report) = report else {
            return body.child(heading).into_any_element();
        };
        if report.providers.is_empty() {
            return body
                .child(heading)
                .child(settings::empty_state(
                    IconName::Gauge,
                    "No usage yet",
                    "Run a bot turn to see usage by provider and bot.",
                    Button::new("usage-empty-chat")
                        .control(Tier::Secondary, Scale::Standard, false, cx)
                        .label("Back to chat")
                        .on_click(cx.listener(|shell, _, window, cx| {
                            window.close_dialog(cx);
                            shell.cancel(window, cx);
                        })),
                    cx,
                ))
                .into_any_element();
        }
        let provider_control = segment_track(cx)
            .w(px(report.providers.len() as f32 * 88.))
            .children(report.providers.iter().map(|provider| {
                let id = provider.provider_id.clone();
                segment(
                    SharedString::from(format!("usage-provider-{id}")),
                    SharedString::from(provider.label()),
                    view.provider.as_ref() == Some(&id),
                    false,
                    cx,
                )
                .on_click(cx.listener(move |shell, _, _, cx| {
                    if let Some(view) = &mut shell.usage {
                        view.provider = Some(id.clone());
                        view.expanded = None;
                        cx.notify();
                    }
                }))
            }));
        body = body.child(heading).child(
            h_flex()
                .gap_3()
                .justify_between()
                .child(provider_control)
                .child(
                    div().w(px(200.)).child(
                        field_style(Select::new(&view.filter).appearance(false), cx)
                            .border_1()
                            .rounded_md()
                            .focus_ring(false)
                            .cursor_pointer()
                            .accessibility_label("Filter usage by bot"),
                    ),
                ),
        );
        let provider = report
            .providers
            .iter()
            .find(|provider| Some(&provider.provider_id) == view.provider.as_ref());
        let Some(provider) = provider else {
            return body.into_any_element();
        };
        let filter = view.filter.read(cx).selected_value().copied().flatten();
        let avatars: HashMap<Uuid, Avatar> = self
            .active
            .as_ref()
            .map(|active| {
                active
                    .app
                    .read(cx)
                    .bots()
                    .iter()
                    .map(|bot| (bot.id, bot.avatar))
                    .collect()
            })
            .unwrap_or_default();
        body = body.child(
            h_flex().w_full().items_stretch().gap_3().children(
                provider
                    .windows
                    .iter()
                    .map(|window| gauge(provider, window, filter, &avatars, cx)),
            ),
        );
        if let Some(window) = provider
            .windows
            .first()
            .filter(|window| !window.buckets.is_empty())
        {
            body = body.child(timeline(window, filter, &avatars, cx));
        }
        let source_note = format!(
            "Figures come from {} with each bot turn; usage outside else.bot updates on the next turn.",
            provider.label()
        );
        let updated = format!("Report updated {}", local_time(report.generated_at));
        body = body
            .child(self.usage_table(provider, filter, &avatars, view.expanded, cx))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground.opacity(0.8))
                    .child(source_note),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground.opacity(0.65))
                    .child(updated),
            );
        body.into_any_element()
    }

    fn usage_table(
        &self,
        provider: &ProviderUsage,
        filter: Option<Uuid>,
        avatars: &HashMap<Uuid, Avatar>,
        expanded: Option<Uuid>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let mut bots: Vec<&BotUsage> = Vec::new();
        for bot in provider.windows.iter().flat_map(|window| &window.bots) {
            if !bots.iter().any(|entry| entry.bot_id == bot.bot_id)
                && filter.is_none_or(|id| id == bot.bot_id)
            {
                bots.push(bot);
            }
        }
        let headers = h_flex()
            .pb_2()
            .border_b_1()
            .border_color(theme.border)
            .text_color(theme.muted_foreground)
            .text_xs()
            .child(cell("Bot", 210.))
            .children(
                provider
                    .windows
                    .iter()
                    .map(|window| cell(short_window(window), 64.)),
            )
            .child(cell("Turns", 48.))
            .child(cell("Tokens in / out", 112.))
            .child(cell("Cache read / write", 122.))
            .child(cell("Mostly", 110.));
        let mut table = v_flex()
            .min_w(px(210. + provider.windows.len() as f32 * 64. + 392.))
            .child(headers);
        if bots.is_empty() {
            table = table.child(div().py_4().text_color(theme.muted_foreground).child(
                if filter.is_some() {
                    "This bot has no turns in these windows."
                } else {
                    "No bot turns in these windows."
                },
            ));
        }
        let ledger = provider.windows.last();
        for listed in bots {
            let id = listed.bot_id;
            let bot = ledger.and_then(|window| window.bot(id)).unwrap_or(listed);
            let row = h_flex()
                .id(SharedString::from(format!("usage-bot-{id}")))
                .w_full()
                .py_3()
                .items_center()
                .cursor_pointer()
                .border_b_1()
                .border_color(theme.border.opacity(0.5))
                .hover(|row| row.bg(theme.muted_foreground.opacity(0.06)))
                .on_click(cx.listener(move |shell, _, _, cx| {
                    if let Some(view) = &mut shell.usage {
                        view.expanded = (view.expanded != Some(id)).then_some(id);
                        cx.notify();
                    }
                }))
                .child(
                    h_flex()
                        .w(px(210.))
                        .flex_none()
                        .gap_2()
                        .child(
                            avatars
                                .get(&id)
                                .map(|avatar| {
                                    bot_avatar(
                                        SharedString::from(format!("usage-avatar-{id}")),
                                        *avatar,
                                        px(24.),
                                    )
                                    .still()
                                    .into_any_element()
                                })
                                .unwrap_or_else(|| dot(color(id, None)).into_any_element()),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().truncate().child(bot.name.clone()))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .truncate()
                                        .child(bot.models.join(", ")),
                                ),
                        ),
                )
                .children(provider.windows.iter().map(|window| {
                    cell(
                        window
                            .bot(id)
                            .map(|bot| percent(bot.percent))
                            .unwrap_or_else(|| "—".into()),
                        64.,
                    )
                }))
                .child(cell(bot.turns.to_string(), 48.))
                .child(cell(
                    format!(
                        "{} / {}",
                        tokens(bot.input_tokens),
                        tokens(bot.output_tokens)
                    ),
                    112.,
                ))
                .child(cell(
                    format!(
                        "{} / {}",
                        tokens(bot.cached_input_tokens),
                        tokens(bot.cache_write_tokens)
                    ),
                    122.,
                ))
                .child(cell(
                    bot.sources
                        .first()
                        .map(|source| source.source.label())
                        .unwrap_or("—"),
                    110.,
                ));
            table = table.child(row);
            if expanded == Some(id)
                && let Some(window) = ledger.filter(|window| window.bot(id).is_some())
            {
                for (index, source) in bot.sources.iter().enumerate() {
                    table = table.child(
                        h_flex()
                            .id(SharedString::from(format!(
                                "usage-source-{}-{}-{index}",
                                window.minutes,
                                source.routine_id.unwrap_or(id)
                            )))
                            .w_full()
                            .gap_3()
                            .py_2()
                            .pl_8()
                            .pr_3()
                            .bg(theme.muted_foreground.opacity(0.05))
                            .text_xs()
                            .child(
                                div()
                                    .w(px(100.))
                                    .text_color(theme.muted_foreground)
                                    .child(source.source.label()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(source.label.clone().unwrap_or_default()),
                            )
                            .child(cell(turns(source.turns), 65.))
                            .child(cell(percent(source.percent), 55.)),
                    );
                }
            }
        }
        if filter.is_none()
            && provider
                .windows
                .iter()
                .any(|window| window.attribution.outside().is_some())
        {
            table = table.child(
                h_flex()
                    .py_3()
                    .text_color(theme.muted_foreground)
                    .child(cell("Outside else.bot", 210.))
                    .children(provider.windows.iter().map(|window| {
                        cell(
                            window
                                .attribution
                                .outside()
                                .map(percent)
                                .unwrap_or_else(|| "—".into()),
                            64.,
                        )
                    })),
            );
        }
        let priced = provider
            .windows
            .iter()
            .filter(|window| window.attribution.is_priced())
            .count();
        let unit = if priced == provider.windows.len() {
            "Share of your cap"
        } else if priced > 0 {
            "Share of your cap where attributed, else share of else.bot usage"
        } else {
            "Share of else.bot usage"
        };
        let scope = ledger
            .map(|window| window.label().to_lowercase())
            .unwrap_or_default();
        let caption = format!(
            "{unit} · expand a bot for its sources. Turns, tokens, and sources cover the {scope}."
        );
        v_flex()
            .gap_2()
            .child(crate::appearance::section_label("By bot", cx))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(caption),
            )
            .child(
                div()
                    .id("usage-ledger-scroll")
                    .w_full()
                    .overflow_x_scroll()
                    .child(table),
            )
            .into_any_element()
    }
}

fn short_window(window: &UsageWindow) -> String {
    match window.minutes {
        300 => "5-hour".into(),
        10080 => "Week".into(),
        _ => window.label(),
    }
}

fn cell(text: impl Into<SharedString>, width: f32) -> Div {
    div()
        .flex_none()
        .w(px(width))
        .pr_2()
        .truncate()
        .child(text.into())
}

fn gauge(
    provider: &ProviderUsage,
    window: &UsageWindow,
    filter: Option<Uuid>,
    avatars: &HashMap<Uuid, Avatar>,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let mut card = v_flex()
        .flex_1()
        .min_w_0()
        .p_4()
        .gap_2()
        .rounded_lg()
        .bg(theme.background)
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(window.label()),
        );
    card = if let Some(used) = window.attribution.used() {
        card.child(
            div()
                .text_size(px(30.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(percent(used)),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!("used · {}", resets(window.attribution.resets_at()))),
        )
    } else {
        let reports_limits = provider
            .windows
            .iter()
            .any(|window| window.attribution.used().is_some());
        card.child(div().text_sm().child(if reports_limits {
            "No reading since this window reset".to_owned()
        } else {
            format!("{} doesn't report its limits", provider.label())
        }))
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("Share of else.bot usage"),
        )
    };
    card = card.child(
        div()
            .text_xs()
            .text_color(theme.muted_foreground.opacity(0.75))
            .child(
                window
                    .attribution
                    .observed_at()
                    .map(|time| format!("As of {}", local_time(time)))
                    .unwrap_or_else(|| format!("Window starts {}", local_time(window.starts_at))),
            ),
    );
    let unattributed = match window.attribution {
        Attribution::Unattributed { used, .. } => Some(used),
        Attribution::Priced { .. } | Attribution::Unreported => None,
    };
    let segments = window
        .bots
        .iter()
        .filter(|_| unattributed.is_none())
        .map(|bot| {
            (
                bot.percent,
                color(bot.bot_id, avatars.get(&bot.bot_id).copied()).opacity(
                    if filter.is_none_or(|id| id == bot.bot_id) {
                        1.
                    } else {
                        0.18
                    },
                ),
            )
        })
        .chain(
            window
                .attribution
                .outside()
                .or(unattributed)
                .map(|amount| (amount, theme.muted_foreground.opacity(0.4))),
        );
    let capped = window.attribution.used().is_some();
    card = card.child(bar(segments, capped, cx));
    if capped {
        card = card.child(
            div()
                .relative()
                .h(px(14.))
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child(div().absolute().left_0().child("0%"))
                .child(
                    div()
                        .absolute()
                        .left(relative(0.8))
                        .ml(px(-10.))
                        .child("80%"),
                )
                .child(div().absolute().right_0().child("100%")),
        );
    }
    card.child(
        h_flex()
            .flex_wrap()
            .gap_x_3()
            .gap_y_1()
            .text_xs()
            .text_color(theme.muted_foreground)
            .children(window.bots.iter().map(|bot| {
                h_flex()
                    .gap_1()
                    .opacity(if filter.is_none_or(|id| id == bot.bot_id) {
                        1.
                    } else {
                        0.4
                    })
                    .child(dot(color(bot.bot_id, avatars.get(&bot.bot_id).copied())))
                    .child(if unattributed.is_some() {
                        format!("{} {} of else.bot use", bot.name, percent(bot.percent))
                    } else {
                        format!("{} {}", bot.name, percent(bot.percent))
                    })
            }))
            .children(window.attribution.outside().map(|amount| {
                h_flex()
                    .gap_1()
                    .child(dot(theme.muted_foreground.opacity(0.4)))
                    .child(format!("Outside else.bot {}", percent(amount)))
            }))
            .children(unattributed.map(|_| {
                h_flex()
                    .gap_1()
                    .child(dot(theme.muted_foreground.opacity(0.4)))
                    .child("Not attributed yet: it splits once a bot turn moves the total")
            })),
    )
}

fn timeline(
    window: &UsageWindow,
    filter: Option<Uuid>,
    avatars: &HashMap<Uuid, Avatar>,
    cx: &App,
) -> Div {
    let theme = cx.theme();
    let peak = window
        .buckets
        .iter()
        .map(|bucket| bucket.bots.iter().map(|bot| bot.percent).sum::<f64>() + bucket.outside)
        .fold(0f64, f64::max)
        .max(f64::EPSILON);
    v_flex()
        .p_4()
        .gap_3()
        .rounded_lg()
        .bg(theme.background)
        .child(crate::appearance::section_label(
            "This window, every 15 minutes",
            cx,
        ))
        .child(h_flex().w_full().h(px(105.)).items_end().gap_1().children(
            window.buckets.iter().map(|bucket| {
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .justify_end()
                    .h_full()
                    .children((bucket.outside > 0.).then(|| {
                        div()
                            .w_full()
                            .h(px((bucket.outside / peak * 105.) as f32))
                            .bg(theme.muted_foreground.opacity(0.4))
                    }))
                    .children(bucket.bots.iter().rev().map(|bot| {
                        div()
                            .w_full()
                            .h(px((bot.percent / peak * 105.) as f32))
                            .bg(
                                color(bot.bot_id, avatars.get(&bot.bot_id).copied()).opacity(
                                    if filter.is_none_or(|id| id == bot.bot_id) {
                                        1.
                                    } else {
                                        0.18
                                    },
                                ),
                            )
                    }))
            }),
        ))
        .child(
            h_flex()
                .justify_between()
                .text_xs()
                .text_color(theme.muted_foreground)
                .children(
                    window
                        .buckets
                        .first()
                        .map(|bucket| local_time(bucket.starts_at)),
                )
                .child(format!("Peak {} / 15 min", percent(peak)))
                .children(
                    window
                        .buckets
                        .last()
                        .map(|bucket| local_time(bucket.starts_at)),
                ),
        )
}
