//! Going to the message a reply quotes, and the highlight that flashes it.

use super::ConversationView;
use crate::app::reply::{self, Author};
use crate::app::row_text::RowText;
use crate::app::transcript::RowKey;
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;
use std::time::Duration;

/// A message the owner jumped to, flashed until it fades out.
pub(in crate::app) struct Reveal {
    /// The row that holds it.
    pub(in crate::app) row: RowKey,
    pub(in crate::app) at: std::time::Instant,
    pub(in crate::app) spot: Spot,
    /// How strongly the highlight paints, as of the last tick, so every
    /// paint until the next tick gives the same color.
    pub(in crate::app) shown: f32,
}

pub(in crate::app) enum Spot {
    /// The whole row, as for a card or a plain-text message.
    Row,
    /// The quoted text of a Markdown message.
    Passage {
        item: RowKey,
        /// The quoted part, or `None` for the whole message.
        quote: Option<String>,
        /// Whether the list was asked to center the passage, which waits for
        /// the message's Markdown to parse.
        centered: bool,
    },
}

/// How long a revealed message stays highlighted before it starts to fade.
const REVEAL_HOLD: Duration = Duration::from_millis(1200);
/// How long its highlight takes to fade out.
const REVEAL_FADE: Duration = Duration::from_millis(1600);

impl Reveal {
    pub(in crate::app) fn new(row: RowKey, spot: Spot) -> Self {
        Self {
            row,
            at: std::time::Instant::now(),
            spot,
            shown: 1.,
        }
    }

    /// How strongly to paint the highlight now, from 1 down to 0 once it
    /// has faded out.
    fn strength(&self) -> f32 {
        let fading = self.at.elapsed().saturating_sub(REVEAL_HOLD);
        let t = (fading.as_secs_f32() / REVEAL_FADE.as_secs_f32()).min(1.);
        1. - t * t * (3. - 2. * t)
    }
}

impl ConversationView {
    /// Scrolls to the message a reply quotes, loading older pages until it
    /// is found or the history runs out.
    pub(in crate::app) fn reveal_quoted(
        &mut self,
        from: RowKey,
        author: Author,
        original: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_reveal = None;
        let Some(conversation) = self.conversation.as_ref() else {
            return;
        };
        let before = match from {
            RowKey::Item(id) => conversation.item(id).map(|item| item.seq),
            _ => None,
        };
        // A whole-message match anywhere beats a partial one in a later message.
        let target = [reply::quotes_whole, reply::quotes_part]
            .into_iter()
            .find_map(|quotes| {
                conversation
                    .items()
                    .rev()
                    .filter(|item| before.is_none_or(|seq| item.seq < seq))
                    .find(|item| {
                        Author::of(item.kind) == Some(author) && quotes(&item.text, original)
                    })
            })
            .map(|item| item.id);
        let older = conversation.older_before_seq.is_some();
        let found = target.and_then(|target| Some((target, self.unfold_item(target, window, cx)?)));
        if let Some((target, ix)) = found
            && let Some(conversation) = self.conversation.as_ref()
        {
            self.list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.),
            });
            let item = RowKey::Item(target);
            let spot = if self.texts.get(&item).is_some_and(RowText::is_markdown) {
                let whole = conversation
                    .item(target)
                    .is_some_and(|item| reply::quotes_whole(&item.text, original));
                Spot::Passage {
                    item,
                    quote: (!whole).then(|| original.to_owned()),
                    centered: false,
                }
            } else {
                Spot::Row
            };
            let row = self.rows[ix].key;
            self.start_reveal(Reveal::new(row, spot), cx);
        } else if older {
            self.pending_reveal = Some((from, author, original.to_owned()));
            self.load_older(window, cx);
        } else {
            window.push_notification(
                "The original message is no longer in this conversation.",
                cx,
            );
        }
    }

    /// Flashes `reveal` until it fades out, painting its passage on each
    /// frame and asking the list to center the passage once it is found.
    pub(in crate::app) fn start_reveal(&mut self, reveal: Reveal, cx: &mut Context<Self>) {
        let previous = self.revealed.replace(reveal);
        if let Some(Reveal {
            spot: Spot::Passage { item, .. },
            ..
        }) = previous
        {
            self.paint_row_quotes(item, cx);
        }
        self.tick_reveal(cx);
        self._reveal_clock = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let live = this.update(cx, |view, cx| view.tick_reveal(cx));
                if !live.unwrap_or(false) {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Repaints the revealed passage, and drops the reveal once it has faded
    /// out. Returns whether it is still showing.
    fn tick_reveal(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(reveal) = self.revealed.as_mut() else {
            return false;
        };
        reveal.shown = reveal.strength();
        let live = reveal.shown > 0.;
        let passage = match &reveal.spot {
            Spot::Passage { item, .. } => Some(*item),
            Spot::Row => None,
        };
        if !live {
            self.revealed = None;
        }
        match passage {
            Some(item) => self.paint_row_quotes(item, cx),
            None => cx.notify(),
        }
        live
    }
}

#[cfg(test)]
mod tests {
    use super::{REVEAL_FADE, REVEAL_HOLD, Reveal, RowKey, Spot};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    fn reveal_from(ago: Duration) -> Reveal {
        Reveal {
            row: RowKey::Item(Uuid::nil()),
            at: Instant::now() - ago,
            spot: Spot::Row,
            shown: 1.,
        }
    }

    #[test]
    fn a_reveal_holds_its_highlight_then_fades_it_out() {
        assert_eq!(reveal_from(Duration::ZERO).strength(), 1.);
        assert_eq!(reveal_from(REVEAL_HOLD / 2).strength(), 1.);
        let fading = reveal_from(REVEAL_HOLD + REVEAL_FADE / 2).strength();
        assert!(
            fading > 0. && fading < 1.,
            "half way through the fade: {fading}"
        );
        assert_eq!(reveal_from(REVEAL_HOLD + REVEAL_FADE).strength(), 0.);
    }
}
