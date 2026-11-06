use super::reply::{Author, MessageReference};
use super::{Connection, ElseApp, RowKey};
use crate::model::{Reaction, ReactionKind};
use gpui_kit::assets::IconName;
use gpui_kit::base::actions::Cancel;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{Disableable as _, Sizable as _, h_flex};
use gpui_kit::*;
use uuid::Uuid;

impl ElseApp {
    pub(super) fn message_actions(
        &self,
        bot: Uuid,
        item: Uuid,
        author: Author,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let connected = self.connection == Connection::Connected;
        h_flex()
            .gap_1()
            .children((author == Author::Bot).then(|| {
                let owner = cx.entity().downgrade();
                Popover::new(SharedString::from(format!("react-{item}")))
                    .anchor(Anchor::TopRight)
                    .p_1()
                    .trigger(
                        Button::new(SharedString::from(format!("add-reaction-{item}")))
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .small()
                            .icon(IconName::FaceSlightlySmilingPlus)
                            .tooltip("Add reaction")
                            .accessibility_label("Add reaction")
                            .disabled(!connected),
                    )
                    .content(move |_, _, _| {
                        h_flex().gap_1().children(
                            [ReactionKind::ThumbsUp, ReactionKind::ThumbsDown].map(|kind| {
                                let owner = owner.clone();
                                Button::new(kind.mark())
                                    .focus_ring(false)
                                    .cursor_pointer()
                                    .ghost()
                                    .label(kind.mark())
                                    .accessibility_label(match kind {
                                        ReactionKind::ThumbsUp => "Thumbs up",
                                        ReactionKind::ThumbsDown => "Thumbs down",
                                    })
                                    .on_click(move |_, window, cx| {
                                        let _ = owner.update(cx, |app, cx| {
                                            app.react(bot, item, kind, window, cx)
                                        });
                                        window.dispatch_action(Box::new(Cancel), cx);
                                    })
                            }),
                        )
                    })
            }))
            .child(
                Button::new(SharedString::from(format!("reply-{item}")))
                    .focus_ring(false)
                    .cursor_pointer()
                    .ghost()
                    .small()
                    .icon(IconName::Reply)
                    .tooltip("Reply")
                    .accessibility_label("Reply to message")
                    .on_click(cx.listener(move |app, _, window, cx| {
                        app.start_reply(bot, item, window, cx);
                    })),
            )
            .into_any_element()
    }

    pub(super) fn message_menu(
        owner: WeakEntity<Self>,
        bot: Uuid,
        item: Uuid,
        author: Author,
        mut menu: PopupMenu,
        _: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let Some(app) = owner.upgrade() else {
            return menu;
        };
        let connected = app.read(cx).connection == Connection::Connected;
        if author == Author::Bot {
            let owner = owner.clone();
            menu = menu
                .item(
                    PopupMenuItem::element(move |_, _| {
                        h_flex().gap_1().children(
                            [ReactionKind::ThumbsUp, ReactionKind::ThumbsDown].map(|reaction| {
                                let owner = owner.clone();
                                Button::new(reaction.mark())
                                    .focus_ring(false)
                                    .cursor_pointer()
                                    .ghost()
                                    .label(reaction.mark())
                                    .disabled(!connected)
                                    .on_click(move |_, window, cx| {
                                        let _ = owner.update(cx, |app, cx| {
                                            app.react(bot, item, reaction, window, cx)
                                        });
                                        window.dispatch_action(Box::new(Cancel), cx);
                                    })
                            }),
                        )
                    })
                    .disabled(true),
                )
                .separator();
        }
        let act = |f: fn(&mut Self, Uuid, Uuid, &mut Window, &mut Context<Self>)| {
            let owner = owner.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = owner.update(cx, |app, cx| f(app, bot, item, window, cx));
            }
        };
        menu.item(
            PopupMenuItem::new("Reply")
                .icon(IconName::Reply)
                .on_click(act(Self::start_reply)),
        )
        .item(
            PopupMenuItem::new("Copy")
                .icon(IconName::Copy)
                .on_click(act(Self::copy_message)),
        )
    }

    fn reference(&self, bot: Uuid, item: Uuid) -> Option<MessageReference> {
        let item = self
            .view
            .as_ref()
            .filter(|v| v.bot_id == bot && self.selected == Some(bot))?
            .conversation
            .as_ref()?
            .item(item)?;
        Some(MessageReference {
            item: item.id,
            author: Author::of(&item.kind)?,
            text: item.text.clone(),
        })
    }

    fn start_reply(&mut self, bot: Uuid, item: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(reference) = self.reference(bot, item) else {
            return;
        };
        self.drafts.entry(bot).or_default().reply_to = Some(reference);
        self.composer.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    pub(super) fn dismiss_reply(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(draft) = self.drafts.get_mut(&bot) {
            draft.reply_to = None;
        }
        if self.selected == Some(bot) {
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn react(
        &mut self,
        bot: Uuid,
        item: Uuid,
        kind: ReactionKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connection != Connection::Connected {
            return;
        }
        let Some(reference) = self
            .reference(bot, item)
            .filter(|r| r.author == Author::Bot)
        else {
            return;
        };
        let reaction = Reaction {
            item_id: reference.item,
            kind,
        };
        self.enqueue(
            bot,
            kind.message(&reference),
            Vec::new(),
            Some(reaction),
            window,
            cx,
        );
    }

    fn copy_message(&mut self, bot: Uuid, item: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let selected = gpui_kit::base::TextSelection::selected_text(window, cx);
        if !selected.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(selected));
            return;
        }
        let Some(text) = self
            .view
            .as_ref()
            .filter(|v| v.bot_id == bot)
            .and_then(|v| v.texts.get(&RowKey::Item(item)))
            .map(|row| row.copy_text(cx))
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
}
