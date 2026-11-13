//! The two shared shapes for conversation items that are not messages:
//! action cards (approvals, questions, credentials, drafts, notices) and
//! one-line activity rows (delegations, created bots, rules, routines).
//!
//! An action card is a hairline frame on the page, with no fill of its own.
//! Its icon sits inline with the title, and the body blocks and footer below
//! start on the icon's left edge, so the card has one left edge and no
//! indent column. Status lives in the sidebar and the paused strip, so a
//! card carries a tag only for a fact found nowhere else, such as "Hard to
//! undo".

use crate::controls::{Control as _, Scale, Tier};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The icon column every activity row leads with.
const LEAD: f32 = 20.;
/// The line height of a card title; the icon centres on it.
const TITLE_LINE: f32 = 22.;

#[derive(IntoElement)]
pub(super) struct ActionCard {
    icon: IconName,
    title: AnyElement,
    source: SharedString,
    meta: Option<SharedString>,
    tag: Option<SharedString>,
    body: Vec<AnyElement>,
    footer: Option<AnyElement>,
}

pub(super) fn card(
    icon: IconName,
    title: impl Into<SharedString>,
    source: impl Into<SharedString>,
) -> ActionCard {
    titled(icon, title.into(), source)
}

/// A card whose title is an element, such as an editable subject line. The
/// card gives it the title's size and weight.
pub(super) fn titled(
    icon: IconName,
    title: impl IntoElement,
    source: impl Into<SharedString>,
) -> ActionCard {
    ActionCard {
        icon,
        title: title.into_any_element(),
        source: source.into(),
        meta: None,
        tag: None,
        body: Vec::new(),
        footer: None,
    }
}

impl ActionCard {
    /// A short fact beside the title that nothing else on screen says.
    pub(super) fn tag(mut self, tag: impl Into<SharedString>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Quiet meta, such as how long it has waited, right-aligned on the
    /// title row.
    pub(super) fn meta(mut self, meta: impl Into<SharedString>) -> Self {
        self.meta = Some(meta.into());
        self
    }

    /// The card's last row; it always renders below every body block.
    pub(super) fn footer(mut self, footer: impl IntoElement) -> Self {
        self.footer = Some(footer.into_any_element());
        self
    }
}

impl ParentElement for ActionCard {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.body.extend(elements);
    }
}

impl RenderOnce for ActionCard {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let heading = h_flex()
            .w_full()
            .min_w_0()
            .items_start()
            .gap_3()
            .child(
                h_flex()
                    .h(px(TITLE_LINE))
                    .flex_none()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(self.icon).size_4()),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(15.))
                            .line_height(px(TITLE_LINE))
                            .font_weight(FontWeight::SEMIBOLD)
                            .whitespace_normal()
                            .child(self.title),
                    )
                    // The bot's name row sits right above the card, so the
                    // source line is only for facts it does not repeat.
                    .when(!self.source.is_empty(), |heading| {
                        heading.child(
                            div()
                                .text_size(px(13.))
                                .text_color(theme.muted_foreground)
                                .whitespace_normal()
                                .child(self.source),
                        )
                    }),
            )
            .children(self.meta.map(|meta| {
                div()
                    .flex_none()
                    .h(px(TITLE_LINE))
                    .flex()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(theme.muted_foreground)
                    .child(meta)
            }))
            .children(self.tag.map(|tag| {
                div()
                    .flex_none()
                    .px_2()
                    .py_0p5()
                    .rounded(theme.radius)
                    .bg(theme.warning.opacity(0.1))
                    .text_xs()
                    .text_color(theme.warning)
                    .child(tag)
            }));
        v_flex()
            .w_full()
            .min_w_0()
            .gap_3()
            .p_5()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .child(heading)
            .children(self.body)
            .children(
                self.footer
                    .map(|footer| div().w_full().pt_1().child(footer)),
            )
    }
}

/// A card's footer row: optional helper text on the left, then the buttons
/// on the right.
pub(super) fn footer() -> Div {
    h_flex()
        .w_full()
        .min_w_0()
        .justify_end()
        .items_center()
        .flex_wrap()
        .gap_2()
}

/// The helper text that leads a footer and takes its free width.
pub(super) fn footer_note(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .flex_1()
        .min_w(px(160.))
        .text_xs()
        .whitespace_normal()
        .text_color(theme.muted_foreground)
        .child(text.into())
}

/// A card's body text: the exact command, request, or reason, on the
/// card's one left edge.
pub(super) fn body_text(text: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .min_w_0()
        .text_sm()
        .line_height(relative(1.5))
        .whitespace_normal()
        .child(text.into())
}

/// The height of a selectable option row inside a card.
pub(super) const OPTION_HEIGHT: f32 = 44.;

/// One activity row: a 36 px line with the icon column, then actor, verb
/// and object, then right-aligned meta and quiet actions.
pub(super) fn event_row(theme: &Theme) -> Div {
    h_flex()
        .w_full()
        .min_w_0()
        .h(px(36.))
        .items_center()
        .gap_2()
        .px_2()
        .rounded(theme.radius)
        .text_size(px(13.))
        .text_color(theme.muted_foreground)
        .hover(|style| style.bg(theme.muted.opacity(0.5)))
}

/// The fixed icon column of an activity row. Two faces overlap diagonally
/// so the column keeps its width.
pub(super) fn event_lead(faces: Vec<AnyElement>) -> Div {
    let lead = div().relative().flex_none().size(px(LEAD));
    let mut faces = faces.into_iter();
    match (faces.next(), faces.next()) {
        (Some(first), Some(second)) => lead
            .child(div().absolute().top_0().left_0().child(first))
            .child(div().absolute().bottom_0().right_0().child(second)),
        (Some(only), None) => lead.flex().items_center().justify_center().child(only),
        _ => lead,
    }
}

/// The size of one face in [`event_lead`], given how many it shows.
pub(super) fn event_face_size(count: usize) -> Pixels {
    if count > 1 { px(14.) } else { px(16.) }
}

impl super::ElseApp {
    /// The icon column for the bots taking part in an activity row.
    pub(super) fn event_faces(&self, key: &str, ids: impl IntoIterator<Item = uuid::Uuid>) -> Div {
        let mut shown: Vec<&crate::api::Bot> = Vec::new();
        for id in ids {
            if shown.len() < 2
                && !shown.iter().any(|bot| bot.id == id)
                && let Some(bot) = self.bots.iter().find(|bot| bot.id == id)
            {
                shown.push(bot);
            }
        }
        let size = event_face_size(shown.len());
        event_lead(
            shown
                .into_iter()
                .map(|bot| {
                    crate::avatar::bot_avatar(
                        SharedString::from(format!("{key}-{}", bot.id)),
                        bot.avatar,
                        size,
                    )
                    .still()
                    .into_any_element()
                })
                .collect(),
        )
    }
}

/// An activity row's icon column holding a plain icon.
pub(super) fn event_icon(icon: IconName) -> Div {
    event_lead(vec![Icon::new(icon).size_4().into_any_element()])
}

/// What they did: secondary.
pub(super) fn event_verb(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .text_color(theme.muted_foreground)
        .child(text.into())
}

/// What it was done to: full strength, taking the free width.
pub(super) fn event_object(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .truncate()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.foreground)
        .child(text.into())
}

/// A quiet action at the end of an activity row: a compact ghost button
/// whose label matches the row's 13 px text. Every trailing element ends on
/// the edge this button's label ends on.
pub(super) fn event_action(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    disabled: bool,
    cx: &App,
) -> Button {
    Button::new(id)
        .control(Tier::Ghost, Scale::Compact, disabled, cx)
        .child(div().text_size(px(13.)).child(label.into()))
}

/// A trailing disclosure chevron, inset to end where an action's label ends.
pub(super) fn event_chevron(expanded: bool) -> Div {
    div().flex_none().mr(Scale::Compact.padding()).child(
        Icon::new(if expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .xsmall(),
    )
}

/// Right-aligned meta: status, count, time. Same size as the row, quieter
/// than its verb.
pub(super) fn event_meta(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .text_size(px(13.))
        .text_color(if theme.mode.is_dark() {
            theme.foreground.opacity(0.55)
        } else {
            super::message_row::meta_color(theme)
        })
        .child(text.into())
}

pub(super) fn failure(theme: &Theme, text: String) -> Div {
    h_flex()
        .w_full()
        .min_w_0()
        .items_start()
        .gap_2()
        .child(
            Icon::new(IconName::TriangleAlert)
                .size_4()
                .text_color(theme.danger),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.danger)
                        .child("Failed"),
                )
                .child(
                    div()
                        .text_sm()
                        .whitespace_normal()
                        .text_color(theme.foreground)
                        .child(text),
                ),
        )
}
