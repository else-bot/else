use super::super::schedule_text::{describe_cron, describe_hours, local_time};
use super::{Banner, RoutineChange, banner, last_run, retryable};
use crate::app::ElseApp;
use crate::app::panel::{alert, item_name, meta, meta_line};
use crate::app::request::Request;
use crate::controls::{Control as _, Scale, Tier, toggle};
use crate::profile_picker::effort_label;
use chrono::Utc;
use else_wire::routines::Routine;
use gpui_kit::base::HoverCard;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Icon, IconName, Sizable as _, h_flex,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::time::Duration;
use uuid::Uuid;

impl ElseApp {
    pub(super) fn render_routine(
        &self,
        bot: Uuid,
        routine: &Routine,
        save: Option<&Request<RoutineChange>>,
        connected: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = routine.id;
        let busy = save.is_some_and(Request::busy);
        let pending = save.and_then(Request::pending);
        let checked = match pending {
            Some(RoutineChange::Enable(enabled)) => *enabled,
            _ => routine.enabled,
        };
        let passed = !checked && routine.run_at.is_some_and(|at| at <= Utc::now());
        let switch = toggle(
            ElementId::Name(format!("routine-{id}").into()),
            checked,
            busy || !connected || passed,
            cx,
        )
        .accessibility_label(format!("Run {}", routine.name))
        .on_click(cx.listener(move |app, enabled: &bool, window, cx| {
            app.set_routine(bot, id, *enabled, window, cx)
        }));
        let retrying = routine.retrying || matches!(pending, Some(RoutineChange::Retry));
        let dismiss = (routine.failed_runs > 0 && !retrying).then(|| {
            Button::new(ElementId::Name(format!("routine-dismiss-{id}").into()))
                .control(Tier::Ghost, Scale::Compact, busy || !connected, cx)
                .label("Dismiss")
                .loading(matches!(pending, Some(RoutineChange::DismissFailures)))
                .accessibility_label(format!("Dismiss the failures of {}", routine.name))
                .on_click(cx.listener(move |app, _, window, cx| {
                    app.dismiss_routine_failures(bot, id, window, cx)
                }))
        });
        let retry = (retrying || retryable(routine)).then(|| {
            Button::new(ElementId::Name(format!("routine-retry-{id}").into()))
                .control(
                    Tier::Primary,
                    Scale::Compact,
                    busy || !connected || retrying,
                    cx,
                )
                .label("Retry")
                .loading(retrying)
                .accessibility_label(format!("Retry {} now", routine.name))
                .on_click(
                    cx.listener(move |app, _, window, cx| app.retry_routine(bot, id, window, cx)),
                )
        });
        // Separate tokens, so a narrow panel wraps between them. Only a
        // token wider than the row, like a long cron description, wraps
        // inside itself.
        let when = match (routine.run_at, &routine.active_hours) {
            (Some(at), _) => vec!["Once".to_owned(), local_time(at, &routine.timezone)],
            (None, Some(hours)) => vec![describe_cron(&routine.cron), describe_hours(hours)],
            (None, None) => vec![describe_cron(&routine.cron)],
        };
        let schedule = format!("{} · {}", when.join(" · "), routine.timezone);
        let last = last_run(routine);
        let choice = routine.model.as_ref().or_else(|| {
            self.settings
                .get(&bot)
                .and_then(|entry| entry.model.shown()?.as_ref())
                .map(|profile| &profile.choice)
        });
        let model_title = match (&routine.model, choice) {
            (Some(choice), _) => self.model_name(choice),
            (None, Some(choice)) => format!("Bot model · {}", self.model_name(choice)),
            (None, None) => "Bot model".to_owned(),
        };
        let model_details = choice
            .map(|choice| {
                let speed = if choice.service_tier == "priority" {
                    "Fast"
                } else {
                    "Standard"
                };
                format!(
                    "{} · {} reasoning · {speed}",
                    self.provider_label(&choice.provider_id),
                    effort_label(&choice.reasoning_effort)
                )
            })
            .unwrap_or_else(|| "No model yet. Choose a bot model.".to_owned());
        let unavailable = routine
            .model
            .as_ref()
            .is_some_and(|choice| !self.model_available(choice));
        let warning = routine
            .requested_model
            .as_ref()
            .map(|requested| format!("Couldn't use \"{requested}\". Using bot model."))
            .or_else(|| {
                unavailable.then(|| "Saved model is unavailable. Choose another model.".to_owned())
            });
        // The model is changed from the row's menu. The meta line names it
        // only when the routine overrides the bot model shown above.
        let quiet = match pending {
            Some(RoutineChange::Quiet(quiet)) => *quiet,
            _ => routine.quiet,
        };
        let mut tokens: Vec<AnyElement> = when
            .into_iter()
            .map(|token| meta(token, cx).min_w_0().into_any_element())
            .collect();
        tokens.push(
            meta(routine.timezone.clone(), cx)
                .flex_none()
                .whitespace_nowrap()
                .into_any_element(),
        );
        if let Some(choice) = routine.model.as_ref().filter(|_| routine.run_at.is_none()) {
            tokens.push(
                meta(self.model_name(choice), cx)
                    .flex_none()
                    .whitespace_nowrap()
                    .into_any_element(),
            );
        }
        if quiet && routine.run_at.is_none() {
            tokens.push(
                meta("Quiet".to_owned(), cx)
                    .flex_none()
                    .whitespace_nowrap()
                    .into_any_element(),
            );
        }
        let summary = div()
            .id(ElementId::Name(format!("routine-pill-{id}").into()))
            .w_full()
            .child(
                item_name(routine.name.clone())
                    .line_clamp(2)
                    .text_ellipsis(),
            );
        let model_line = routine
            .run_at
            .is_none()
            .then(|| format!("{model_title} · {model_details}"));
        let name = routine.name.clone();
        let prompt = routine.prompt.clone();
        let next = match (routine.enabled, routine.next_run_at) {
            (false, _) => None,
            _ if routine.run_at.is_some() => None,
            (true, Some(at)) => Some(format!("Next run {}", local_time(at, &routine.timezone))),
            (true, None) => Some("No further runs".to_owned()),
        };
        let description = HoverCard::new(ElementId::Name(format!("routine-card-{id}").into()))
            .w_full()
            .anchor(Anchor::RightCenter)
            .open_delay(Duration::from_millis(300))
            .trigger(summary)
            .content(move |_, _, cx| {
                let theme = cx.theme();
                let line =
                    |text: String, color: Hsla| div().text_xs().text_color(color).child(text);
                let card = v_flex()
                    .w(px(300.))
                    .min_w_0()
                    .p_3()
                    .gap_2()
                    .rounded(theme.radius_lg)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .whitespace_normal()
                            .child(name.clone()),
                    )
                    .child(
                        div()
                            .id("routine-card-prompt")
                            .max_h(px(240.))
                            .overflow_y_scroll()
                            .text_sm()
                            .whitespace_normal()
                            .text_color(theme.foreground)
                            .child(prompt.clone()),
                    )
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(line(schedule.clone(), theme.muted_foreground))
                            .children(
                                model_line
                                    .clone()
                                    .map(|text| line(text, theme.muted_foreground)),
                            )
                            .children(next.clone().map(|text| line(text, theme.muted_foreground)))
                            .children(last.clone().map(|(text, error)| {
                                line(
                                    text,
                                    if error {
                                        theme.danger
                                    } else {
                                        theme.muted_foreground
                                    },
                                )
                            })),
                    );
                let (border, background) = (theme.border, theme.popover);
                h_flex()
                    .id("routine-preview")
                    .items_center()
                    .pr_3()
                    .child(card)
                    .child(
                        canvas(
                            |_, _, _| (),
                            move |bounds, _, window, _| {
                                let mid = bounds.center().y;
                                for (inset, color) in [(0., border), (1., background)] {
                                    let mut path = PathBuilder::fill();
                                    path.move_to(point(
                                        bounds.left() - px(1.),
                                        bounds.top() + px(inset),
                                    ));
                                    path.line_to(point(bounds.right() - px(inset), mid));
                                    path.line_to(point(
                                        bounds.left() - px(1.),
                                        bounds.bottom() - px(inset),
                                    ));
                                    path.close();
                                    if let Ok(path) = path.build() {
                                        window.paint_path(path, color);
                                    }
                                }
                            },
                        )
                        .w(px(8.))
                        .h(px(16.)),
                    )
            })
            .into_any_element();
        let theme = cx.theme().clone();
        let owner = cx.entity().downgrade();
        let reminder = routine.run_at.is_some();
        let no_models = self.caps.is_none();
        let (model_badge, model_hint) = match (&routine.model, choice) {
            (Some(choice), _) => (
                "Custom",
                Hint {
                    title: format!("{}, for this routine only", self.model_name(choice)).into(),
                    body: format!("{model_details}. The bot's chats keep their own model.").into(),
                },
            ),
            (None, Some(choice)) => (
                "Bot",
                Hint {
                    title: format!("{}, from the bot", self.model_name(choice)).into(),
                    body: format!(
                        "{model_details}. Pick another model for this routine only; \
                         the bot's chats keep theirs."
                    )
                    .into(),
                },
            ),
            (None, None) => (
                "Bot",
                Hint {
                    title: "No bot model yet".into(),
                    body: model_details.clone().into(),
                },
            ),
        };
        let quiet_hint = Hint {
            title: "Only post when there's news".into(),
            body: "The routine still runs on schedule. If nothing changed since the last run, \
                   it stays silent instead of saying so."
                .into(),
        };
        let danger = theme.danger;
        // Quiet, the model and Delete sit in one menu, so the name and the
        // schedule keep the row's width in a narrow panel. Each setting
        // explains itself in a card beside the menu.
        let actions_menu = Button::new(ElementId::Name(format!("routine-more-{id}").into()))
            .focus_ring(false)
            .cursor_pointer()
            .ghost()
            .xsmall()
            .icon(IconName::Ellipsis)
            .accessibility_label(format!("Actions for {}", routine.name))
            .loading(matches!(
                pending,
                Some(RoutineChange::Quiet(_) | RoutineChange::Model | RoutineChange::Delete)
            ))
            .disabled(busy || !connected)
            .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                let act = |f: fn(&mut Self, Uuid, Uuid, &mut Window, &mut Context<Self>)| {
                    let owner = owner.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let _ = owner.update(cx, |app, cx| f(app, bot, id, window, cx));
                    }
                };
                // Each item draws its own icon, so the hint card anchors at
                // the menu's edge; the quiet check goes on the right.
                let menu = menu.check_side(gpui_kit::component::Side::Right);
                let menu = if reminder {
                    menu
                } else {
                    let quiet_hint = quiet_hint.clone();
                    let model_hint = model_hint.clone();
                    menu.item(menu_heading("Delivery"))
                        .item(
                            PopupMenuItem::element(move |_, cx| {
                                hinted(
                                    format!("routine-quiet-hint-{id}"),
                                    gpui_kit::assets::IconName::BellOff,
                                    "Quiet if no news",
                                    None,
                                    &quiet_hint,
                                    cx,
                                )
                            })
                            .checked(quiet)
                            .on_click(if quiet {
                                act(|app, bot, id, window, cx| {
                                    app.set_routine_quiet(bot, id, false, window, cx)
                                })
                            } else {
                                act(|app, bot, id, window, cx| {
                                    app.set_routine_quiet(bot, id, true, window, cx)
                                })
                            }),
                        )
                        .item(menu_heading("Run with"))
                        .item(
                            PopupMenuItem::element(move |_, cx| {
                                hinted(
                                    format!("routine-model-hint-{id}"),
                                    gpui_kit::assets::IconName::Cpu,
                                    "Change model…",
                                    Some(model_badge),
                                    &model_hint,
                                    cx,
                                )
                            })
                            .disabled(no_models)
                            .on_click(act(
                                |app, bot, id, window, cx| {
                                    app.open_routine_model_dialog(bot, id, window, cx)
                                },
                            )),
                        )
                        .separator()
                };
                menu.item(
                    PopupMenuItem::element(move |_, _| {
                        h_flex()
                            .gap_2()
                            .text_color(danger)
                            .child(Icon::new(gpui_kit::assets::IconName::Trash).xsmall())
                            .child("Delete…")
                    })
                    .on_click(act(|app, bot, id, window, cx| {
                        app.open_routine_delete_dialog(bot, id, window, cx)
                    })),
                )
            });
        // The switch lines up with the name; the schedule below it gets the
        // row's whole width.
        let header = v_flex()
            .w_full()
            .gap_0p5()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_start()
                    .child(div().flex_1().min_w_0().child(description))
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .items_center()
                            .child(actions_menu)
                            .child(switch),
                    ),
            )
            .child(meta_line(tokens, cx));
        let banner = banner(routine, save);
        let shown = banner.as_ref().map_or("", Banner::text).to_owned();
        let problem = matches!(banner, Some(Banner::Problem(_)));
        let expanded = banner.is_some() && self.expanded_routine_problems.get(&id) == Some(&shown);
        // The panel's 1 px border and its sections' 16 px sides.
        let column = px(self.details_panel_width(window) - 33.);
        let width = if problem {
            column - REASON_ICON
        } else {
            column
        };
        let long = banner.is_some() && reason_is_long(&shown, width, window, cx);
        let opened = shown.clone();
        let more = long.then(|| {
            Button::new(ElementId::Name(format!("routine-reason-{id}").into()))
                .control(Tier::Ghost, Scale::Compact, false, cx)
                .flush_start(Scale::Compact)
                .label(if expanded { "Show less" } else { "Show more" })
                .on_click(cx.listener(move |app, _, _, cx| {
                    if expanded {
                        app.expanded_routine_problems.remove(&id);
                    } else {
                        app.expanded_routine_problems.insert(id, opened.clone());
                    }
                    cx.notify();
                }))
        });
        let actions = (retry.is_some() || dismiss.is_some()).then(|| {
            h_flex()
                .gap_1()
                .items_center()
                .children(retry)
                .children(dismiss)
        });
        let color = if problem {
            theme.danger
        } else {
            theme.muted_foreground
        };
        let banner = match banner {
            Some(_) => Some(
                reason(shown, color, problem, long && !expanded, more, actions).into_any_element(),
            ),
            None => actions.map(IntoElement::into_any_element),
        };
        v_flex()
            .w_full()
            .gap_1()
            .child(header)
            .children(banner)
            .children(warning.map(|warning| alert(warning, theme.warning, cx)))
            .into_any_element()
    }
}

/// What a menu item's side card says about the setting it changes.
#[derive(Clone)]
struct Hint {
    title: SharedString,
    body: SharedString,
}

/// A muted, unclickable heading over a group of menu items.
fn menu_heading(title: &'static str) -> PopupMenuItem {
    PopupMenuItem::element(move |_, _| div().text_xs().child(title.to_uppercase())).disabled(true)
}

/// A menu item's label, an optional value badge, and a card beside the menu
/// that explains the setting while the pointer rests on the item.
fn hinted(
    id: String,
    icon: gpui_kit::assets::IconName,
    label: &'static str,
    badge: Option<&'static str>,
    hint: &Hint,
    cx: &App,
) -> HoverCard {
    let theme = cx.theme();
    let badge = badge.map(|text| {
        div()
            .px_1p5()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(text)
    });
    let hint = hint.clone();
    HoverCard::new(ElementId::Name(id.into()))
        .flex_1()
        .anchor(Anchor::RightCenter)
        .open_delay(Duration::from_millis(300))
        .trigger(
            h_flex()
                .w_full()
                .gap_2()
                .child(Icon::new(icon).xsmall())
                .child(div().flex_1().child(label))
                .children(badge),
        )
        .content(move |_, _, cx| {
            let theme = cx.theme();
            let card = v_flex()
                .w(px(240.))
                .p_3()
                .gap_1()
                .rounded(theme.radius_lg)
                .bg(theme.popover)
                .text_color(theme.popover_foreground)
                .border_1()
                .border_color(theme.border)
                .shadow_lg()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .whitespace_normal()
                        .child(hint.title.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .whitespace_normal()
                        .text_color(theme.muted_foreground)
                        .child(hint.body.clone()),
                );
            // The padding, not a margin, keeps the card clear of the menu's
            // edge: the popup measures this row to place it.
            h_flex().id("routine-hint").pr_5().child(card)
        })
}

/// The lines a routine's banner shows before "Show more".
const REASON_LINES: usize = 3;
const REASON_SIZE: Pixels = px(12.5);
/// The warning icon and its gap, beside the reason.
const REASON_ICON: Pixels = px(18.);

/// Whether `text` wraps past [`REASON_LINES`] at `width`, shaped as the
/// text element shapes it.
fn reason_is_long(text: &str, width: Pixels, window: &Window, cx: &App) -> bool {
    let run = TextRun {
        len: text.len(),
        font: font(cx.theme().font_family.clone()),
        color: Hsla::default(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_text(
            SharedString::from(text.to_owned()),
            REASON_SIZE,
            &[run],
            Some(width),
            None,
        )
        .is_ok_and(|lines| {
            lines
                .iter()
                .map(|line| line.wrap_boundaries().len() + 1)
                .sum::<usize>()
                > REASON_LINES
        })
}

/// A routine's state under its name: why it stopped or how a retry is
/// going, with the actions that answer it on the line below.
fn reason(
    text: String,
    color: Hsla,
    icon: bool,
    clamped: bool,
    more: Option<Button>,
    actions: Option<Div>,
) -> Div {
    let body = v_flex()
        .flex_1()
        .min_w_0()
        .items_start()
        .child(
            div()
                .w_full()
                .whitespace_normal()
                .when(clamped, |body| {
                    body.line_clamp(REASON_LINES).text_ellipsis()
                })
                .child(text),
        )
        .children(more);
    v_flex()
        .w_full()
        .gap_1()
        .child(
            h_flex()
                .pt_0p5()
                .gap_1p5()
                .items_start()
                .text_size(REASON_SIZE)
                .line_height(px(18.))
                .text_color(color)
                .when(icon, |row| {
                    row.child(
                        Icon::new(IconName::TriangleAlert)
                            .size_3()
                            .mt(px(3.))
                            .flex_none(),
                    )
                })
                .child(body),
        )
        .children(actions.map(|actions| {
            h_flex()
                .when(icon, |row| row.child(div().flex_none().w(REASON_ICON)))
                .child(actions)
        }))
}
