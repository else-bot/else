//! Swipe actions on a list row, as iOS lists have them (Mail, Messages): a
//! drag sideways uncovers the actions on that side; letting go past half
//! of them leaves them open, a long swipe runs the first one, and a tap on
//! an action runs it. The drag follows the finger natively; the row
//! springs to rest.

use std::rc::Rc;

use nsh_core::prelude::*;

/// One action behind the row.
#[derive(Clone)]
pub struct Action {
    pub label: &'static str,
    pub tint: Rgba,
    pub run: Rc<dyn Fn()>,
}

impl Action {
    pub fn new(label: &'static str, tint: u32, run: impl Fn() + 'static) -> Self {
        Self {
            label,
            tint: rgb(tint),
            run: Rc::new(run),
        }
    }
}

/// The width of each action.
const WIDTH: f32 = 76.0;
/// How far past its actions a swipe runs the first one.
const FULL: f32 = 2.4;

fn spring() -> Spring {
    Spring::new(320.0)
}

/// Where the row rests after a drag that ended at `x`: open on one side,
/// a full swipe that runs that side's first action, or closed.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Rest {
    Closed,
    Leading,
    Trailing,
    RunLeading,
    RunTrailing,
}

fn rest(x: f32, leading: usize, trailing: usize) -> Rest {
    let lead = leading as f32 * WIDTH;
    let trail = trailing as f32 * WIDTH;
    match x {
        x if leading > 0 && x > lead * FULL.max(1.0) => Rest::RunLeading,
        x if trailing > 0 && x < -(trail.max(WIDTH) * FULL) => Rest::RunTrailing,
        x if leading > 0 && x > lead / 2.0 => Rest::Leading,
        x if trailing > 0 && x < -trail / 2.0 => Rest::Trailing,
        _ => Rest::Closed,
    }
}

/// `content` with `leading` actions behind its left edge and `trailing`
/// behind its right, the first of each the outermost. `open` is called for
/// a tap on the row while it rests closed.
pub fn row(
    content: impl IntoElement,
    leading: Vec<Action>,
    trailing: Vec<Action>,
    open: impl Fn() + 'static,
) -> impl IntoElement {
    let x = animated_value(0.0);
    let shown = signal(Rest::Closed);
    let (lead_n, trail_n) = (leading.len(), trailing.len());
    let close = {
        move || {
            shown.set(Rest::Closed);
            drop(x.spring(0.0, spring()));
        }
    };
    let side = |actions: Vec<Action>, trailing: bool| {
        view()
            .absolute()
            .top_0()
            .bottom_0()
            .flex_row()
            .when(trailing, |v| v.right_0())
            .when(!trailing, |v| v.left_0())
            .children(
                // The first action sits at the outer edge.
                (if trailing {
                    actions.into_iter().rev().collect::<Vec<_>>()
                } else {
                    actions
                })
                .into_iter()
                .map(move |action| {
                    view()
                        .w(px(WIDTH))
                        .items_center()
                        .justify_center()
                        .bg(action.tint)
                        .role(AccessibilityRole::Button)
                        .label(action.label)
                        .on_press(move || {
                            close();
                            (action.run)();
                        })
                        .child(
                            text(action.label)
                                .text_size(px(15.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(0xffffff)),
                        )
                        .into_element()
                }),
            )
    };
    let first_lead = leading.first().map(|a| a.run.clone());
    let first_trail = trailing.first().map(|a| a.run.clone());
    let ended = {
        move |event: &PanEvent| {
            let at = event.value_x.unwrap_or(event.translation.x.0);
            let to = rest(at, lead_n, trail_n);
            let target = match to {
                Rest::Leading => lead_n as f32 * WIDTH,
                Rest::Trailing => -(trail_n as f32 * WIDTH),
                Rest::Closed | Rest::RunLeading | Rest::RunTrailing => 0.0,
            };
            shown.set(match to {
                Rest::Leading | Rest::Trailing => to,
                _ => Rest::Closed,
            });
            drop(x.spring(target, spring()));
            let run = match to {
                Rest::RunLeading => first_lead.clone(),
                Rest::RunTrailing => first_trail.clone(),
                _ => None,
            };
            if let Some(run) = run {
                haptic(Haptic::Impact(HapticImpact::Medium));
                run();
            }
        }
    };
    let tap = {
        move || {
            if shown.get_untracked() == Rest::Closed {
                open();
            } else {
                close();
            }
        }
    };
    view()
        .overflow_hidden()
        .child(side(leading, false))
        .child(side(trailing, true))
        .child(
            view()
                .bg(super::parts::colors::chat())
                .pan_values(Some(&x), None)
                .bind_translate_x(&x)
                .on_pan_end(ended)
                .on_press(tap)
                .child(content),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drag_rests_open_past_half_and_runs_the_first_action_when_long() {
        assert_eq!(rest(10.0, 1, 2), Rest::Closed);
        assert_eq!(rest(-80.0, 1, 2), Rest::Trailing);
        assert_eq!(rest(-400.0, 1, 2), Rest::RunTrailing);
        assert_eq!(rest(50.0, 1, 2), Rest::Leading);
        assert_eq!(rest(200.0, 1, 2), Rest::RunLeading);
        assert_eq!(rest(200.0, 0, 2), Rest::Closed, "no leading actions");
    }
}
