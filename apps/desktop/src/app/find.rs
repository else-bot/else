//! Find in the open conversation: the server counts and orders the matches,
//! and the transcript loads only a window around the one shown.

use super::ElseApp;
use super::row_text::{Highlights, RowText};
use super::transcript::RowKey;
use crate::controls::{Control as _, Scale, Tier, text_field};
use crate::model::{GapSize, SearchHit};
use else_plain_text::find::Needle;
use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::base::input::InputEvent;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape, Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex};
use gpui_kit::*;
use std::time::Duration;
use uuid::Uuid;

/// How long typing must pause before the search runs.
const DEBOUNCE: Duration = Duration::from_millis(150);
/// Shown items loaded on each side of a match that is not loaded yet.
const NEIGHBOURS: i64 = 5;
/// Shown items one click on a gap row loads.
const GAP_PAGE: i64 = 50;

pub(super) struct Find {
    input: Entity<InputState>,
    phase: Phase,
    /// The row to bring near the middle of the list once it is laid out,
    /// and how far below the list's top to put it.
    centering: Option<(usize, Pixels)>,
    _input: Subscription,
}

enum Phase {
    /// Nothing to search for.
    Empty,
    /// Waiting for the typing to pause, then for the server.
    Searching {
        _task: Task<()>,
    },
    Found(Found),
    Failed,
}

struct Found {
    needle: Needle,
    hits: Vec<SearchHit>,
    total: usize,
    /// The match shown, counted from the newest.
    current: usize,
    /// Whether the transcript has scrolled the current match into view.
    landed: bool,
    /// Loading a window of messages around the current match.
    _load: Option<Task<()>>,
}

/// The message holding match `index`, counted from the newest, and which of
/// its matches that is, counted from its end.
fn locate(hits: &[SearchHit], index: usize) -> Option<(SearchHit, usize)> {
    let mut left = index;
    for hit in hits {
        if left < hit.count {
            return Some((*hit, left));
        }
        left -= hit.count;
    }
    None
}

/// The match after `current` toward older messages, or toward newer ones,
/// wrapping at either end.
fn step(current: usize, total: usize, older: bool) -> usize {
    match (older, current) {
        (true, _) => (current + 1) % total,
        (false, 0) => total - 1,
        (false, _) => current - 1,
    }
}

impl Found {
    fn target(&self) -> Option<(SearchHit, usize)> {
        locate(&self.hits, self.current)
    }

    fn label(&self) -> String {
        if self.total == 0 {
            "No matches".into()
        } else {
            format!("{} of {}", self.current + 1, self.total)
        }
    }
}

impl ElseApp {
    pub(super) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let input = match &view.find {
            Some(find) => find.input.clone(),
            None => {
                let input =
                    cx.new(|cx| InputState::new(window, cx).placeholder("Find in conversation"));
                let subscription =
                    cx.subscribe_in(&input, window, |app, _, event: &InputEvent, window, cx| {
                        match event {
                            InputEvent::Change => app.find_changed(window, cx),
                            InputEvent::PressEnter { shift, .. } => {
                                app.step_find(!shift, window, cx)
                            }
                            _ => {}
                        }
                    });
                view.find = Some(Find {
                    input: input.clone(),
                    phase: Phase::Empty,
                    centering: None,
                    _input: subscription,
                });
                input
            }
        };
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        cx.notify();
    }

    /// Closes the find bar, clears its highlights, and returns to the
    /// latest messages. Loaded windows stay.
    pub(super) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        if view.find.take().is_none() {
            return;
        }
        self.paint_quotes(cx);
        self.jump_to_latest(window, cx);
        self.composer
            .update(cx, |composer, cx| composer.focus(window, cx));
    }

    fn find_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let bot = view.bot_id;
        let Some(find) = view.find.as_mut() else {
            return;
        };
        let query = find.input.read(cx).value().to_string();
        let Some(needle) = Needle::new(&query) else {
            find.phase = Phase::Empty;
            self.paint_quotes(cx);
            cx.notify();
            return;
        };
        find.phase = Phase::Searching {
            _task: cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(DEBOUNCE).await;
                let hits = client.search(bot, query).await;
                let _ = this.update_in(cx, |app, window, cx| {
                    let Some(find) = app.view_of(bot).and_then(|view| view.find.as_mut()) else {
                        return;
                    };
                    find.phase = match hits {
                        Ok(hits) => Phase::Found(Found {
                            needle,
                            hits: hits.hits,
                            total: hits.total,
                            current: 0,
                            landed: false,
                            _load: None,
                        }),
                        Err(_) => Phase::Failed,
                    };
                    if app.shows(bot) {
                        app.show_match(window, cx);
                    }
                });
            }),
        };
        cx.notify();
    }

    /// Moves to the next match toward older messages, or toward newer ones.
    pub(super) fn step_find(&mut self, older: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Phase::Found(found)) = self
            .view
            .as_mut()
            .and_then(|view| view.find.as_mut())
            .map(|find| &mut find.phase)
        else {
            return;
        };
        if found.total == 0 {
            return;
        }
        found.current = step(found.current, found.total, older);
        found.landed = false;
        found._load = None;
        self.show_match(window, cx);
    }

    /// Shows the current match of a view shown again, if it was found or
    /// its messages loaded while the view was warm.
    pub(super) fn resume_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let unlanded = self
            .view
            .as_ref()
            .and_then(|view| view.find.as_ref())
            .is_some_and(|find| matches!(&find.phase, Phase::Found(found) if !found.landed));
        if unlanded {
            self.show_match(window, cx);
        }
    }

    /// Scrolls to the current match, unfolding what hides it, or first
    /// loads a window of messages around it.
    fn show_match(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let bot = view.bot_id;
        let (Some(conversation), Some(find)) = (view.conversation.as_ref(), view.find.as_mut())
        else {
            return;
        };
        let Phase::Found(found) = &mut find.phase else {
            self.paint_quotes(cx);
            cx.notify();
            return;
        };
        let Some((hit, _)) = found.target() else {
            self.paint_quotes(cx);
            cx.notify();
            return;
        };
        if conversation.item(hit.item_id).is_none() {
            let around = conversation.loaded_around(hit.seq);
            found._load = Some(cx.spawn_in(window, async move |this, cx| {
                let loaded = client
                    .window(bot, hit.seq, (NEIGHBOURS, NEIGHBOURS), around)
                    .await;
                let _ = this.update_in(cx, |app, window, cx| {
                    let Some(view) = app.view.as_mut().filter(|view| view.bot_id == bot) else {
                        return;
                    };
                    match loaded {
                        Ok(loaded) => {
                            let merged = view.conversation.as_mut().is_some_and(|conversation| {
                                conversation.merge_window(loaded, around.0, around.1)
                            });
                            if !merged {
                                // Another load changed what is loaded there;
                                // ask again around the match.
                                app.show_match(window, cx);
                                return;
                            }
                            app.sync_rows(window, cx);
                            app.land_on(hit.item_id, window, cx);
                        }
                        Err(err) => {
                            app.notice = Some(format!("Could not load that message: {err}"));
                            cx.notify();
                        }
                    }
                });
            }));
            cx.notify();
            return;
        }
        self.land_on(hit.item_id, window, cx);
    }

    fn land_on(&mut self, item: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.unfold_item(item, window, cx)
            && let Some(view) = self.view.as_mut()
        {
            view.list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.),
            });
            let lead = view.list.viewport_bounds().size.height * 0.4;
            if let Some(find) = view.find.as_mut() {
                find.centering = Some((ix, lead));
            }
        }
        self.paint_quotes(cx);
        cx.notify();
    }

    /// Loads the newest page of the unloaded stretch just below the loaded
    /// item `above`.
    pub(super) fn load_gap(&mut self, above: i64, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let Some(conversation) = view.conversation.as_ref() else {
            return;
        };
        if view.loading_gap.is_some() {
            return;
        }
        let bot = view.bot_id;
        let (below, _) = conversation.loaded_around(above);
        let task = cx.spawn_in(window, async move |this, cx| {
            let page = client
                .window(bot, above - 1, (GAP_PAGE, 0), (below, Some(above)))
                .await;
            let _ = this.update_in(cx, |app, window, cx| {
                let Some(view) = app.view.as_mut().filter(|view| view.bot_id == bot) else {
                    return;
                };
                view.loading_gap = None;
                match page {
                    Ok(page) => {
                        let Some(conversation) = view.conversation.as_mut() else {
                            return;
                        };
                        if conversation.merge_window(page, below, Some(above)) {
                            app.sync_rows(window, cx);
                        } else if conversation.gap_below(above).is_some() {
                            // Another load changed what is loaded there.
                            app.load_gap(above, window, cx);
                        }
                    }
                    Err(err) => {
                        app.notice = Some(format!("Could not load earlier messages: {err}"));
                        cx.notify();
                    }
                }
            });
        });
        view.loading_gap = Some((above, task));
        cx.notify();
    }

    pub(super) fn render_gap(&self, above: i64, cx: &mut Context<Self>) -> AnyElement {
        let view = self.view.as_ref();
        let size = view
            .and_then(|view| view.conversation.as_ref())
            .and_then(|c| c.gap_below(above))
            .unwrap_or(GapSize::Uncounted);
        let loading =
            view.is_some_and(|view| matches!(view.loading_gap, Some((at, _)) if at == above));
        let label = match (loading, size) {
            (true, _) => "Loading…".to_owned(),
            (false, GapSize::Counted(1)) => "… 1 earlier message …".to_owned(),
            (false, GapSize::Counted(count)) => format!("… {count} earlier messages …"),
            (false, GapSize::Uncounted) => "… earlier messages …".to_owned(),
        };
        h_flex()
            .w_full()
            .justify_center()
            .py_1()
            .child(
                Button::new(ElementId::Name(format!("gap-{above}").into()))
                    .control(Tier::Ghost, Scale::Compact, loading, cx)
                    .label(label)
                    .on_click(
                        cx.listener(move |app, _, window, cx| app.load_gap(above, window, cx)),
                    ),
            )
            .into_any_element()
    }

    /// Wraps row `ix` so that, the first time it is laid out as the row to
    /// center, the list scrolls it about 40% down the viewport. The list
    /// measures the rows above it as it walks back.
    pub(super) fn center_row(&self, ix: usize, row: Div, cx: &mut Context<Self>) -> Div {
        let Some((_, lead)) = self
            .view
            .as_ref()
            .and_then(|view| view.find.as_ref()?.centering)
            .filter(|(row, _)| *row == ix)
        else {
            return row;
        };
        let owner = cx.entity().downgrade();
        row.on_prepaint(move |bounds, window, cx| {
            window.request_autoscroll(Bounds::new(
                point(bounds.origin.x, bounds.origin.y - lead),
                size(bounds.size.width, px(1.)),
            ));
            window.defer(cx, move |_, cx| {
                let _ = owner.update(cx, |app, _| {
                    if let Some(find) = app.view.as_mut().and_then(|view| view.find.as_mut()) {
                        find.centering.take_if(|(row, _)| *row == ix);
                    }
                });
            });
        })
    }

    pub(super) fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let find = self.view.as_ref()?.find.as_ref()?;
        let theme = cx.theme().clone();
        let (label, can_step) = match &find.phase {
            Phase::Empty => (String::new(), false),
            Phase::Searching { .. } => ("…".into(), false),
            Phase::Found(found) => (found.label(), found.total > 0),
            Phase::Failed => ("Search failed".into(), false),
        };
        let arrow = |id: &'static str, icon: IconName, name: &'static str, older: bool| {
            Button::new(id)
                .focus_ring(false)
                .cursor_pointer()
                .icon(icon)
                .small()
                .ghost()
                .disabled(!can_step)
                .accessibility_label(name)
                .on_click(cx.listener(move |app, _, window, cx| app.step_find(older, window, cx)))
        };
        Some(
            h_flex()
                .key_context("FindBar")
                .capture_action(cx.listener(|app, _: &Escape, window, cx| {
                    app.close_find(window, cx);
                    cx.stop_propagation();
                }))
                .absolute()
                .top_2()
                .right_5()
                .gap_1()
                .items_center()
                .p_1()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .shadow_md()
                .child(
                    text_field(Input::new(&find.input), cx)
                        .small()
                        .w(px(220.))
                        .prefix(Icon::new(IconName::Search).small())
                        .aria_label("Find in conversation"),
                )
                .child(
                    div()
                        .min_w(px(72.))
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(arrow(
                    "find-older",
                    IconName::ChevronUp,
                    "Older match",
                    true,
                ))
                .child(arrow(
                    "find-newer",
                    IconName::ChevronDown,
                    "Newer match",
                    false,
                ))
                .child(
                    Button::new("find-close")
                        .focus_ring(false)
                        .cursor_pointer()
                        .icon(IconName::Close)
                        .small()
                        .ghost()
                        .accessibility_label("Close find")
                        .on_click(cx.listener(|app, _, window, cx| app.close_find(window, cx))),
                )
                .into_any_element(),
        )
    }
}

/// The find bar's highlights on one row: every match softly, the current
/// one strongly, scrolling the list to the current one the first time it
/// is painted.
pub(super) fn highlights(
    find: Option<&mut Find>,
    key: RowKey,
    text: &RowText,
    cx: &mut App,
) -> Highlights {
    let Some(Phase::Found(found)) = find.map(|find| &mut find.phase) else {
        return Vec::new();
    };
    let ranges = text.find(&found.needle, cx);
    let current = found
        .target()
        .filter(|(hit, _)| key == RowKey::Item(hit.item_id))
        .and_then(|(_, from_end)| ranges.len().checked_sub(from_end + 1));
    let theme = cx.theme();
    let (soft, strong) = (theme.warning.opacity(0.28), theme.warning.opacity(0.65));
    let mut painted = Vec::with_capacity(ranges.len());
    for (ix, range) in ranges.into_iter().enumerate() {
        if Some(ix) == current {
            if !found.landed {
                found.landed = text.reveal(&range, cx);
            }
            painted.push((range, strong));
        } else {
            painted.push((range, soft));
        }
    }
    painted
}

#[cfg(test)]
mod tests {
    use super::{locate, step};
    use crate::model::SearchHit;
    use uuid::Uuid;

    fn hit(id: u128, count: usize) -> SearchHit {
        SearchHit {
            item_id: Uuid::from_u128(id),
            seq: id as i64,
            count,
        }
    }

    #[test]
    fn matches_count_from_the_newest_message_and_from_its_end() {
        let hits = [hit(9, 2), hit(5, 1), hit(2, 3)];
        let at = |index| locate(&hits, index).map(|(hit, from_end)| (hit.seq, from_end));
        assert_eq!(at(0), Some((9, 0)));
        assert_eq!(at(1), Some((9, 1)));
        assert_eq!(at(2), Some((5, 0)));
        assert_eq!(at(5), Some((2, 2)));
        assert_eq!(at(6), None);
    }

    #[test]
    fn enter_walks_to_older_matches_and_wraps() {
        assert_eq!(step(0, 3, true), 1);
        assert_eq!(step(2, 3, true), 0);
        assert_eq!(step(0, 3, false), 2);
        assert_eq!(step(2, 3, false), 1);
    }
}
