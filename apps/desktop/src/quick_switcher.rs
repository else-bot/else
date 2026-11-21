//! The Ctrl+K (Cmd+K on macOS) quick switcher's index and ranking. Pure, so
//! ranking is tested without a window.
//!
//! The index holds every saved server in rail order with the bots the
//! desktop last saw on it, so a search never waits on the network and an
//! offline server's bots still show. A match on the start of the name beats
//! one on the start of a word, which beats one inside a word, which beats
//! letters spread out from a word start. A bot also matches on its server's name,
//! at a lower weight. A query of several words can also name a server and then
//! a bot on it ("pro tech" or "p tm" finds Proliquid's Tech Manager) when each
//! part starts a word or spells word initials. Such a reading ranks first,
//! after only a name that starts with the whole query, and prefers a recently
//! used server. Recently opened items rank higher.

use else_wire::bots::Avatar;

use std::collections::HashMap;
use std::path::Path;

use crate::sidebar::fold;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How many recently opened items the switcher remembers.
pub const RECENTS: usize = 8;
const MAX_MEMORY_BYTES: u64 = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Server { server: Uuid },
    Bot { server: Uuid, bot: Uuid },
}

impl Target {
    pub fn server(self) -> Uuid {
        match self {
            Self::Server { server } | Self::Bot { server, .. } => server,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bot {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub avatar: Avatar,
}

impl Bot {
    /// The switcher's entries for a server's bot list, without bots being
    /// deleted.
    pub fn listed(bots: &[else_wire::bots::Bot]) -> Vec<Self> {
        bots.iter()
            .filter(|bot| bot.is_active())
            .map(|bot| Self {
                id: bot.id,
                name: bot.name.clone(),
                avatar: bot.avatar,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    pub id: Uuid,
    pub name: String,
    pub bots: Vec<Bot>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Index {
    /// In rail order.
    pub servers: Vec<Server>,
}

/// One result: what Enter opens and which characters of its name, or of its
/// server's name, matched the query.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub target: Target,
    /// Char indices into the item's name.
    pub name: Vec<usize>,
    /// Char indices into the server's name, when a bot matched on it.
    pub server: Vec<usize>,
}

impl Index {
    pub fn server(&self, id: Uuid) -> Option<&Server> {
        self.servers.iter().find(|s| s.id == id)
    }

    pub fn bot(&self, server: Uuid, bot: Uuid) -> Option<(&Server, &Bot)> {
        let server = self.server(server)?;
        Some((server, server.bots.iter().find(|b| b.id == bot)?))
    }

    pub fn contains(&self, target: Target) -> bool {
        match target {
            Target::Server { server } => self.server(server).is_some(),
            Target::Bot { server, bot } => self.bot(server, bot).is_some(),
        }
    }

    /// The results for `query`, best first. An empty query lists recent
    /// items, or every bot of the scoped server with recent ones first.
    /// `scope` limits results to one server's bots.
    pub fn search(&self, query: &str, scope: Option<Uuid>, recents: &[Target]) -> Vec<Hit> {
        let bonus =
            |seen: Option<usize>| seen.map_or(0, |rank| (RECENTS - rank.min(RECENTS)) as i32 * 8);
        let recency = |target: Target| bonus(recents.iter().position(|r| *r == target));
        let server_recency =
            |server: Uuid| bonus(recents.iter().position(|r| r.server() == server));
        let words: Vec<&str> = query.split_whitespace().collect();
        let query = words.join(" ");
        let query = query.as_str();
        let named = |text: &str| {
            let (tier, at) = find(query, text)?;
            let lead = if words.len() > 1 && tier == Tier::Prefix {
                SPLIT_TOP
            } else {
                0
            };
            Some((tier.points(text) + lead, at))
        };
        let servers = self
            .servers
            .iter()
            .filter(|s| scope.is_none_or(|id| s.id == id));
        let mut scored: Vec<(i32, String, Hit)> = Vec::new();
        for server in servers {
            if scope.is_none() {
                let target = Target::Server { server: server.id };
                let candidate = if query.is_empty() {
                    recents.contains(&target).then(|| (0, Vec::new()))
                } else {
                    named(&server.name)
                };
                if let Some((points, name)) = candidate {
                    scored.push((
                        points + recency(target),
                        fold(&server.name),
                        Hit {
                            target,
                            name,
                            server: Vec::new(),
                        },
                    ));
                }
            }
            for bot in &server.bots {
                let target = Target::Bot {
                    server: server.id,
                    bot: bot.id,
                };
                let candidate = if query.is_empty() {
                    (scope.is_some() || recents.contains(&target))
                        .then(|| (0, Vec::new(), Vec::new()))
                } else {
                    let by_name = named(&bot.name).map(|(p, at)| (p, at, Vec::new()));
                    let by_server = scope
                        .is_none()
                        .then(|| score(query, &server.name))
                        .flatten()
                        .map(|(p, at)| (p * 3 / 5, Vec::new(), at));
                    let by_both = scope
                        .is_none()
                        .then(|| split(&words, &server.name, &bot.name))
                        .flatten()
                        .map(|(p, at, server_at)| (p + server_recency(server.id), at, server_at));
                    [by_name, by_server, by_both]
                        .into_iter()
                        .flatten()
                        .reduce(|best, next| if next.0 > best.0 { next } else { best })
                };
                if let Some((points, name, server_at)) = candidate {
                    scored.push((
                        points + recency(target),
                        fold(&bot.name),
                        Hit {
                            target,
                            name,
                            server: server_at,
                        },
                    ));
                }
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored.into_iter().map(|(_, _, hit)| hit).collect()
    }
}

/// More than any split reading scores, so a name that starts with the whole
/// query, such as a server whose name is still being typed, stays first.
const SPLIT_TOP: i32 = 4000;

/// The best reading of `words` as a server's name followed by a bot's name:
/// its points and the matched chars of the bot's and the server's names.
/// Each part must start a word or spell word initials; any such reading
/// outranks a match on a single name.
fn split(words: &[&str], server: &str, bot: &str) -> Option<(i32, Vec<usize>, Vec<usize>)> {
    let weight = |tier: Tier| match tier {
        Tier::Prefix => Some(1000),
        Tier::WordStart => Some(800),
        Tier::Initials(points) => Some(points),
        Tier::Inside | Tier::Spread(_) => None,
    };
    (1..words.len())
        .filter_map(|at| {
            let (s_tier, s_at) = find(&words[..at].join(" "), server)?;
            let (b_tier, b_at) = find(&words[at..].join(" "), bot)?;
            Some((1000 + weight(s_tier)? + weight(b_tier)?, b_at, s_at))
        })
        .max_by_key(|(points, ..)| *points)
}

/// How a query's letters sit in a name, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// The name starts with the query.
    Prefix,
    /// A word in the name starts with the query.
    WordStart,
    /// The query appears inside a word.
    Inside,
    /// Every letter starts a word, with the placement's points.
    Initials(i32),
    /// The letters are spread out from a word start, with the placement's
    /// points.
    Spread(i32),
}

impl Tier {
    fn points(self, text: &str) -> i32 {
        let len = text.chars().count() as i32;
        match self {
            Self::Prefix => 1000 - len,
            Self::WordStart => 800 - len,
            Self::Inside => 500 - len,
            Self::Initials(points) | Self::Spread(points) => points,
        }
    }
}

/// How well `query` matches `text`, and which chars matched; `None` when it
/// does not. Case-insensitive.
pub fn score(query: &str, text: &str) -> Option<(i32, Vec<usize>)> {
    let (tier, at) = find(query, text)?;
    Some((tier.points(text), at))
}

fn find(query: &str, text: &str) -> Option<(Tier, Vec<usize>)> {
    let lower = |s: &str| -> Vec<char> {
        s.chars()
            .map(|c| c.to_lowercase().next().unwrap_or(c))
            .collect()
    };
    let (query, text) = (lower(query), lower(text));
    if query.is_empty() || query.len() > text.len() {
        return None;
    }
    let word_start = |i: usize| i == 0 || !text[i - 1].is_alphanumeric();
    let run = |at: usize| (at..at + query.len()).collect::<Vec<_>>();
    let found: Vec<usize> = (0..=text.len() - query.len())
        .filter(|&at| text[at..at + query.len()] == query[..])
        .collect();
    if found.first() == Some(&0) {
        return Some((Tier::Prefix, run(0)));
    }
    if let Some(&at) = found.iter().find(|&&at| word_start(at)) {
        return Some((Tier::WordStart, run(at)));
    }
    if let Some(&at) = found.first() {
        return Some((Tier::Inside, run(at)));
    }
    // Letters spread out: the first must start a word. Of every way to
    // place the letters, keep the best scoring: word starts and adjacent
    // letters earn points, skipped letters cost them.
    let (q, t) = (query.len(), text.len());
    let gain = |j: usize| if word_start(j) { 12 } else { 0 };
    // best[i][j]: the best score with letter i of the query at char j.
    let mut best = vec![vec![None::<i32>; t]; q];
    let mut from = vec![vec![0usize; t]; q];
    for j in 0..t {
        if text[j] == query[0] && word_start(j) {
            best[0][j] = Some(gain(j) - j as i32 * 2);
        }
    }
    for i in 1..q {
        for j in i..t {
            if text[j] != query[i] {
                continue;
            }
            for k in i - 1..j {
                let Some(before) = best[i - 1][k] else {
                    continue;
                };
                let adjacent = if j == k + 1 { 6 } else { 0 };
                let points = before + gain(j) + adjacent - (j - k - 1) as i32 * 2;
                if best[i][j].is_none_or(|current| points > current) {
                    best[i][j] = Some(points);
                    from[i][j] = k;
                }
            }
        }
    }
    let (mut j, points) = (0..t)
        .filter_map(|j| best[q - 1][j].map(|points| (j, points)))
        .max_by_key(|&(j, points)| (points, std::cmp::Reverse(j)))?;
    let mut at = vec![j; q];
    for i in (1..q).rev() {
        j = from[i][j];
        at[i - 1] = j;
    }
    let points = 200 + points;
    let tier = if at.iter().all(|&j| word_start(j)) {
        Tier::Initials(points)
    } else {
        Tier::Spread(points)
    };
    (points >= 150).then_some((tier, at))
}

/// A key the open switcher handles before its search field sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Down,
    Up,
    Enter,
    Escape,
    /// Search within the selected server.
    Tab,
    Backspace,
}

/// What a key press asks the window to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Let the search field have the key.
    Pass,
    /// The selection or scope changed; redraw.
    Moved,
    /// The scope changed; clear the query and search again.
    Rescoped,
    Open(Target),
    Close,
}

/// The open switcher: its server scope, results and selection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Picker {
    pub scope: Option<Uuid>,
    pub hits: Vec<Hit>,
    pub selected: usize,
}

impl Picker {
    /// Searches again and selects the best result.
    pub fn refresh(&mut self, index: &Index, query: &str, recents: &[Target]) {
        if self.scope.is_some_and(|id| index.server(id).is_none()) {
            self.scope = None;
        }
        self.hits = index.search(query, self.scope, recents);
        self.selected = 0;
    }

    pub fn select(&mut self, ix: usize) -> bool {
        let changed = ix < self.hits.len() && ix != self.selected;
        if changed {
            self.selected = ix;
        }
        changed
    }

    pub fn press(&mut self, key: Key, query_empty: bool) -> Outcome {
        let count = self.hits.len();
        match key {
            Key::Down | Key::Up if count == 0 => Outcome::Moved,
            Key::Down => {
                self.selected = (self.selected + 1) % count;
                Outcome::Moved
            }
            Key::Up => {
                self.selected = (self.selected + count - 1) % count;
                Outcome::Moved
            }
            Key::Enter => self
                .hits
                .get(self.selected)
                .map_or(Outcome::Moved, |hit| Outcome::Open(hit.target)),
            Key::Escape if self.scope.is_some() => {
                self.scope = None;
                Outcome::Rescoped
            }
            Key::Escape => Outcome::Close,
            Key::Tab => match self.hits.get(self.selected).map(|hit| hit.target) {
                Some(Target::Server { server }) => {
                    self.scope = Some(server);
                    Outcome::Rescoped
                }
                _ => Outcome::Moved,
            },
            Key::Backspace if query_empty && self.scope.is_some() => {
                self.scope = None;
                Outcome::Rescoped
            }
            Key::Backspace => Outcome::Pass,
        }
    }
}

/// Puts `target` first, keeping at most [`RECENTS`].
pub fn remember(recents: &mut Vec<Target>, target: Target) {
    recents.retain(|r| *r != target);
    recents.insert(0, target);
    recents.truncate(RECENTS);
}

/// What the switcher keeps across launches: recent items and each server's
/// last seen bots, so an offline server's bots still show.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    pub recents: Vec<Target>,
    pub bots: HashMap<Uuid, Vec<Bot>>,
}

impl Memory {
    pub fn load(path: &Path) -> Self {
        crate::config::read_private_limited(path, MAX_MEMORY_BYTES)
            .ok()
            .flatten()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Writes the memory, dropping the largest bot lists until it fits
    /// the size [`Memory::load`] accepts.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let encode = |memory: &Self| {
            serde_json::to_vec(memory)
                .map_err(|err| format!("Could not serialize the quick switcher: {err}"))
        };
        let mut bytes = encode(self)?;
        let mut kept = self.clone();
        while bytes.len() as u64 > MAX_MEMORY_BYTES {
            let Some(largest) = kept
                .bots
                .iter()
                .max_by_key(|(_, bots)| bots.len())
                .map(|(id, _)| *id)
            else {
                return Err("The quick switcher's memory is too large to save.".into());
            };
            kept.bots.remove(&largest);
            bytes = encode(&kept)?;
        }
        crate::config::replace_private(path, &bytes)
            .map_err(|err| format!("Could not save the quick switcher: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot(name: &str) -> Bot {
        Bot {
            id: Uuid::new_v4(),
            name: name.into(),
            avatar: Avatar::default(),
        }
    }

    fn server(name: &str, bots: &[&str]) -> Server {
        Server {
            id: Uuid::new_v4(),
            name: name.into(),
            bots: bots.iter().map(|b| bot(b)).collect(),
        }
    }

    fn index() -> Index {
        Index {
            servers: vec![
                server(
                    "This computer",
                    &["Development Assistant", "Code Reviewer", "Inbox"],
                ),
                server(
                    "Home server",
                    &["Research", "Home Automation", "Travel Planner"],
                ),
                server("Studio Mac", &["Render Farm", "Photo Librarian"]),
            ],
        }
    }

    fn names(index: &Index, hits: &[Hit]) -> Vec<String> {
        hits.iter()
            .map(|hit| match hit.target {
                Target::Server { server } => index.server(server).unwrap().name.clone(),
                Target::Bot { server, bot } => index.bot(server, bot).unwrap().1.name.clone(),
            })
            .collect()
    }

    #[test]
    fn a_prefix_beats_a_word_start_which_beats_a_match_inside_a_word() {
        let prefix = score("rev", "Reviewer").unwrap();
        let word = score("rev", "Code Reviewer").unwrap();
        let inside = score("rev", "Unreviewed").unwrap();
        let spread = score("cr", "Code Reviewer").unwrap();
        assert!(prefix.0 > word.0 && word.0 > inside.0 && inside.0 > spread.0);
        assert_eq!(word.1, vec![5, 6, 7]);
        assert_eq!(spread.1, vec![0, 5]);
    }

    #[test]
    fn spread_letters_find_a_match_a_greedy_pass_would_miss() {
        let (_, at) = score("rsc", "Research Studio").unwrap();
        assert_eq!(at, vec![0, 2, 6]);
        let (_, at) = score("hs", "Home server").unwrap();
        assert_eq!(at, vec![0, 5], "word starts win over letters inside words");
    }

    #[test]
    fn an_oversized_memory_drops_bot_lists_but_keeps_recents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switcher.json");
        let mut memory = Memory::default();
        let server = Uuid::new_v4();
        remember(&mut memory.recents, Target::Server { server });
        let small = Uuid::new_v4();
        memory.bots.insert(small, vec![bot("Kept")]);
        let long = "x".repeat(200);
        memory
            .bots
            .insert(server, (0..4000).map(|_| bot(&long)).collect());
        memory.save(&path).unwrap();
        let loaded = Memory::load(&path);
        assert_eq!(loaded.recents, memory.recents);
        assert!(!loaded.bots.contains_key(&server));
        assert_eq!(loaded.bots[&small], memory.bots[&small]);
    }

    #[test]
    fn spread_letters_must_start_on_a_word() {
        assert_eq!(score("home", "This computer"), None);
        assert_eq!(score("xyz", "Inbox"), None);
        assert!(score("da", "Development Assistant").is_some());
    }

    #[test]
    fn matching_ignores_case_and_highlights_by_char() {
        let (_, at) = score("ÉT", "Café Été").unwrap();
        assert_eq!(at, vec![5, 6]);
    }

    #[test]
    fn a_bot_matches_on_its_name_before_its_server() {
        let index = index();
        let hits = index.search("home", None, &[]);
        assert_eq!(
            names(&index, &hits)[..2],
            ["Home server".to_owned(), "Home Automation".to_owned()]
        );
        let research = hits
            .iter()
            .find(|h| names(&index, std::slice::from_ref(h))[0] == "Research")
            .expect("bots match on their server's name");
        assert_eq!(research.server, vec![0, 1, 2, 3]);
        assert!(research.name.is_empty());
    }

    fn owners_index() -> Index {
        Index {
            servers: vec![
                server(
                    "Proliquid",
                    &[
                        "Proliquid",
                        "Tech Manager",
                        "Sentry",
                        "Dev",
                        "Auto",
                        "Release",
                        "Telegram",
                        "Discord",
                        "Twitter",
                        "Inbox",
                        "DevOps",
                        "Marketing Manager",
                        "Bento",
                        "Scout",
                        "Spy",
                        "Canvas Manager",
                    ],
                ),
                server(
                    "Home",
                    &[
                        "Jarvis",
                        "Fleet",
                        "Else",
                        "Inbox",
                        "AliExpress",
                        "WhatsApp",
                        "Amazon",
                        "Quant",
                    ],
                ),
            ],
        }
    }

    #[test]
    fn several_words_name_a_server_then_a_bot_on_it() {
        let index = owners_index();
        for query in ["pro tech", "p t", "p tm", "Proliquid tech man"] {
            let hits = index.search(query, None, &[]);
            assert_eq!(names(&index, &hits)[0], "Tech Manager", "{query}");
            assert_eq!(hits[0].target.server(), index.servers[0].id, "{query}");
        }
        let hits = index.search("pro tech", None, &[]);
        assert_eq!(hits[0].server, vec![0, 1, 2]);
        assert_eq!(hits[0].name, vec![0, 1, 2, 3]);
        let hits = index.search("p tm", None, &[]);
        assert_eq!(hits[0].name, vec![0, 5]);
        let hits = index.search("  pro   tech ", None, &[]);
        assert_eq!(names(&index, &hits)[0], "Tech Manager");
    }

    #[test]
    fn a_split_needs_each_part_on_a_word_start() {
        let index = owners_index();
        for query in ["liquid tech", "pd tech", "pro anager"] {
            assert!(
                !index
                    .search(query, None, &[])
                    .iter()
                    .any(|hit| !hit.server.is_empty() && !hit.name.is_empty()),
                "{query}"
            );
        }
        let index = Index {
            servers: vec![
                server("Team Chat", &["Main"]),
                server("Home", &["Tech Manager"]),
            ],
        };
        let hits = index.search("tech man", None, &[]);
        assert_eq!(names(&index, &hits)[0], "Tech Manager");
    }

    #[test]
    fn a_name_starting_with_the_whole_query_beats_a_split() {
        let index = index();
        for query in ["this c", "studio m"] {
            let hits = index.search(query, None, &[]);
            assert!(
                matches!(hits[0].target, Target::Server { .. }),
                "{query}: {:?}",
                names(&index, &hits)
            );
        }
    }

    #[test]
    fn several_words_still_match_within_one_name() {
        let index = owners_index();
        let hits = index.search("tech  man", None, &[]);
        assert_eq!(names(&index, &hits)[0], "Tech Manager");
        let hits = index.search("pro tech", Some(index.servers[0].id), &[]);
        assert!(
            hits.is_empty(),
            "a scope matches the whole query to the bot"
        );
        let hits = index.search("tech man", Some(index.servers[0].id), &[]);
        assert_eq!(names(&index, &hits), ["Tech Manager"]);
    }

    #[test]
    fn a_server_word_prefers_the_recent_server() {
        let mut index = owners_index();
        index.servers.push(server("Personal", &["Tasks"]));
        let plain = index.search("p t", None, &[]);
        assert_eq!(names(&index, &plain)[..2], ["Tasks", "Tech Manager"]);
        let sentry = Target::Bot {
            server: index.servers[0].id,
            bot: index.servers[0].bots[2].id,
        };
        let warm = index.search("p t", None, &[sentry]);
        assert_eq!(
            names(&index, &warm)[..4],
            ["Tech Manager", "Telegram", "Twitter", "Tasks"]
        );
    }

    #[test]
    fn offline_servers_bots_are_found() {
        let index = index();
        let hits = index.search("render", None, &[]);
        assert_eq!(names(&index, &hits), ["Render Farm"]);
        assert_eq!(hits[0].target.server(), index.servers[2].id);
    }

    #[test]
    fn recent_items_break_ties_and_lead_an_empty_query() {
        let index = index();
        let studio = index.servers[2].id;
        let farm = index.servers[2].bots[0].id;
        let research = Target::Bot {
            server: index.servers[1].id,
            bot: index.servers[1].bots[0].id,
        };
        let recents = vec![
            Target::Bot {
                server: studio,
                bot: farm,
            },
            Target::Server { server: studio },
            research,
        ];
        assert_eq!(
            names(&index, &index.search("", None, &recents)),
            ["Render Farm", "Studio Mac", "Research"]
        );
        let plain = index.search("r", None, &[]);
        let warm = index.search("r", None, &recents);
        let at = |hits: &[Hit], name: &str| names(&index, hits).iter().position(|n| n == name);
        assert!(at(&warm, "Render Farm") < at(&plain, "Render Farm"));
    }

    #[test]
    fn a_scope_lists_only_that_servers_bots() {
        let index = index();
        let home = index.servers[1].id;
        assert_eq!(
            names(&index, &index.search("", Some(home), &[])),
            ["Home Automation", "Research", "Travel Planner"]
        );
        assert_eq!(
            names(&index, &index.search("home", Some(home), &[])),
            ["Home Automation"]
        );
    }

    #[test]
    fn no_match_is_empty() {
        assert!(index().search("zzq", None, &[]).is_empty());
    }

    #[test]
    fn arrows_wrap_and_enter_opens_the_selection() {
        let index = index();
        let mut picker = Picker::default();
        picker.refresh(&index, "re", &[]);
        let count = picker.hits.len();
        assert!(count > 2);
        assert_eq!(picker.press(Key::Up, false), Outcome::Moved);
        assert_eq!(picker.selected, count - 1);
        assert_eq!(picker.press(Key::Down, false), Outcome::Moved);
        assert_eq!(picker.press(Key::Down, false), Outcome::Moved);
        assert_eq!(picker.selected, 1);
        assert_eq!(
            picker.press(Key::Enter, false),
            Outcome::Open(picker.hits[1].target)
        );
        picker.refresh(&index, "re", &[]);
        assert_eq!(picker.selected, 0, "a new search selects the best result");
    }

    #[test]
    fn enter_with_no_results_opens_nothing() {
        let mut picker = Picker::default();
        picker.refresh(&index(), "zzq", &[]);
        assert_eq!(picker.press(Key::Down, false), Outcome::Moved);
        assert_eq!(picker.press(Key::Enter, false), Outcome::Moved);
    }

    #[test]
    fn tab_scopes_to_a_server_and_escape_or_backspace_leaves_it() {
        let index = index();
        let home = index.servers[1].id;
        let mut picker = Picker::default();
        picker.refresh(&index, "home", &[]);
        assert_eq!(picker.hits[0].target, Target::Server { server: home });
        assert_eq!(picker.press(Key::Tab, false), Outcome::Rescoped);
        assert_eq!(picker.scope, Some(home));
        picker.refresh(&index, "", &[]);
        assert_eq!(picker.hits.len(), 3);
        assert_eq!(
            picker.press(Key::Tab, true),
            Outcome::Moved,
            "a bot has no scope"
        );
        assert_eq!(picker.press(Key::Backspace, false), Outcome::Pass);
        assert_eq!(picker.press(Key::Backspace, true), Outcome::Rescoped);
        assert_eq!(picker.scope, None);
        assert_eq!(picker.press(Key::Backspace, true), Outcome::Pass);
        picker.scope = Some(home);
        assert_eq!(picker.press(Key::Escape, false), Outcome::Rescoped);
        assert_eq!(picker.press(Key::Escape, false), Outcome::Close);
    }

    #[test]
    fn a_removed_scoped_server_drops_the_scope() {
        let mut index = index();
        let gone = index.servers.remove(2).id;
        let mut picker = Picker {
            scope: Some(gone),
            ..Picker::default()
        };
        picker.refresh(&index, "", &[]);
        assert_eq!(picker.scope, None);
    }

    #[test]
    fn remembering_moves_an_item_first_and_caps_the_list() {
        let mut recents = Vec::new();
        let targets: Vec<Target> = (0..RECENTS + 2)
            .map(|_| Target::Server {
                server: Uuid::new_v4(),
            })
            .collect();
        for target in &targets {
            remember(&mut recents, *target);
        }
        remember(&mut recents, targets[RECENTS + 1 - 3]);
        assert_eq!(recents.len(), RECENTS);
        assert_eq!(recents[0], targets[RECENTS - 2]);
        assert_eq!(recents[1], targets[RECENTS + 1]);
    }

    #[test]
    fn memory_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("switcher.json");
        let index = index();
        let mut memory = Memory::default();
        remember(
            &mut memory.recents,
            Target::Server {
                server: index.servers[0].id,
            },
        );
        memory
            .bots
            .insert(index.servers[2].id, index.servers[2].bots.clone());
        memory.save(&path).unwrap();
        assert_eq!(Memory::load(&path), memory);
        assert_eq!(
            Memory::load(&dir.path().join("missing.json")),
            Memory::default()
        );
    }
}
