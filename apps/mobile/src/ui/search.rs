//! Searching one bot's chat: the server's matches, newest first, each with
//! who wrote it and its text; a tap shows the message among its neighbours,
//! as iOS Messages shows a search result in context.

use std::sync::Arc;

use else_wire::conversation::SearchHit;
use else_wire::items::{Item, ItemKind};
use futures_util::future::join_all;
use nsh_base::{input, nav_page};
use nsh_core::prelude::*;
use uuid::Uuid;

use super::list;
use super::parts::{self, colors};
use super::session;
use crate::api::Client;

/// The most matches listed; the count says how many there are.
const SHOWN: usize = 30;
/// Messages shown on each side of a match.
const AROUND: i64 = 6;

pub fn open(bot: Uuid) {
    super::push(format!("search-{bot}"), move || screen(bot));
}

/// A match as the list shows it.
#[derive(Debug, Clone, PartialEq)]
struct Found {
    hit: SearchHit,
    item: Result<Item, String>,
}

/// What the search found, or why it couldn't.
#[derive(Debug, Clone, PartialEq)]
enum Results {
    Idle,
    Searching,
    Found { total: usize, found: Vec<Found> },
    Failed(String),
}

/// The match's own message: a window's `older` counts the anchor.
async fn item_of(client: Arc<Client>, bot: Uuid, hit: &SearchHit) -> Result<Item, String> {
    let window = client
        .window(bot, hit.seq, (1, 0), (None, None))
        .await
        .map_err(|err| err.to_string())?;
    window
        .items
        .into_iter()
        .find(|item| item.id == hit.item_id)
        .ok_or_else(|| "This message is no longer shown.".to_owned())
}

fn who(item: &Item) -> &'static str {
    match item.kind {
        ItemKind::User => "You",
        ItemKind::Assistant => "Bot",
        _ => "Note",
    }
}

/// What the owner wrote, without the quote a reply carries.
fn shown(text: &str) -> &str {
    else_client::reply::Replying::parse(text).map_or(text, |read| read.body)
}

fn one_line(text: &str) -> String {
    shown(text).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn screen(bot: Uuid) -> Screen {
    let query = signal(String::new());
    let results = signal(Results::Idle);
    // Each search supersedes the one before: only the newest lands.
    let round = signal(0_u64);
    let search = move || {
        let wanted = query.get_untracked().trim().to_owned();
        if wanted.is_empty() {
            results.set(Results::Idle);
            return;
        }
        let this = round.get_untracked() + 1;
        round.set(this);
        results.set(Results::Searching);
        let client = session::server().client.clone();
        spawn(async move {
            let outcome = match client.clone().search(bot, wanted).await {
                Ok(hits) => {
                    let shown: Vec<SearchHit> = hits.hits.into_iter().take(SHOWN).collect();
                    let items =
                        join_all(shown.iter().map(|hit| item_of(client.clone(), bot, hit))).await;
                    Results::Found {
                        total: hits.total,
                        found: shown
                            .into_iter()
                            .zip(items)
                            .map(|(hit, item)| Found { hit, item })
                            .collect(),
                    }
                }
                Err(err) => Results::Failed(err.to_string()),
            };
            if round.try_get() == Some(this) {
                results.try_set(outcome);
            }
        });
    };
    let body = dynamic(move || {
        Some(match results.get() {
            Results::Idle => parts::footnote("Search this chat's messages.").into_element(),
            Results::Searching => parts::footnote("Searching…").into_element(),
            Results::Failed(message) => parts::footnote(message).into_element(),
            Results::Found { total: 0, .. } => parts::footnote("No messages match.").into_element(),
            Results::Found { total, found } => {
                let rows: Vec<AnyElement> = found
                    .into_iter()
                    .map(|found| {
                        let (title, snippet) = match &found.item {
                            Ok(item) => (who(item).to_owned(), one_line(&item.text)),
                            Err(err) => ("Message".to_owned(), err.clone()),
                        };
                        let hit = found.hit;
                        view()
                            .px_4()
                            .py(px(11.0))
                            .gap(px(2.0))
                            .active(|s| s.bg(colors::fill()))
                            .role(AccessibilityRole::Button)
                            .on_press(move || open_context(bot, hit))
                            .child(
                                text(title)
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(colors::secondary()),
                            )
                            .child(
                                text(snippet)
                                    .number_of_lines(3)
                                    .text_size(px(15.0))
                                    .line_height(px(20.0))
                                    .text_color(colors::label()),
                            )
                            .into_element()
                    })
                    .collect();
                let shown = rows.len();
                list::section(
                    Some(if total == 1 { "1 match" } else { "Matches" }),
                    rows,
                    (total > shown).then(|| format!("Showing the newest {shown} of {total}.")),
                )
                .into_element()
            }
        })
    });
    nav_page()
        .key(format!("search-{bot}"))
        .title("Search")
        .header_large_title(false)
        .child(
            scroll()
                .flex_1()
                .bg(colors::background())
                .keyboard_dismiss_mode(KeyboardDismissMode::Interactive)
                .child(
                    view()
                        .py_4()
                        .gap_4()
                        .child(
                            parts::group().child(
                                input()
                                    .px_4()
                                    .min_h(px(44.0))
                                    .value(query)
                                    .on_change(move |t: &str| query.set(t.to_owned()))
                                    .placeholder("Search")
                                    .auto_focus(true)
                                    .auto_capitalize(AutoCapitalize::None)
                                    .return_key_type(ReturnKeyType::Search)
                                    .on_submit(move |_: &str| search())
                                    .text_size(px(17.0))
                                    .text_color(colors::label())
                                    .placeholder_color(|| color(colors::secondary()))
                                    .caret_color(|| color(colors::tint()))
                                    .label("Search this chat"),
                            ),
                        )
                        .child(body),
                ),
        )
}

fn open_context(bot: Uuid, hit: SearchHit) {
    super::push(format!("search-{bot}-{}", hit.item_id), move || {
        context(bot, hit)
    });
}

/// The match among its neighbours, the match marked.
fn context(bot: Uuid, hit: SearchHit) -> Screen {
    let around = resource(
        || Some(()),
        move |(), _| {
            let client = session::server().client.clone();
            async move {
                client
                    .window(bot, hit.seq, (AROUND, AROUND), (None, None))
                    .await
                    .map(|window| window.items)
                    .map_err(|err| err.to_string())
            }
        },
    );
    let body = dynamic(move || match (around.get(), around.error()) {
        (None, None) => Some(
            view()
                .py_4()
                .child(parts::footnote("Loading…"))
                .into_element(),
        ),
        (None, Some(message)) => Some(view().py_4().child(parts::footnote(message)).into_element()),
        (Some(items), _) => {
            // Reactions show as marks in the chat, not as messages.
            let items: Vec<Item> = items
                .into_iter()
                .filter(|item| !item.text.trim().is_empty() && item.reaction.is_none())
                .collect();
            let at = items
                .iter()
                .position(|item| item.id == hit.item_id)
                .unwrap_or(0);
            let count = items.len();
            let items = std::rc::Rc::new(items);
            Some(untracked(|| {
                virtual_list(
                    move || count,
                    move |row: Position| {
                        let item = &items[row.index()];
                        view().py_1().child(message(item, item.id == hit.item_id))
                    },
                )
                .estimated_item_size(px(80.0))
                .initial_scroll_index(at)
                .flex_1()
                .content_padding_top(px(12.0))
                .content_padding_bottom(px(12.0))
                .into_element()
            }))
        }
    });
    nav_page()
        .key(format!("search-{bot}-{}", hit.item_id))
        .title("Match")
        .header_large_title(false)
        .child(view().flex_1().bg(colors::chat()).child(body))
}

/// One message of the context: the owner's on the right, the rest on the
/// left, the match ringed.
fn message(item: &Item, matched: bool) -> impl IntoElement {
    let owner = item.kind == ItemKind::User;
    let row = view().px_4().w_full();
    if owner {
        row.items_end()
    } else {
        row.items_start()
    }
    .child(
        view()
            .max_w(relative(0.85))
            .px(px(14.0))
            .py(px(8.0))
            .rounded(px(18.0))
            .border_curve(BorderCurve::Continuous)
            .bg(if owner {
                colors::bubble_owner()
            } else {
                colors::bubble_bot()
            })
            .border_2()
            .border_color(if matched {
                colors::warning()
            } else if owner {
                colors::bubble_owner()
            } else {
                colors::bubble_bot()
            })
            .child(
                text(shown(&item.text).to_owned())
                    .selectable(true)
                    .text_size(px(16.0))
                    .line_height(px(21.0))
                    .text_color(if owner {
                        rgb(0xffffff).into()
                    } else {
                        colors::label()
                    }),
            ),
    )
}
