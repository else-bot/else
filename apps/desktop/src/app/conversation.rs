use super::ElseApp;
use super::reply::Author;
use super::row_text::RowText;
use super::task_reply::state::ReplyState;
use super::transcript::{self, Folds, RowKey, Transcript, TranscriptRow, group_open};
use crate::api::{ClientError, LiveCommand, ReplyPosition, StreamMessage};
use crate::model::{Applied, Conversation, Snapshot};
use chrono::Local;
use else_wire::bots::Phase;
use else_wire::items::ItemKind;
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(super) struct ReplyLayout {
    pub position: crate::api::ReplyPosition,
    pub bounds: Bounds<Pixels>,
}

/// Where the unread divider sits, and the timer that hides it once the
/// owner has seen the replies below it.
pub(super) struct Unread {
    pub(super) since: ReplyPosition,
    pub(super) dismissal: Option<Task<()>>,
}

impl Unread {
    pub(super) fn since(since: ReplyPosition) -> Self {
        Self {
            since,
            dismissal: None,
        }
    }
}

pub(super) struct ConversationView {
    pub(super) bot_id: Uuid,
    pub(super) unread: Option<Unread>,
    pub(super) conversation: Option<Conversation>,
    pub(super) stream_problem: Option<String>,
    /// The older page being loaded.
    pub(super) older: Option<Task<()>>,
    pub(super) rows: Vec<TranscriptRow>,
    pub(super) list: ListState,
    pub(super) texts: HashMap<RowKey, RowText>,
    pub(super) cards: super::cards::Cards,
    /// Where cards that can pause the bot were last painted, so the paused
    /// strip can step aside while one is on screen.
    pub(super) sightings: super::question_card::Sightings,
    pub(super) expanded_tasks: HashSet<Uuid>,
    /// Replies whose folded earlier updates are open.
    pub(super) expanded_folds: HashSet<Uuid>,
    /// Delegation groups whose task details are expanded.
    pub(super) expanded_councils: HashSet<Uuid>,
    pub(super) expanded_events: HashSet<Uuid>,
    /// Bot exchanges the owner folded or unfolded, over how each shows by
    /// default.
    pub(super) exchange_folds: HashMap<Uuid, super::bot_activity::Fold>,
    /// The exchange the owner just unfolded: once laid out, its header moves
    /// to the top unless the whole exchange already shows.
    pub(super) landing: Option<Uuid>,
    /// Exchange replies shown in full instead of their excerpt.
    pub(super) full_replies: HashSet<Uuid>,
    /// No-news runs ([`RowKey::QuietRun`]) and groups of them
    /// ([`RowKey::QuietRuns`]) the owner opened.
    pub(super) open_quiet: HashSet<RowKey>,
    /// The waiting tray lists its waits instead of only its header.
    pub(super) tray_expanded: bool,
    /// Waits in the tray whose details are open.
    pub(super) expanded_waits: HashSet<Uuid>,
    /// Waits the owner asked to stop that the server has not settled yet.
    pub(super) stopping_waits: HashSet<Uuid>,
    pub(super) reply_transitions: HashMap<Uuid, std::time::Instant>,
    pub(super) _reply_clock: Option<Task<()>>,
    pub(super) latest_seen: bool,
    pub(super) reply_layout: Option<ReplyLayout>,
    /// Where the last "go to original message" landed.
    pub(super) revealed: Option<Reveal>,
    _reveal_clock: Option<Task<()>>,
    /// A quoted message to find once older history arrives.
    pending_reveal: Option<(RowKey, Author, String)>,
    /// The open find bar.
    pub(super) find: Option<super::find::Find>,
    /// The gap row being loaded, by the loaded item just above it.
    pub(super) loading_gap: Option<(i64, Task<()>)>,
    pub(super) load: Load,
}

/// Where a view stands with the server's newest page.
pub(super) enum Load {
    /// Its conversation, if any, is the server's and follows the stream.
    Settled,
    Running {
        _task: Task<()>,
    },
    /// The last load or stream event failed; a shown view retries after
    /// a pause, a warm one when shown again.
    Failed {
        _retry: Option<Task<()>>,
    },
}

/// A message the owner jumped to, flashed until it fades out.
pub(super) struct Reveal {
    /// The row that holds it.
    pub(super) row: RowKey,
    pub(super) at: std::time::Instant,
    pub(super) spot: Spot,
    /// How strongly the highlight paints, as of the last tick, so every
    /// paint until the next tick gives the same color.
    pub(super) shown: f32,
}

pub(super) enum Spot {
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
    pub(super) fn new(row: RowKey, spot: Spot) -> Self {
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
    pub(super) fn row_index(&self, key: RowKey) -> Option<usize> {
        let task = match key {
            RowKey::Item(id) => self
                .conversation
                .as_ref()
                .and_then(|c| c.item(id))
                .map(|i| RowKey::Task(i.turn_id)),
            _ => None,
        };
        self.rows
            .iter()
            .position(|row| row.contains(key))
            .or_else(|| task.and_then(|task| self.rows.iter().position(|row| row.key == task)))
    }

    pub(super) fn new(bot_id: Uuid) -> Self {
        let list = ListState::new(0, ListAlignment::Bottom, px(400.));
        list.set_follow_mode(FollowMode::Tail);
        Self {
            bot_id,
            unread: None,
            conversation: None,
            stream_problem: None,
            older: None,
            rows: Vec::new(),
            list,
            texts: HashMap::new(),
            cards: Default::default(),
            sightings: Default::default(),
            expanded_tasks: HashSet::new(),
            expanded_folds: HashSet::new(),
            expanded_councils: HashSet::new(),
            expanded_events: HashSet::new(),
            exchange_folds: HashMap::new(),
            landing: None,
            full_replies: HashSet::new(),
            open_quiet: HashSet::new(),
            tray_expanded: true,
            expanded_waits: HashSet::new(),
            stopping_waits: HashSet::new(),
            reply_transitions: HashMap::new(),
            _reply_clock: None,
            latest_seen: false,
            reply_layout: None,
            revealed: None,
            _reveal_clock: None,
            pending_reveal: None,
            find: None,
            loading_gap: None,
            load: Load::Settled,
        }
    }

    /// Settles what only the shown view finishes, before switching away:
    /// older pages and gaps that were loading, a pending jump and the
    /// unread divider. Its newest page keeps loading.
    pub(super) fn park(&mut self) {
        self.cards.park();
        self._reply_clock = None;
        self.unread = None;
        self.older = None;
        self.loading_gap = None;
        self.pending_reveal = None;
        self.revealed = None;
        self._reveal_clock = None;
        self.landing = None;
    }
}

impl ElseApp {
    pub(super) fn remeasure_row(&self, key: RowKey, cx: &mut Context<Self>) {
        if let Some(view) = self.view.as_ref()
            && let Some(ix) = view.row_index(key)
        {
            view.list.remeasure_items(ix..ix + 1);
        }
        cx.notify();
    }

    pub(super) fn load_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bot) = self.view.as_ref().map(|view| view.bot_id) {
            self.load_into(bot, window, cx);
        }
    }

    /// Loads `bot`'s newest page into its view, shown or warm. A view with
    /// nothing to draw shows the saved page while the server's loads.
    pub(super) fn load_into(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let (client, saved) = (self.client.clone(), self.saved.clone());
        let Some(view) = self.view_of(bot) else {
            return;
        };
        let cold = view.conversation.is_none();
        view.load = Load::Running {
            _task: cx.spawn_in(window, async move |this, cx| {
                let fetch = cx.background_executor().spawn(client.latest(bot));
                if cold
                    && let Some(saved) = saved.clone()
                    && let Some(page) = cx
                        .background_executor()
                        .spawn(async move { saved.load(bot) })
                        .await
                {
                    let _ = this
                        .update_in(cx, |app, window, cx| app.apply_saved(bot, page, window, cx));
                }
                let snapshot = fetch.await.map(|crate::api::Latest { snapshot, raw }| {
                    if let Some(saved) = saved {
                        let ticket = saved.ticket();
                        cx.background_executor()
                            .spawn(async move {
                                if let Err(err) = saved.save(bot, &raw, ticket) {
                                    eprintln!("{err}");
                                }
                            })
                            .detach();
                    }
                    snapshot
                });
                let _ = this.update_in(cx, |app, window, cx| {
                    if let Some(cursor) = app.apply_snapshot(bot, snapshot, window, cx) {
                        app.live_command(LiveCommand::Subscribe { bot, cursor });
                    }
                });
            }),
        };
    }

    /// Shows the saved page of a view still waiting for its first one.
    fn apply_saved(
        &mut self,
        bot: Uuid,
        page: Snapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self.shows(bot);
        let Some(view) = self.view_of(bot).filter(|v| v.conversation.is_none()) else {
            return;
        };
        let Ok(conversation) = Conversation::from_saved(page) else {
            return;
        };
        view.conversation = Some(conversation);
        if shown {
            self.sync_rows(window, cx);
        }
    }

    /// Applies `bot`'s newest page to its view, and returns the cursor to
    /// follow if the view is shown.
    fn apply_snapshot(
        &mut self,
        bot: Uuid,
        snapshot: Result<Snapshot, ClientError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let shown = self.shows(bot);
        let view = self.view_of(bot)?;
        view.load = Load::Failed { _retry: None };
        match snapshot.map(Conversation::from_snapshot) {
            Ok(Ok(conversation)) => {
                let cursor = conversation.cursor();
                view.conversation = Some(conversation);
                view.load = Load::Settled;
                view.loading_gap = None;
                view.stream_problem = None;
                if !shown {
                    return None;
                }
                self.sync_rows(window, cx);
                self.ensure_rules(bot, window, cx);
                Some(cursor)
            }
            Ok(Err(_)) => {
                view.stream_problem = Some("The server sent an invalid snapshot.".into());
                cx.notify();
                None
            }
            Err(ClientError::Api { status: 410, .. }) => {
                self.refresh_now(window, cx);
                None
            }
            Err(err) => {
                view.stream_problem = Some(format!("Could not load history: {err}"));
                cx.notify();
                if shown {
                    view.load = Load::Failed {
                        _retry: Some(cx.spawn_in(window, async move |this, cx| {
                            cx.background_executor().timer(Duration::from_secs(2)).await;
                            let _ = this.update_in(cx, |app, window, cx| {
                                if app.shows(bot) {
                                    app.load_conversation(window, cx);
                                }
                            });
                        })),
                    };
                }
                None
            }
        }
    }

    pub(super) fn apply_stream(
        &mut self,
        bot: Uuid,
        message: StreamMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A warm view keeps applying what arrives until its unsubscribe
        // lands, so the cursor it resumes from is the one it applied.
        let shown = self.shows(bot);
        let Some(view) = self.view_of(bot) else {
            return;
        };
        let failed = match message {
            StreamMessage::Frame(frame) => {
                let Some(conversation) = view.conversation.as_mut() else {
                    return;
                };
                match conversation.apply(&frame) {
                    Ok(Applied::Duplicate) => false,
                    // The bot list stays authoritative for a warm bot's
                    // activity: its last frames can trail the list.
                    Ok(Applied::Changed) if !shown => {
                        self.prune_pending(bot);
                        false
                    }
                    Ok(Applied::Changed) => {
                        let activity = conversation.activity.clone();
                        if activity.as_ref().is_some_and(|a| a.phase == Phase::Idle) {
                            self.stopping.remove(&bot);
                        }
                        if let Some(entry) = self.bots.iter_mut().find(|b| b.id == bot)
                            && activity.is_some()
                        {
                            entry.activity = activity;
                        }
                        self.sync_rows(window, cx);
                        self.ensure_rules(bot, window, cx);
                        false
                    }
                    Err(_) => true,
                }
            }
            StreamMessage::Reset => true,
            StreamMessage::Deleted => {
                self.refresh_now(window, cx);
                false
            }
        };
        if !failed {
            return;
        }
        // Dropped so the reloaded page's cursor subscribes anew, even when
        // it equals the one asked for before.
        self.live_command(LiveCommand::Unsubscribe { bot });
        if shown {
            self.load_conversation(window, cx);
        } else if let Some(view) = self.view_of(bot)
            && matches!(view.load, Load::Settled)
        {
            view.load = Load::Failed { _retry: None };
        }
    }

    /// Drops `bot`'s pending sends its conversation now holds.
    pub(super) fn prune_pending(&mut self, bot: Uuid) {
        let view = match self.view.as_ref() {
            Some(view) if view.bot_id == bot => Some(view),
            _ => self.warm.get(bot),
        };
        if let (Some(conversation), Some(pending)) = (
            view.and_then(|v| v.conversation.as_ref()),
            self.pending.get_mut(&bot),
        ) {
            pending.retain(|p| !conversation.has_request(p.request_id));
        }
    }

    /// Scrolls to the message a reply quotes, loading older pages until it
    /// is found or the history runs out.
    pub(super) fn reveal_quoted(
        &mut self,
        bot: Uuid,
        from: RowKey,
        author: Author,
        original: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.view.as_mut().filter(|v| v.bot_id == bot) else {
            return;
        };
        view.pending_reveal = None;
        let Some(conversation) = view.conversation.as_ref() else {
            return;
        };
        let before = match from {
            RowKey::Item(id) => conversation.item(id).map(|item| item.seq),
            _ => None,
        };
        // A whole-message match anywhere beats a partial one in a later message.
        let target = [super::reply::quotes_whole, super::reply::quotes_part]
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
        let Some(view) = self.view.as_mut() else {
            return;
        };
        if let Some((target, ix)) = found
            && let Some(conversation) = view.conversation.as_ref()
        {
            view.list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.),
            });
            let item = RowKey::Item(target);
            let spot = if view.texts.get(&item).is_some_and(RowText::is_markdown) {
                let whole = conversation
                    .item(target)
                    .is_some_and(|item| super::reply::quotes_whole(&item.text, original));
                Spot::Passage {
                    item,
                    quote: (!whole).then(|| original.to_owned()),
                    centered: false,
                }
            } else {
                Spot::Row
            };
            let row = view.rows[ix].key;
            self.start_reveal(Reveal::new(row, spot), cx);
        } else if older {
            view.pending_reveal = Some((from, author, original.to_owned()));
            self.load_older(window, cx);
        } else {
            window.push_notification(
                "The original message is no longer in this conversation.",
                cx,
            );
        }
    }

    /// Shows item `target` whole, opening a folded bot exchange or a task's
    /// earlier replies, and returns the index of the row that holds it.
    pub(super) fn unfold_item(
        &mut self,
        target: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.open_quiet_run_of(target, window, cx);
        let view = self.view.as_mut()?;
        let conversation = view.conversation.as_ref()?;
        // An exchange reply may be folded or cut to an excerpt.
        let folded = !view.full_replies.contains(&target)
            && view.rows.iter().any(|row| {
                row.activity
                    .as_ref()
                    .is_some_and(|activity| activity.answers.contains(&target))
            });
        if folded {
            if let Some(receipt) = conversation
                .item(target)
                .and_then(|item| conversation.task_receipt(item.turn_id))
            {
                view.exchange_folds
                    .insert(receipt.id, super::bot_activity::Fold::Open);
            }
            view.full_replies.insert(target);
            self.sync_rows(window, cx);
        }
        let view = self.view.as_mut()?;
        let conversation = view.conversation.as_ref()?;
        let ix = view.row_index(RowKey::Item(target))?;
        if let RowKey::Item(reply) = view.rows[ix].key
            && view.rows[ix].folded.contains(&target)
        {
            view.expanded_folds.insert(reply);
            view.list.remeasure_items(ix..ix + 1);
        }
        if let Some(task) = &view.rows[ix].task
            && (task.history.contains(&target)
                || (task.kept.contains(&target) && task.reply_superseded(conversation, target))
                || (task.latest == Some(target)
                    && !task.latest_kept
                    && matches!(task.state, ReplyState::Queued | ReplyState::Working)))
        {
            view.expanded_tasks.insert(task.turn);
            view.list.remeasure_items(ix..ix + 1);
        }
        Some(ix)
    }

    /// Opens the no-news run holding item `target`, and the group of runs
    /// it folds into, so a find match inside it shows.
    fn open_quiet_run_of(&mut self, target: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let Some(turn) = view
            .conversation
            .as_ref()
            .and_then(|c| c.item(target))
            .map(|item| item.turn_id)
            .filter(|&turn| view.conversation.as_ref().is_some_and(|c| c.no_news(turn)))
        else {
            return;
        };
        if view.open_quiet.insert(RowKey::QuietRun(turn)) {
            self.sync_rows(window, cx);
        }
    }

    /// Shows or folds a no-news run, or a group of them.
    pub(super) fn toggle_quiet(
        &mut self,
        key: RowKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let runs = view
            .rows
            .iter()
            .find(|row| row.key == key)
            .map(|row| row.quiet_runs.clone())
            .unwrap_or_default();
        if !runs.is_empty() && group_open(&runs, &view.open_quiet) {
            for turn in runs {
                view.open_quiet.remove(&RowKey::QuietRuns(turn));
                view.open_quiet.remove(&RowKey::QuietRun(turn));
            }
        } else if !view.open_quiet.remove(&key) {
            view.open_quiet.insert(key);
        }
        self.sync_rows(window, cx);
        // The rows below a folded or unfolded run keep their keys, but their
        // spacing depends on the row above.
        if let Some(view) = self.view.as_ref() {
            view.list.remeasure();
        }
    }

    /// Flashes `reveal` until it fades out, painting its passage on each
    /// frame and asking the list to center the passage once it is found.
    pub(super) fn start_reveal(&mut self, reveal: Reveal, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let previous = view.revealed.replace(reveal);
        if let Some(Reveal {
            spot: Spot::Passage { item, .. },
            ..
        }) = previous
        {
            self.paint_row_quotes(item, cx);
        }
        self.tick_reveal(cx);
        let Some(view) = self.view.as_mut() else {
            return;
        };
        view._reveal_clock = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let live = this.update(cx, |app, cx| app.tick_reveal(cx));
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
        let Some(view) = self.view.as_mut() else {
            return false;
        };
        let Some(reveal) = view.revealed.as_mut() else {
            return false;
        };
        reveal.shown = reveal.strength();
        let live = reveal.shown > 0.;
        let passage = match &reveal.spot {
            Spot::Passage { item, .. } => Some(*item),
            Spot::Row => None,
        };
        if !live {
            view.revealed = None;
        }
        match passage {
            Some(item) => self.paint_row_quotes(item, cx),
            None => cx.notify(),
        }
        live
    }

    pub(super) fn load_older(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let Some(before) = view.conversation.as_ref().and_then(|c| c.older_before_seq) else {
            return;
        };
        if view.older.is_some() {
            return;
        }
        let bot = view.bot_id;
        view.older = Some(cx.spawn_in(window, async move |this, cx| {
            let page = client.snapshot(bot, Some(before)).await;
            let _ = this.update_in(cx, |app, window, cx| {
                let Some(view) = app.view_of(bot) else {
                    return;
                };
                view.older = None;
                match page {
                    Ok(page) => {
                        if let Some(conversation) = view.conversation.as_mut() {
                            conversation.merge_older(page);
                        }
                        let pending = view.pending_reveal.take();
                        app.sync_rows(window, cx);
                        if let Some((from, author, original)) = pending {
                            app.reveal_quoted(bot, from, author, &original, window, cx);
                        }
                    }
                    Err(err) => {
                        app.notice = Some(format!("Could not load older messages: {err}"));
                        cx.notify();
                    }
                }
            });
        }));
        cx.notify();
    }

    pub(super) fn sync_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_settings_changes();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let bot = view.bot_id;
        if self.bots.iter().any(|b| b.id == bot && b.silenced()) {
            view.unread = None;
        }
        let conversation = view.conversation.as_ref();
        if let (Some(pending), Some(conversation)) = (self.pending.get_mut(&bot), conversation) {
            pending.retain(|p| !conversation.has_request(p.request_id));
        }
        let Transcript {
            rows,
            texts,
            tasks,
            ticking,
        } = transcript::build(
            conversation,
            self.pending.get(&bot).map_or(&[], Vec::as_slice),
            &Folds {
                quiet: &view.open_quiet,
                full_replies: &view.full_replies,
            },
            view.unread.as_ref().map(|unread| unread.since),
            &Local,
        );
        let old = std::mem::take(&mut view.rows);
        view.reply_transitions
            .retain(|_, at| at.elapsed() < Duration::from_millis(320));
        for task in tasks.values() {
            if task.state.terminal()
                && old.iter().any(|row| {
                    row.task
                        .as_ref()
                        .is_some_and(|prior| prior.turn == task.turn && !prior.state.terminal())
                })
            {
                view.reply_transitions
                    .insert(task.turn, std::time::Instant::now());
                if task.state == ReplyState::Complete {
                    view.expanded_tasks.remove(&task.turn);
                }
            }
        }
        let same = |a: &TranscriptRow, b: &TranscriptRow| a.key == b.key;
        let prefix = old
            .iter()
            .zip(&rows)
            .take_while(|(a, b)| same(a, b))
            .count();
        let max_suffix = old.len().min(rows.len()) - prefix;
        let suffix = old
            .iter()
            .rev()
            .zip(rows.iter().rev())
            .take(max_suffix)
            .take_while(|(a, b)| same(a, b))
            .count();
        if prefix + suffix != old.len() || old.len() != rows.len() {
            view.list
                .splice(prefix..old.len() - suffix, rows.len() - prefix - suffix);
        }
        let previous: HashMap<_, _> = old.iter().map(|row| (row.key, row)).collect();
        for (ix, row) in rows.iter().enumerate() {
            if previous.get(&row.key).is_some_and(|old| *old != row) {
                view.list.remeasure_items(ix..ix + 1);
            }
        }
        let mut changed = Vec::new();
        let mut created = false;
        let text_keys: HashSet<_> = texts.iter().map(|(key, ..)| *key).collect();
        for (key, source) in texts {
            match view.texts.get_mut(&key) {
                Some(row) if row.format() == source.format => {
                    if row.sync(source, &self.mention_bots, window, cx) {
                        changed.push(key);
                    }
                }
                _ => {
                    view.texts.insert(
                        key,
                        RowText::new(
                            key,
                            source,
                            self.client.clone(),
                            &self.mention_bots,
                            bot,
                            window,
                            cx,
                        ),
                    );
                    created = true;
                }
            }
        }
        view.texts.retain(|key, _| text_keys.contains(key));
        if created {
            // New text views learn their wrapped height during their first
            // layout; measure the rows again once that has happened.
            let list = view.list.clone();
            window.on_next_frame(move |window, _| {
                let list = list.clone();
                window.on_next_frame(move |window, _| {
                    list.remeasure();
                    window.refresh();
                });
            });
        }
        for key in changed {
            let key = match key {
                RowKey::Item(id) => conversation
                    .and_then(|c| c.item(id))
                    .filter(|item| {
                        item.kind == ItemKind::Assistant && tasks.contains_key(&item.turn_id)
                    })
                    .map_or(key, |item| RowKey::Task(item.turn_id)),
                _ => key,
            };
            if let Some(ix) = rows.iter().position(|r| r.contains(key)) {
                view.list.remeasure_items(ix..ix + 1);
            }
        }
        view.rows = rows;
        if created {
            super::quotes::paint_view(view, &self.drafts, cx);
        }
        if !ticking {
            view._reply_clock = None;
        } else if view._reply_clock.is_none() {
            view._reply_clock = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let keep = this.update(cx, |app, cx| {
                        let ticking = app
                            .view
                            .as_ref()
                            .filter(|view| view.bot_id == bot)
                            .is_some_and(|view| {
                                transcript::ticking(
                                    view.rows.iter().filter_map(|row| row.task.as_ref()),
                                    view.conversation.as_ref(),
                                )
                            });
                        if ticking {
                            cx.notify();
                        }
                        ticking
                    });
                    if !matches!(keep, Ok(true)) {
                        break;
                    }
                }
            }));
        }
        self.sync_cards(window, cx);
        self.check_reading(window, cx);
        cx.notify();
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
