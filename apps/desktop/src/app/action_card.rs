//! The two shared shapes for conversation items that are not messages:
//! action cards (approvals, questions, credentials, drafts, notices) and
//! one-line activity rows (delegations, created bots, rules, routines).
//!
//! An action card has one grid: a 20 px icon column, a 12 px gap, and one
//! text column that holds the title, the source line, every body block and
//! the footer, so all of them share one left edge. Status lives in the
//! sidebar and the paused strip, so a card carries a tag only for a fact
//! found nowhere else, such as "Hard to undo".

use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Icon, Theme, h_flex, v_flex};
use gpui_kit::*;

/// The icon column every action card and activity row leads with.
const LEAD: f32 = 20.;

#[derive(IntoElement)]
pub(super) struct ActionCard {
    icon: IconName,
    title: SharedString,
    source: SharedString,
    tag: Option<SharedString>,
    body: Vec<AnyElement>,
    footer: Option<AnyElement>,
}

pub(super) fn card(
    icon: IconName,
    title: impl Into<SharedString>,
    source: impl Into<SharedString>,
) -> ActionCard {
    ActionCard {
        icon,
        title: title.into(),
        source: source.into(),
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
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(15.))
                            .line_height(px(22.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .whitespace_normal()
                            .child(self.title),
                    )
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme.muted_foreground)
                            .whitespace_normal()
                            .child(self.source),
                    ),
            )
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
        h_flex()
            .w_full()
            .min_w_0()
            .items_start()
            .gap_3()
            .p_4()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(
                h_flex()
                    .w(px(LEAD))
                    .h(px(22.))
                    .flex_none()
                    .justify_center()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(self.icon).size_4()),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .child(heading)
                    .children(self.body)
                    .children(
                        self.footer
                            .map(|footer| div().w_full().pt_1().child(footer)),
                    ),
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

/// Quoted context inside a card: the exact command, request, or reason,
/// set off by a rule instead of a darker box.
pub(super) fn quote(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .min_w_0()
        .pl_3()
        .border_l_2()
        .border_color(theme.border)
        .text_sm()
        .line_height(relative(1.6))
        .whitespace_normal()
        .child(text.into())
}

/// The height of a selectable option row inside a card.
pub(super) const OPTION_HEIGHT: f32 = 40.;

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

/// Who acted: full strength.
pub(super) fn event_actor(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .min_w_0()
        .truncate()
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.foreground)
        .child(text.into())
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

/// Right-aligned meta: status, count, time.
pub(super) fn event_meta(theme: &Theme, text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .text_xs()
        .text_color(theme.muted_foreground)
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
