use crate::api::Bot;
use crate::app::ElseApp;
use crate::app::panel::{hairline, note};
use crate::avatar::{Avatar, AvatarPicker, bot_avatar};
use crate::controls::{Control as _, Scale, Tier, text_field};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{
    ActiveTheme as _, FocusableExt as _, Icon, Sizable as _, Size, h_flex, v_flex,
};
use gpui_kit::*;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Metadata {
    name: String,
    avatar: Avatar,
}

impl Metadata {
    fn of(bot: &Bot) -> Self {
        Self {
            name: bot.name.clone(),
            avatar: bot.avatar,
        }
    }
}

fn parse_name(text: &str) -> Result<String, &'static str> {
    let name = text.trim();
    if name.is_empty() {
        Err("Give the bot a name.")
    } else if name.chars().count() > 80 {
        Err("Keep the name to 80 characters or fewer.")
    } else {
        Ok(name.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq)]
struct MetadataSync {
    seen: Metadata,
    revision: Option<i64>,
    saved: Metadata,
    desired: Metadata,
    sending: Option<Metadata>,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct MetadataChange {
    name: Option<String>,
    avatar: Option<Avatar>,
}

impl MetadataSync {
    fn new(values: Metadata, revision: Option<i64>) -> Self {
        Self {
            seen: values.clone(),
            revision,
            saved: values.clone(),
            desired: values,
            sending: None,
            error: None,
        }
    }

    fn begin(&mut self) -> Option<MetadataChange> {
        if self.sending.is_some() || self.desired == self.saved {
            return None;
        }
        let change = MetadataChange {
            name: (self.desired.name != self.saved.name).then(|| self.desired.name.clone()),
            avatar: (self.desired.avatar != self.saved.avatar).then_some(self.desired.avatar),
        };
        self.sending = Some(self.desired.clone());
        self.error = None;
        Some(change)
    }

    fn finish(&mut self, result: Result<(Metadata, Option<i64>), String>) {
        let Some(sent) = self.sending.take() else {
            return;
        };
        match result {
            Ok((server, revision)) => {
                let server = if revision < self.revision {
                    self.saved.clone()
                } else {
                    self.revision = revision;
                    server
                };
                if self.desired.name == sent.name {
                    self.desired.name = server.name.clone();
                }
                if self.desired.avatar == sent.avatar {
                    self.desired.avatar = server.avatar;
                }
                self.saved = server;
            }
            Err(err) => self.error = (self.desired != self.saved).then_some(err),
        }
    }

    fn remote(&mut self, latest: Metadata, revision: Option<i64>) {
        if revision < self.revision || (revision.is_none() && latest == self.seen) {
            return;
        }
        if self.desired.name == self.saved.name {
            self.desired.name = latest.name.clone();
        }
        if self.desired.avatar == self.saved.avatar {
            self.desired.avatar = latest.avatar;
        }
        self.saved = latest.clone();
        self.seen = latest;
        self.revision = revision;
        if self.desired == self.saved {
            self.error = None;
        }
    }
}

pub(super) struct Details {
    sync: MetadataSync,
    name: Entity<InputState>,
    editing: bool,
    problem: Option<&'static str>,
    picking: bool,
    _edits: Subscription,
}

impl Details {
    pub(super) fn new(bot: &Bot, window: &mut Window, cx: &mut Context<ElseApp>) -> Self {
        let values = Metadata::of(bot);
        let id = bot.id;
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Name")
                .default_value(values.name.clone())
        });
        Self {
            _edits: cx.subscribe(&name, move |app, _, event: &InputEvent, cx| match event {
                InputEvent::PressEnter { .. } | InputEvent::Blur => app.commit_name(id, cx),
                InputEvent::Change => {
                    if let Some(details) = app.settings.get_mut(&id).map(|s| &mut s.details) {
                        details.problem = None;
                    }
                    cx.notify();
                }
                InputEvent::Focus => {}
            }),
            sync: MetadataSync::new(values, bot.metadata_revision),
            name,
            editing: false,
            problem: None,
            picking: false,
        }
    }

    pub(super) fn sync_metadata(&mut self, bot: &Bot) {
        self.sync.remote(Metadata::of(bot), bot.metadata_revision);
    }
}

impl ElseApp {
    pub(in crate::app) fn rename_bot(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.active_bots().any(|b| b.id == id) {
            return;
        }
        self.open_bot(id, window, cx);
        if !self.details.open() {
            self.toggle_settings(window, cx);
        }
        self.edit_name(id, window, cx);
    }

    fn edit_name(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(details) = self.settings.get_mut(&bot).map(|s| &mut s.details) else {
            return;
        };
        if !details.editing {
            details.editing = true;
            details.problem = None;
            let name = details.sync.desired.name.clone();
            details
                .name
                .update(cx, |state, cx| state.set_value(name, window, cx));
        }
        details.name.update(cx, |state, cx| {
            state.focus(window, cx);
            state.select_all(window, cx);
        });
        cx.notify();
    }

    fn commit_name(&mut self, bot: Uuid, cx: &mut Context<Self>) {
        let Some(details) = self.settings.get_mut(&bot).map(|s| &mut s.details) else {
            return;
        };
        if !details.editing {
            return;
        }
        match parse_name(&details.name.read(cx).value()) {
            Ok(name) => {
                details.editing = false;
                details.problem = None;
                details.sync.desired.name = name;
                self.flush_details(bot, cx);
            }
            Err(problem) => details.problem = Some(problem),
        }
        cx.notify();
    }

    fn pick_avatar(&mut self, bot: Uuid, avatar: Avatar, cx: &mut Context<Self>) {
        if let Some(details) = self.settings.get_mut(&bot).map(|s| &mut s.details) {
            details.sync.desired.avatar = avatar;
            self.flush_details(bot, cx);
            cx.notify();
        }
    }

    fn set_picking(&mut self, bot: Uuid, open: bool, cx: &mut Context<Self>) {
        if let Some(details) = self.settings.get_mut(&bot).map(|s| &mut s.details) {
            details.picking = open;
            cx.notify();
        }
    }

    fn flush_details(&mut self, bot: Uuid, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let Some(change) = self
            .settings
            .get_mut(&bot)
            .and_then(|s| s.details.sync.begin())
        else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = client.update_bot(bot, change.name, change.avatar).await;
            let _ = this.update(cx, |app, cx| {
                let Some(details) = app.settings.get_mut(&bot).map(|s| &mut s.details) else {
                    return;
                };
                details.sync.finish(
                    result
                        .as_ref()
                        .map(|bot| (Metadata::of(bot), bot.metadata_revision))
                        .map_err(ToString::to_string),
                );
                if details.sync.error.is_none() {
                    app.flush_details(bot, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The panel's compact identity header: a 32 px avatar that opens the
    /// picker and the bot's 15 px name, which turns into a field when
    /// clicked. A full-width rule closes it like every section below.
    pub(super) fn render_identity(
        &self,
        bot: Uuid,
        details: &Details,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let name = if details.editing {
            div()
                .id("bot-name-editor")
                .w_full()
                .on_mouse_down_out(cx.listener(move |app, _, _, cx| app.commit_name(bot, cx)))
                .child(
                    text_field(Input::new(&details.name), cx)
                        .text_left()
                        .text_size(px(15.))
                        .font_weight(FontWeight::SEMIBOLD),
                )
                .into_any_element()
        } else {
            let hover = theme.sidebar_accent;
            h_flex()
                .id("bot-name")
                .max_w_full()
                .min_w_0()
                .gap_1p5()
                .px_2()
                .py_0p5()
                .rounded(theme.radius)
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .group("bot-name")
                .aria_description("Click to rename")
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(15.))
                        .line_height(px(20.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(details.sync.desired.name.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .opacity(0.)
                        .group_hover("bot-name", |style| style.opacity(1.))
                        .child(
                            Icon::new(gpui_kit::assets::IconName::PencilLine)
                                .size_3p5()
                                .text_color(theme.muted_foreground),
                        ),
                )
                .on_click(cx.listener(move |app, _, window, cx| app.edit_name(bot, window, cx)))
                .into_any_element()
        };
        let problem = details
            .problem
            .filter(|_| details.editing)
            .map(|problem| note(problem, true, cx));
        let error = details.sync.error.clone().map(|err| {
            h_flex()
                .gap_2()
                .items_center()
                .child(note(format!("Couldn't save. {err}"), true, cx))
                .child(
                    Button::new("retry-details")
                        .control(Tier::Secondary, Scale::Compact, false, cx)
                        .label("Retry")
                        .on_click(cx.listener(move |app, _, _, cx| app.flush_details(bot, cx))),
                )
        });
        h_flex()
            .flex_none()
            .w_full()
            .px_4()
            .py_3()
            .gap_1()
            .items_center()
            .border_b_1()
            .border_color(hairline(cx))
            .child(self.render_avatar_editor(bot, details, cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(name)
                    .children(problem)
                    .children(error),
            )
            .into_any_element()
    }

    fn render_avatar_editor(
        &self,
        bot: Uuid,
        details: &Details,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let editing = if details.picking {
            "Close avatar picker"
        } else {
            "Edit avatar"
        };
        let badge = div()
            .absolute()
            .right(px(-3.))
            .bottom(px(-3.))
            .size(px(16.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .text_color(theme.popover_foreground)
            .shadow_sm()
            .opacity(if details.picking { 1. } else { 0. })
            .group_hover("avatar-editor", |style| style.opacity(1.))
            .child(Icon::new(gpui_kit::assets::IconName::PencilLine).with_size(Size::Size(px(9.))));
        let hero = Button::new("edit-avatar")
            .focus_ring(false)
            .cursor_pointer()
            .ghost()
            .w(px(32.))
            .h(px(32.))
            .p_0()
            .rounded(theme.radius)
            .accessibility_label(editing)
            .toggled(details.picking)
            .child(
                div()
                    .relative()
                    .group("avatar-editor")
                    .child(bot_avatar(
                        ElementId::Name(format!("settings-avatar-{bot}").into()),
                        details.sync.desired.avatar,
                        px(32.),
                    ))
                    .child(badge),
            );
        let app = cx.entity().downgrade();
        let toggle = app.clone();
        // The header sits at the top of the panel, so the picker opens down.
        Popover::new("avatar-picker")
            .anchor(Anchor::TopLeft)
            .open(details.picking)
            .on_open_change(move |open, _, cx| {
                let _ = toggle.update(cx, |app, cx| app.set_picking(bot, *open, cx));
            })
            .trigger(hero)
            .content(move |_, _, cx| {
                let current = app
                    .upgrade()
                    .and_then(|app| {
                        let app = app.read(cx);
                        app.settings
                            .get(&bot)
                            .map(|s| s.details.sync.desired.avatar)
                    })
                    .unwrap_or_default();
                let app = app.clone();
                AvatarPicker::new(current, move |avatar, _, cx| {
                    let _ = app.update(cx, |app, cx| app.pick_avatar(bot, avatar, cx));
                })
                .into_any_element()
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{Metadata, MetadataChange, MetadataSync, parse_name};
    use crate::avatar::{Avatar, AvatarColor, AvatarShape};

    fn metadata(name: &str, avatar: Avatar) -> Metadata {
        Metadata {
            name: name.into(),
            avatar,
        }
    }

    fn egg() -> Avatar {
        Avatar {
            shape: AvatarShape::ALL[1],
            color: AvatarColor::ALL[1],
        }
    }

    #[test]
    fn names_mirror_the_api_limits() {
        assert_eq!(parse_name("  Planner "), Ok("Planner".into()));
        assert!(parse_name(&"é".repeat(80)).is_ok());
        assert!(parse_name(" ").is_err());
        assert!(parse_name(&"é".repeat(81)).is_err());
    }

    #[test]
    fn one_request_at_a_time_and_newer_intent_survives_its_response() {
        let mut sync = MetadataSync::new(metadata("Old", Avatar::default()), Some(0));
        sync.desired.name = "New".into();
        assert_eq!(
            sync.begin(),
            Some(MetadataChange {
                name: Some("New".into()),
                avatar: None,
            })
        );
        sync.desired.avatar = egg();
        assert_eq!(sync.begin(), None, "waits for the request in flight");
        sync.remote(metadata("Old", Avatar::default()), Some(0));
        assert_eq!(sync.desired, metadata("New", egg()), "stale list ignored");

        sync.finish(Ok((metadata("New", Avatar::default()), Some(1))));
        assert_eq!(sync.desired, metadata("New", egg()), "the newer pick stays");
        assert_eq!(
            sync.begin(),
            Some(MetadataChange {
                name: None,
                avatar: Some(egg()),
            }),
            "only the pick made meanwhile"
        );
        sync.finish(Ok((metadata("New", egg()), Some(2))));
        assert_eq!(sync.begin(), None, "nothing left to save");
    }

    #[test]
    fn a_delayed_echo_cannot_change_the_shape_of_the_next_color_pick() {
        let mut sync = MetadataSync::new(metadata("Bot", Avatar::default()), Some(0));
        sync.desired.avatar = egg();
        sync.begin();
        sync.finish(Ok((metadata("Bot", egg()), Some(1))));
        let cloud = Avatar {
            shape: AvatarShape::Cloud,
            ..egg()
        };
        sync.desired.avatar = cloud;
        sync.begin();
        sync.finish(Ok((metadata("Bot", cloud), Some(2))));
        sync.remote(metadata("Bot", egg()), Some(1));
        sync.desired.avatar.color = AvatarColor::Pink;
        assert_eq!(
            sync.begin(),
            Some(MetadataChange {
                name: None,
                avatar: Some(Avatar {
                    color: AvatarColor::Pink,
                    ..cloud
                }),
            })
        );
    }

    #[test]
    fn a_newer_remote_edit_survives_an_older_http_response() {
        let mut sync = MetadataSync::new(metadata("Old", Avatar::default()), Some(0));
        sync.desired.name = "Local".into();
        sync.begin();
        sync.remote(metadata("Remote", Avatar::default()), Some(2));
        sync.finish(Ok((metadata("Local", Avatar::default()), Some(1))));
        assert_eq!(sync.desired.name, "Remote");
        assert_eq!(sync.begin(), None);
    }

    #[test]
    fn a_server_echo_confirms_a_save_whose_response_was_lost() {
        let mut sync = MetadataSync::new(metadata("Old", Avatar::default()), Some(0));
        sync.desired.avatar = egg();
        sync.begin();
        sync.finish(Err("lost response".into()));
        sync.remote(metadata("Old", egg()), Some(1));
        assert_eq!(sync.saved, sync.desired);
        assert_eq!(sync.error, None);
        assert_eq!(sync.begin(), None);
    }

    #[test]
    fn a_server_echo_before_a_lost_response_stays_confirmed() {
        let mut sync = MetadataSync::new(metadata("Old", Avatar::default()), Some(0));
        sync.desired.avatar = egg();
        sync.begin();
        sync.remote(metadata("Old", egg()), Some(1));
        sync.finish(Err("lost response".into()));
        assert_eq!(sync.error, None);
        assert_eq!(sync.begin(), None);
    }

    #[test]
    fn a_failed_save_keeps_the_change_for_a_retry() {
        let mut sync = MetadataSync::new(metadata("Old", Avatar::default()), Some(0));
        sync.desired.avatar = egg();
        sync.begin();
        sync.finish(Err("offline".into()));
        assert_eq!(sync.error.as_deref(), Some("offline"));
        sync.remote(metadata("Elsewhere", Avatar::default()), Some(1));
        assert_eq!(
            sync.desired,
            metadata("Elsewhere", egg()),
            "a remote rename lands; the unsaved pick stays"
        );
        assert_eq!(
            sync.begin(),
            Some(MetadataChange {
                name: None,
                avatar: Some(egg()),
            })
        );
        assert_eq!(sync.error, None);
    }
}
