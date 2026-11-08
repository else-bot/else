//! A bot-to-bot exchange read as a chat: the open conversation's bot on the
//! right, the bots it talked to on the left, one bubble per message. A reply
//! carries the task it answers, so the task shows as this bot's bubble just
//! before the reply.

use super::reply::Author;
use super::{ElseApp, RowKey};
use gpui_kit::component::menu::ContextMenuExt as _;
use crate::avatar::bot_avatar;
use crate::model::{Conversation, Item};
use chrono::{DateTime, Utc};
use gpui_kit::component::text::TextViewState;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Body {
    /// The item's own text, already rendered for the transcript.
    Item(Uuid),
    /// The task a reply answers.
    Prompt { item: Uuid, text: String },
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Bubble {
    pub(super) sender: Uuid,
    pub(super) name: String,
    pub(super) kind: &'static str,
    pub(super) at: Option<DateTime<Utc>>,
    pub(super) mine: bool,
    pub(super) body: Body,
}

pub(super) fn bubbles(items: &[&Item], me: Uuid, my_name: &str) -> Vec<Bubble> {
    let mut bubbles = Vec::new();
    for item in items {
        if let Some(task) = &item.bot_task {
            bubbles.push(Bubble {
                sender: task.sender_bot_id,
                name: task.sender_name.clone(),
                kind: "Task",
                at: item.created_at,
                mine: task.sender_bot_id == me,
                body: if task.prompt.trim().is_empty() {
                    Body::Empty
                } else {
                    Body::Item(item.id)
                },
            });
        } else if let Some(reply) = &item.bot_reply {
            if !reply.prompt.trim().is_empty() {
                bubbles.push(Bubble {
                    sender: me,
                    name: my_name.to_owned(),
                    kind: "Task",
                    at: None,
                    mine: true,
                    body: Body::Prompt {
                        item: item.id,
                        text: reply.prompt.clone(),
                    },
                });
            }
            bubbles.push(Bubble {
                sender: reply.sender_bot_id,
                name: reply.sender_name.clone(),
                kind: reply.status.label(),
                at: item.created_at,
                mine: reply.sender_bot_id == me,
                body: if item.text.trim().is_empty() {
                    Body::Empty
                } else {
                    Body::Item(item.id)
                },
            });
        }
    }
    bubbles
}

type Prompts = Rc<RefCell<HashMap<Uuid, Entity<TextViewState>>>>;

impl ElseApp {
    pub(super) fn open_bot_exchange(
        &mut self,
        anchor: Uuid,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let owner = cx.entity().downgrade();
        let prompts: Prompts = Rc::default();
        window.open_dialog(cx, move |dialog, _, cx| {
            let dialog = dialog.title(title.clone()).w(px(680.));
            let Some(app) = owner.upgrade() else {
                return dialog;
            };
            let Some((me, bubbles)) = app.read(cx).exchange_bubbles(anchor) else {
                return dialog.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("This exchange is no longer in the open conversation."),
                );
            };
            for bubble in &bubbles {
                if let Body::Prompt { item, text } = &bubble.body
                    && !prompts.borrow().contains_key(item)
                {
                    let state = cx.new(|cx| TextViewState::markdown(text, cx));
                    prompts.borrow_mut().insert(*item, state);
                }
            }
            let app = app.read(cx);
            let messages = bubbles
                .iter()
                .enumerate()
                .map(|(ix, bubble)| app.render_bubble(ix, me, bubble, &prompts.borrow(), &owner, cx));
            dialog.child(
                v_flex()
                    .id("bot-exchange")
                    .w_full()
                    .max_h(px(560.))
                    .overflow_y_scroll()
                    .gap_4()
                    .pr_2()
                    .children(messages),
            )
        });
    }

    fn exchange_bubbles(&self, anchor: Uuid) -> Option<(Uuid, Vec<Bubble>)> {
        let view = self.view.as_ref()?;
        let conversation: &Conversation = view.conversation.as_ref()?;
        let activity = view
            .rows
            .iter()
            .find(|row| row.key == RowKey::Item(anchor))?
            .activity
            .as_ref()?;
        let items: Vec<&Item> = activity
            .items
            .iter()
            .filter_map(|id| conversation.item(*id))
            .collect();
        let me = view.bot_id;
        let name = self
            .bots
            .iter()
            .find(|bot| bot.id == me)
            .map_or("This bot", |bot| bot.name.as_str());
        Some((me, bubbles(&items, me, name)))
    }

    fn render_bubble(
        &self,
        ix: usize,
        me: Uuid,
        bubble: &Bubble,
        prompts: &HashMap<Uuid, Entity<TextViewState>>,
        owner: &WeakEntity<ElseApp>,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme();
        let body = match &bubble.body {
            Body::Item(id) => self
                .view
                .as_ref()
                .and_then(|view| view.texts.get(&RowKey::Item(*id)))
                .map(|text| text.render(cx)),
            Body::Prompt { item, .. } => prompts
                .get(item)
                .map(|state| super::row_text::markdown_view(state, cx).into_any_element()),
            Body::Empty => None,
        }
        .unwrap_or_else(|| {
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Finished without a reply.")
                .into_any_element()
        });
        let mine = bubble.mine || bubble.sender == me;
        let avatar = self
            .bots
            .iter()
            .find(|bot| bot.id == bubble.sender)
            .map(|bot| {
                bot_avatar(
                    SharedString::from(format!("exchange-{ix}-{}", bubble.sender)),
                    bot.avatar,
                    px(28.),
                )
                .still()
                .into_any_element()
            })
            .unwrap_or_else(|| div().size(px(28.)).flex_none().into_any_element());
        let meta = h_flex()
            .gap_1p5()
            .text_xs()
            .text_color(theme.muted_foreground)
            .when(mine, |meta| meta.flex_row_reverse())
            .child(
                div()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(bubble.name.clone()),
            )
            .child(bubble.kind)
            .children(bubble.at.map(super::message_row::stamp));
        let item = match bubble.body {
            Body::Item(id) => Some(id),
            _ => None,
        };
        let card = div()
            .id(SharedString::from(format!("exchange-bubble-{ix}")))
            .w_full()
            .px_3p5()
            .py_2()
            .rounded(theme.radius_lg)
            .map(|card| {
                if mine {
                    card.bg(theme.secondary)
                } else {
                    card.border_1()
                        .border_color(theme.border.opacity(0.6))
                        .bg(theme.sidebar.opacity(0.6))
                }
            })
            .child(body);
        let card = match item {
            Some(item) => {
                let owner = owner.clone();
                card.context_menu(move |menu, window, cx| {
                    ElseApp::message_menu(owner.clone(), me, item, Author::Bot, menu, window, cx)
                })
                .into_any_element()
            }
            None => card.into_any_element(),
        };
        h_flex()
            .w_full()
            .items_start()
            .gap_2p5()
            .when(mine, |row| row.flex_row_reverse())
            .child(avatar)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .max_w(relative(0.85))
                    .gap_1()
                    .when(mine, |column| column.items_end())
                    .child(meta)
                    .child(card),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{Body, bubbles};
    use crate::model::Item;
    use serde_json::json;
    use uuid::Uuid;

    fn item(id: u128, extra: serde_json::Value) -> Item {
        let mut value = json!({
            "id": Uuid::from_u128(id),
            "turn_id": Uuid::from_u128(100),
            "seq": id,
            "kind": "user",
            "text": "Done: v0.94.24 is live.",
        });
        value.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_reply_reads_as_this_bots_task_then_the_other_bots_answer() {
        let me = Uuid::from_u128(1);
        let release = Uuid::from_u128(2);
        let reply = item(
            10,
            json!({"bot_reply": {
                "sender_bot_id": release, "sender_name": "Release",
                "prompt": "Cut a patch release.", "status": "completed"
            }}),
        );
        let task = item(
            11,
            json!({"text": "", "bot_task": {
                "sender_bot_id": release, "sender_name": "Release", "prompt": "Review the notes."
            }}),
        );
        let bubbles = bubbles(&[&reply, &task], me, "Tech Manager");
        let shape: Vec<_> = bubbles
            .iter()
            .map(|b| (b.name.as_str(), b.kind, b.mine))
            .collect();
        assert_eq!(
            shape,
            [
                ("Tech Manager", "Task", true),
                ("Release", "Reply", false),
                ("Release", "Task", false),
            ]
        );
        assert_eq!(
            bubbles[0].body,
            Body::Prompt {
                item: reply.id,
                text: "Cut a patch release.".into()
            }
        );
        assert_eq!(bubbles[1].body, Body::Item(reply.id));
        assert_eq!(bubbles[2].body, Body::Item(task.id));
    }

    #[test]
    fn an_empty_reply_still_shows_who_answered() {
        let reply = item(
            10,
            json!({"text": " ", "bot_reply": {
                "sender_bot_id": Uuid::from_u128(2), "sender_name": "Release",
                "prompt": "", "status": "failed"
            }}),
        );
        let bubbles = bubbles(&[&reply], Uuid::from_u128(1), "Tech Manager");
        assert_eq!(bubbles.len(), 1);
        assert_eq!(bubbles[0].kind, "Reply · Failed");
        assert_eq!(bubbles[0].body, Body::Empty);
    }
}
