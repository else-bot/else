use super::ComputerView;
use crate::api::MAX_TEXT_CHARS;
use gpui_kit::*;
use std::ops::Range;

/// Keys the macOS text system composes and commits through
/// [`EntityInputHandler`]. Option stays with composition, so Option+E starts
/// an accent rather than sending guest Alt+E; Control and Command keep their
/// raw shortcuts.
pub(super) fn is_text(stroke: &Keystroke) -> bool {
    !stroke.modifiers.control
        && !stroke.modifiers.platform
        && !stroke.modifiers.function
        && stroke
            .key_char
            .as_ref()
            .is_some_and(|text| text.chars().all(|ch| !ch.is_control()))
}

impl EntityInputHandler for ComputerView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let text: Vec<u16> = self
            .marked_text
            .as_deref()
            .unwrap_or("")
            .encode_utf16()
            .collect();
        let range = range.start.min(text.len())..range.end.min(text.len());
        let value = String::from_utf16(text.get(range.clone())?).ok()?;
        *adjusted = Some(range);
        Some(value)
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        self.controlling().then(|| {
            let end = self
                .marked_text
                .as_deref()
                .unwrap_or("")
                .encode_utf16()
                .count();
            UTF16Selection {
                range: end..end,
                reversed: false,
            }
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_text
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.marked_text.take() {
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = None;
        if !self.controlling() {
            return;
        }
        if text.chars().count() > MAX_TEXT_CHARS {
            self.notice = Some(format!(
                "Text longer than {MAX_TEXT_CHARS} characters was not typed."
            ));
            cx.notify();
            return;
        }
        self.sync_modifiers(Modifiers::default(), cx);
        if self
            .commands
            .as_ref()
            .is_some_and(|commands| !commands.type_text(text))
        {
            self.fail("Connection interrupted. Click to reconnect.".into(), cx);
            self.commands = None;
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
        if self.controlling() {
            self.marked_text = (!text.is_empty()).then(|| text.to_owned());
        }
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(Bounds::new(bounds.origin, size(px(1.), px(20.))))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(0)
    }
}
