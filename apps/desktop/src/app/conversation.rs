use super::ElseApp;
use super::bot_mention::MentionRegistry;
use super::pending_send::PendingSend;
use super::reply::{Author, Quote};
use super::request::Settle;
use super::row_text::{RowHost, RowText};
use super::task_reply::state::ReplyState;
use super::transcript::{self, CardKey, Folds, RowKey, Transcript, TranscriptRow, group_open};
use crate::api::{Client, ClientError, LiveCommand, StreamMessage};
use crate::model::{Applied, Conversation};
use chrono::Local;
use else_wire::bots::ReplyPosition;
use else_wire::bots::{Bot, Phase};
use else_wire::conversation::Snapshot;
use else_wire::items::ItemKind;
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(super) struct ReplyLayout {
    pub position: else_wire::bots::ReplyPosition,
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

/// What the transcript reads from the window. The window builds it after
/// each of its notifies, and the view redraws only when it changed, so each
/// field stays cheap to clone and compare.
#[derive(Clone, Default, PartialEq)]
pub(super) struct TranscriptInputs {
    pub(super) connected: bool,
    pub(super) hidden: bool,
    pub(super) bots: Arc<[Bot]>,
    pub(super) mention_bots: Arc<MentionRegistry>,
    pub(super) pending: Vec<PendingSend>,
    pub(super) quotes: Vec<Quote>,
    pub(super) restoring_rules: bool,
}

/// Whether the window shows a view or keeps it warm. A warm view keeps its
/// conversation current but builds no rows or cards until it is shown.
enum Presence {
    Shown,
    Parked,
}

/// One bot's transcript: its rows, their cards and the list they scroll in.
pub(super) struct ConversationView {
    pub(super) bot_id: Uuid,
    presence: Presence,
    pub(super) app: WeakEntity<ElseApp>,
    pub(super) client: Arc<Client>,
    pub(super) inputs: TranscriptInputs,
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
        row_index(&self.rows, self.conversation.as_ref(), key)
    }

    pub(super) fn new(
        bot_id: Uuid,
        app: WeakEntity<ElseApp>,
        client: Arc<Client>,
        inputs: TranscriptInputs,
        cx: &mut Context<Self>,
    ) -> Self {
        let list = ListState::new(0, ListAlignment::Bottom, px(400.));
        list.set_follow_mode(FollowMode::Tail);
        let (view, owner) = (cx.entity().downgrade(), app.clone());
        list.set_scroll_handler(move |_, window, cx| {
            // The owner scrolled, so an exchange still laying out no longer
            // moves the view when it lands.
            let _ = view.update(cx, |view, _| view.landing = None);
            let _ = owner.update(cx, |app, cx| app.check_reading(window, cx));
        });
        Self {
            bot_id,
            presence: Presence::Parked,
            app,
            client,
            inputs,
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
    pub(super) fn show(&mut self) {
        self.presence = Presence::Shown;
    }

    pub(super) fn shown(&self) -> bool {
        matches!(self.presence, Presence::Shown)
    }

    pub(super) fn park(&mut self) {
        self.presence = Presence::Parked;
        self.park_find();
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
    pub(super) fn load_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(bot) = self.shown_bot(cx) {
            self.load_into(bot, window, cx);
        }
    }

    /// Loads `bot`'s newest page into its view, shown or warm. A view with
    /// nothing to draw shows the saved page while the server's loads.
    pub(super) fn load_into(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let (client, saved) = (self.client.clone(), self.saved.clone());
        let Some(view) = self.view_of(bot, cx) else {
            return;
        };
        let cold = view.read(cx).conversation.is_none();
        let task = cx.spawn_in(window, async move |this, cx| {
            let fetch = cx.background_executor().spawn(client.latest(bot));
            if cold
                && let Some(saved) = saved.clone()
                && let Some(page) = cx
                    .background_executor()
                    .spawn(async move { saved.load(bot) })
                    .await
            {
                let _ =
                    this.update_in(cx, |app, window, cx| app.apply_saved(bot, page, window, cx));
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
        });
        view.update(cx, |view, _| view.load = Load::Running { _task: task });
    }

    /// Shows the saved page of a view still waiting for its first one.
    fn apply_saved(
        &mut self,
        bot: Uuid,
        page: Snapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let shown = self.shows(bot, cx);
        let Some(view) = self
            .view_of(bot, cx)
            .filter(|view| view.read(cx).conversation.is_none())
        else {
            return;
        };
        let Ok(conversation) = Conversation::from_saved(page) else {
            return;
        };
        view.update(cx, |view, _| view.conversation = Some(conversation));
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
        let shown = self.shows(bot, cx);
        let view = self.view_of(bot, cx)?;
        let failed = |problem: String, retry: Option<Task<()>>, cx: &mut Context<Self>| {
            view.update(cx, |view, _| {
                view.stream_problem = Some(problem);
                view.load = Load::Failed { _retry: retry };
            });
            cx.notify();
        };
        match snapshot.map(Conversation::from_snapshot) {
            Ok(Ok(conversation)) => {
                let cursor = conversation.cursor();
                view.update(cx, |view, _| {
                    view.conversation = Some(conversation);
                    view.load = Load::Settled;
                    view.loading_gap = None;
                    view.stream_problem = None;
                });
                if !shown {
                    return None;
                }
                self.sync_rows(window, cx);
                self.ensure_rules(bot, window, cx);
                Some(cursor)
            }
            Ok(Err(_)) => {
                failed("The server sent an invalid snapshot.".into(), None, cx);
                None
            }
            Err(ClientError::Api { status: 410, .. }) => {
                view.update(cx, |view, _| view.load = Load::Failed { _retry: None });
                self.refresh_now(window, cx);
                None
            }
            Err(err) => {
                let retry = shown.then(|| {
                    cx.spawn_in(window, async move |this, cx| {
                        cx.background_executor().timer(Duration::from_secs(2)).await;
                        let _ = this.update_in(cx, |app, window, cx| {
                            if app.shows(bot, cx) {
                                app.load_conversation(window, cx);
                            }
                        });
                    })
                });
                failed(format!("Could not load history: {err}"), retry, cx);
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
        let shown = self.shows(bot, cx);
        let Some(view) = self.view_of(bot, cx) else {
            return;
        };
        let failed = match message {
            StreamMessage::Frame(frame) => {
                let Some(applied) = view.update(cx, |view, _| {
                    let conversation = view.conversation.as_mut()?;
                    Some(
                        conversation
                            .apply(&frame)
                            .map(|applied| (applied, conversation.activity.clone())),
                    )
                }) else {
                    return;
                };
                match applied {
                    Ok((Applied::Duplicate, _)) => false,
                    // The bot list stays authoritative for a warm bot's
                    // activity: its last frames can trail the list.
                    Ok((Applied::Changed, _)) if !shown => {
                        self.prune_pending(bot, cx);
                        false
                    }
                    Ok((Applied::Changed, activity)) => {
                        if activity.as_ref().is_some_and(|a| a.phase == Phase::Idle) {
                            self.stopping.remove(&bot);
                        }
                        if let Some(entry) = Arc::make_mut(&mut self.bots)
                            .iter_mut()
                            .find(|b| b.id == bot)
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
        } else {
            view.update(cx, |view, _| {
                if matches!(view.load, Load::Settled) {
                    view.load = Load::Failed { _retry: None };
                }
            });
        }
    }

    /// Drops `bot`'s pending sends its conversation now holds.
    pub(super) fn prune_pending(&mut self, bot: Uuid, cx: &App) {
        let Some(view) = self.view_of(bot, cx) else {
            return;
        };
        if let (Some(conversation), Some(pending)) = (
            view.read(cx).conversation.as_ref(),
            self.pending.get_mut(&bot),
        ) {
            pending.retain(|p| !conversation.has_request(p.request_id));
        }
    }

    pub(super) fn transcript_inputs(&self, bot: Uuid) -> TranscriptInputs {
        TranscriptInputs {
            connected: self.connection == super::Connection::Connected,
            hidden: self.hidden,
            bots: self.bots.clone(),
            mention_bots: self.mention_bots.clone(),
            pending: self.pending.get(&bot).cloned().unwrap_or_default(),
            quotes: self.quotes_of(bot).to_vec(),
            restoring_rules: self.restoring_rules(bot),
        }
    }

    fn quotes_of(&self, bot: Uuid) -> &[Quote] {
        self.drafts.get(&bot).map_or(&[], |draft| &draft.quotes)
    }

    /// Pushes what the shown view reads, if it changed. It runs after each
    /// notify of the window, so a change to what the view reads must notify
    /// the window.
    pub(super) fn sync_view_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.clone() else {
            return;
        };
        let inputs = self.transcript_inputs(view.read(cx).bot_id);
        if view.read(cx).inputs != inputs {
            view.update(cx, |view, cx| view.set_inputs(inputs, window, cx));
        }
    }

    pub(super) fn sync_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.view.clone() else {
            return;
        };
        self.prune_pending(view.read(cx).bot_id, cx);
        self.sync_view_inputs(window, cx);
        view.update(cx, |view, cx| view.sync_rows(window, cx));
        cx.notify();
    }
}

/// The row that shows `key`, or for an item folded into a task, the task's
/// row.
pub(super) fn row_index(
    rows: &[TranscriptRow],
    conversation: Option<&Conversation>,
    key: RowKey,
) -> Option<usize> {
    let task = match key {
        RowKey::Item(id) => conversation
            .and_then(|c| c.item(id))
            .map(|i| RowKey::Task(i.turn_id)),
        _ => None,
    };
    rows.iter()
        .position(|row| row.contains(key))
        .or_else(|| task.and_then(|task| rows.iter().position(|row| row.key == task)))
}

pub(super) fn on_app<E: ?Sized>(
    app: &WeakEntity<ElseApp>,
    f: impl Fn(&mut ElseApp, &E, &mut Window, &mut Context<ElseApp>) + 'static,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    let app = app.clone();
    move |event, window, cx| {
        let _ = app.update(cx, |app, cx| f(app, event, window, cx));
    }
}

impl ConversationView {
    pub(super) fn remeasure_row(&self, key: RowKey, cx: &mut Context<Self>) {
        if let Some(ix) = self.row_index(key) {
            self.list.remeasure_items(ix..ix + 1);
        }
        cx.notify();
    }

    pub(super) fn set_inputs(
        &mut self,
        inputs: TranscriptInputs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let reach = |inputs: &TranscriptInputs| (inputs.connected, inputs.hidden);
        let reached = reach(&self.inputs) != reach(&inputs);
        let quoted = self.inputs.quotes != inputs.quotes;
        self.inputs = inputs;
        if reached {
            if !self.inputs.connected {
                self.clear_secret_forms(window, cx);
            }
            self.sync_skill_cards(window, cx);
            self.sync_secret_cards(window, cx);
            self.sync_social_cards(window, cx);
        }
        if quoted {
            self.paint_quotes(cx);
        }
        cx.notify();
    }

    pub(super) fn on_app<E: ?Sized>(
        &self,
        f: impl Fn(&mut ElseApp, &E, &mut Window, &mut Context<ElseApp>) + 'static,
    ) -> impl Fn(&E, &mut Window, &mut App) + 'static {
        on_app(&self.app, f)
    }

    /// Redraws card `key` after its request settled, and returns whether
    /// the caller goes on to handle the result. The view is drawn cached, so
    /// a result that arrives once the connection already dropped is drawn
    /// only through this notify.
    pub(super) fn settle_card(
        &self,
        key: CardKey,
        outcome: Settle,
        cx: &mut Context<Self>,
    ) -> bool {
        self.remeasure_row(RowKey::Card(key), cx);
        outcome.settled()
    }

    /// Shows `notice` under the composer once this update ends.
    pub(super) fn notice(&self, notice: String, cx: &mut Context<Self>) {
        let app = self.app.clone();
        cx.defer(move |cx| {
            let _ = app.update(cx, |app, cx| {
                app.notice = Some(notice);
                cx.notify();
            });
        });
    }

    /// Asks the window to check what the owner has read, once this view is
    /// no longer being updated.
    pub(super) fn check_reading(&self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.shown() {
            return;
        }
        let app = self.app.clone();
        window.defer(cx, move |window, cx| {
            let _ = app.update(cx, |app, cx| app.check_reading(window, cx));
        });
    }

    /// Scrolls to the message a reply quotes, loading older pages until it
    /// is found or the history runs out.
    pub(super) fn reveal_quoted(
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
                    .is_some_and(|item| super::reply::quotes_whole(&item.text, original));
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

    /// Shows item `target` whole, opening a folded bot exchange or a task's
    /// earlier replies, and returns the index of the row that holds it.
    pub(super) fn unfold_item(
        &mut self,
        target: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.open_quiet_run_of(target, window, cx);
        let conversation = self.conversation.as_ref()?;
        // An exchange reply may be folded or cut to an excerpt.
        let folded = !self.full_replies.contains(&target)
            && self.rows.iter().any(|row| {
                row.activity
                    .as_ref()
                    .is_some_and(|activity| activity.answers.contains(&target))
            });
        if folded {
            if let Some(receipt) = conversation
                .item(target)
                .and_then(|item| conversation.task_receipt(item.turn_id))
            {
                self.exchange_folds
                    .insert(receipt.id, super::bot_activity::Fold::Open);
            }
            self.full_replies.insert(target);
            self.sync_rows(window, cx);
        }
        let conversation = self.conversation.as_ref()?;
        let ix = self.row_index(RowKey::Item(target))?;
        if let RowKey::Item(reply) = self.rows[ix].key
            && self.rows[ix].folded.contains(&target)
        {
            self.expanded_folds.insert(reply);
            self.list.remeasure_items(ix..ix + 1);
        }
        if let Some(task) = &self.rows[ix].task
            && (task.history.contains(&target)
                || (task.kept.contains(&target) && task.reply_superseded(conversation, target))
                || (task.latest == Some(target)
                    && !task.latest_kept
                    && matches!(task.state, ReplyState::Queued | ReplyState::Working)))
        {
            self.expanded_tasks.insert(task.turn);
            self.list.remeasure_items(ix..ix + 1);
        }
        Some(ix)
    }

    /// Opens the no-news run holding item `target`, and the group of runs
    /// it folds into, so a find match inside it shows.
    fn open_quiet_run_of(&mut self, target: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(turn) = self
            .conversation
            .as_ref()
            .and_then(|c| c.item(target))
            .map(|item| item.turn_id)
            .filter(|&turn| self.conversation.as_ref().is_some_and(|c| c.no_news(turn)))
        else {
            return;
        };
        if self.open_quiet.insert(RowKey::QuietRun(turn)) {
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
        let runs = self
            .rows
            .iter()
            .find(|row| row.key == key)
            .map(|row| row.quiet_runs.clone())
            .unwrap_or_default();
        if !runs.is_empty() && group_open(&runs, &self.open_quiet) {
            for turn in runs {
                self.open_quiet.remove(&RowKey::QuietRuns(turn));
                self.open_quiet.remove(&RowKey::QuietRun(turn));
            }
        } else if !self.open_quiet.remove(&key) {
            self.open_quiet.insert(key);
        }
        self.sync_rows(window, cx);
        // The rows below a folded or unfolded run keep their keys, but their
        // spacing depends on the row above.
        self.list.remeasure();
    }

    /// Flashes `reveal` until it fades out, painting its passage on each
    /// frame and asking the list to center the passage once it is found.
    pub(super) fn start_reveal(&mut self, reveal: Reveal, cx: &mut Context<Self>) {
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

    pub(super) fn load_older(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(before) = self.conversation.as_ref().and_then(|c| c.older_before_seq) else {
            return;
        };
        if self.older.is_some() {
            return;
        }
        let (client, app, bot) = (self.client.clone(), self.app.clone(), self.bot_id);
        self.older = Some(cx.spawn_in(window, async move |this, cx| {
            let page = client.snapshot(bot, Some(before)).await;
            let failed = this.update_in(cx, |view, window, cx| {
                view.older = None;
                match page {
                    Ok(page) => {
                        if let Some(conversation) = view.conversation.as_mut() {
                            conversation.merge_older(page);
                        }
                        let pending = view.pending_reveal.take();
                        view.sync_rows(window, cx);
                        if let Some((from, author, original)) = pending {
                            view.reveal_quoted(from, author, &original, window, cx);
                        }
                        None
                    }
                    Err(err) => {
                        cx.notify();
                        Some(err)
                    }
                }
            });
            if let Ok(Some(err)) = failed {
                let _ = app.update(cx, |app, cx| {
                    app.notice = Some(format!("Could not load older messages: {err}"));
                    cx.notify();
                });
            }
        }));
        cx.notify();
    }

    pub(super) fn sync_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.shown() {
            return;
        }
        let bot = self.bot_id;
        if self.inputs.bots.iter().any(|b| b.id == bot && b.silenced()) {
            self.unread = None;
        }
        let conversation = self.conversation.as_ref();
        let pending: Vec<PendingSend> = self
            .inputs
            .pending
            .iter()
            .filter(|p| conversation.is_none_or(|c| !c.has_request(p.request_id)))
            .cloned()
            .collect();
        if pending.len() < self.inputs.pending.len() {
            // A loaded page confirmed a send; the window drops it and
            // rebuilds these rows.
            let app = self.app.clone();
            window.defer(cx, move |window, cx| {
                let _ = app.update(cx, |app, cx| app.sync_rows(window, cx));
            });
        }
        let Transcript {
            rows,
            texts,
            tasks,
            ticking,
        } = transcript::build(
            conversation,
            &pending,
            &Folds {
                quiet: &self.open_quiet,
                full_replies: &self.full_replies,
            },
            self.unread.as_ref().map(|unread| unread.since),
            &Local,
        );
        let old = std::mem::take(&mut self.rows);
        self.reply_transitions
            .retain(|_, at| at.elapsed() < Duration::from_millis(320));
        for task in tasks.values() {
            if task.state.terminal()
                && old.iter().any(|row| {
                    row.task
                        .as_ref()
                        .is_some_and(|prior| prior.turn == task.turn && !prior.state.terminal())
                })
            {
                self.reply_transitions
                    .insert(task.turn, std::time::Instant::now());
                if task.state == ReplyState::Complete {
                    self.expanded_tasks.remove(&task.turn);
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
            self.list
                .splice(prefix..old.len() - suffix, rows.len() - prefix - suffix);
        }
        let previous: HashMap<_, _> = old.iter().map(|row| (row.key, row)).collect();
        for (ix, row) in rows.iter().enumerate() {
            if previous.get(&row.key).is_some_and(|old| *old != row) {
                self.list.remeasure_items(ix..ix + 1);
            }
        }
        let mut changed = Vec::new();
        let mut created = false;
        let text_keys: HashSet<_> = texts.iter().map(|(key, ..)| *key).collect();
        let mention_bots = self.inputs.mention_bots.clone();
        let host = RowHost {
            client: self.client.clone(),
            bot,
            app: self.app.clone(),
        };
        for (key, source) in texts {
            match self.texts.get_mut(&key) {
                Some(row) if row.format() == source.format => {
                    if row.sync(source, &mention_bots, window, cx) {
                        changed.push(key);
                    }
                }
                _ => {
                    let text = RowText::new(key, source, &host, &mention_bots, window, cx);
                    self.texts.insert(key, text);
                    created = true;
                }
            }
        }
        self.texts.retain(|key, _| text_keys.contains(key));
        if created {
            // New text views learn their wrapped height during their first
            // layout; measure the rows again once that has happened.
            let list = self.list.clone();
            window.on_next_frame(move |window, _| {
                let list = list.clone();
                window.on_next_frame(move |window, _| {
                    list.remeasure();
                    window.refresh();
                });
            });
        }
        let conversation = self.conversation.as_ref();
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
                self.list.remeasure_items(ix..ix + 1);
            }
        }
        self.rows = rows;
        if created {
            self.paint_quotes(cx);
        }
        if !ticking {
            self._reply_clock = None;
        } else if self._reply_clock.is_none() {
            self._reply_clock = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let keep = this.update(cx, |view, cx| {
                        let ticking = transcript::ticking(
                            view.rows.iter().filter_map(|row| row.task.as_ref()),
                            view.conversation.as_ref(),
                        );
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
