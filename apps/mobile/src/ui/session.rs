//! The connected server, as signals on the engine thread: the bot list,
//! the live connection's state, and each open chat. One live connection
//! feeds them all; it ends with the connected screen.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use else_wire::bots::Bot;
use nsh_core::platform::Lifecycle;
use nsh_core::prelude::*;
use uuid::Uuid;

use else_client::api::{LiveCommand, LiveCommands};

use else_client::request::{Request, Settle};

use crate::api::{Client, ClientError, LiveEvent, StreamMessage};
use crate::server::Profile;
use else_client::model::Conversation;

/// Whether the phone has a server, and its client when it has.
#[derive(Clone, Default)]
pub enum Link {
    #[default]
    None,
    Connected(Rc<Server>),
}

impl PartialEq for Link {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::None, Self::None) => true,
            (Self::Connected(a), Self::Connected(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

pub struct Server {
    pub profile: Profile,
    pub client: Arc<Client>,
    /// The chats on screen, so live events reach them. They end with the
    /// server: a switch or a disconnect leaves nothing to reset.
    chats: RefCell<HashMap<Uuid, Signal<Chat>>>,
    /// The live connection's subscriptions, once it has started.
    live: RefCell<Option<LiveCommands>>,
}

impl Server {
    fn chat(&self, bot: Uuid) -> Option<Signal<Chat>> {
        self.chats.borrow().get(&bot).copied()
    }

    /// Follows `bot`'s channel from `cursor`, or stops following it.
    fn follow(&self, bot: Uuid, cursor: Option<String>) {
        if let Some(commands) = &*self.live.borrow() {
            let _ = commands.send(match cursor {
                Some(cursor) => LiveCommand::Subscribe { bot, cursor },
                None => LiveCommand::Unsubscribe { bot },
            });
        }
    }
}

pub fn link() -> Signal<Link> {
    global::<Link>()
}

/// The connected server; only screens shown while connected call it.
pub fn server() -> Rc<Server> {
    match link().get_untracked() {
        Link::Connected(server) => server,
        Link::None => panic!("no server is connected"),
    }
}

/// Saves a server checked by the connect form and switches to it.
pub fn connect(profile: Profile) -> Result<(), String> {
    let client = crate::api::connect(&profile)?;
    profile.save()?;
    use_server(profile, client);
    Ok(())
}

/// Switches to another saved server. The connected screen is rebuilt for
/// it: the old live connection and the old bot list end.
pub fn switch(origin: &str) -> Result<(), String> {
    let profile = Profile::saved(origin)
        .ok_or_else(|| "This server's owner key is no longer on this iPhone.".to_owned())?;
    let client = crate::api::connect(&profile)?;
    profile.activate()?;
    use_server(profile, client);
    Ok(())
}

/// Opens the saved server at launch, without saving it again.
pub fn resume(profile: Profile) -> Result<(), String> {
    let client = crate::api::connect(&profile)?;
    use_server(profile, client);
    Ok(())
}

fn use_server(profile: Profile, client: Arc<Client>) {
    batch(|| {
        bots().set(Bots::default());
        live_state().set(LiveState::Connecting);
        link().set(Link::Connected(Rc::new(Server {
            profile,
            client,
            chats: RefCell::default(),
            live: RefCell::default(),
        })));
    });
}

/// Removes the server in use and its key from the phone, then opens the
/// next saved server, or the connect screen when none is left.
pub fn disconnect() {
    let origin = server().profile.origin.as_str().to_owned();
    let _ = std::fs::remove_dir_all(super::files::server_dir(&origin));
    match Profile::forget(&origin).map(resume) {
        Some(Ok(())) => {}
        Some(Err(err)) => {
            log::warn!("{err}");
            clear();
        }
        None => clear(),
    }
}

fn clear() {
    batch(|| {
        bots().set(Bots::default());
        live_state().set(LiveState::Connecting);
        link().set(Link::None);
    });
}

/// The connected screen's owner, which outlives the cards and buttons a
/// request starts from.
#[derive(Clone, Copy, PartialEq)]
struct Held(Owner);

impl Default for Held {
    fn default() -> Self {
        Self(Owner::detached())
    }
}

fn held() -> Signal<Held> {
    global::<Held>()
}

/// Runs a request the server must see through under the connected server,
/// not under the view that started it: a chat card that scrolls away or a
/// button that is rebuilt would otherwise cancel it midway. It ends with
/// the server, on a switch or a disconnect. Write the starter's signals
/// with `try_set`, since the view may be gone by then.
pub fn spawn_held<T: 'static>(future: impl std::future::Future<Output = T> + 'static) {
    held().get_untracked().0.run(|| {
        spawn(future);
    });
}

/// Sends `op` for `request`, held by the connected server (see
/// [`spawn_held`]). An op equal to the one whose outcome is unknown is a
/// resend of it, under the same request ID it carries; any other starts a
/// new request. `send` makes the call, and `done` gets the server's answer
/// once `request` accepted it. Nothing is sent while `request` is busy.
pub fn submit<Op, T, F>(
    request: Signal<Request<Op>>,
    op: Op,
    send: impl FnOnce(Op) -> F + 'static,
    done: impl FnOnce(T) + 'static,
) where
    Op: Clone + PartialEq + 'static,
    T: 'static,
    F: std::future::Future<Output = Result<T, ClientError>> + 'static,
{
    // The view that holds `request` may be gone, such as a card a live
    // update replaced while its confirmation was open: then nothing is sent.
    let mut started = None;
    try_update(request, |r| {
        started = if r.unconfirmed_op() == Some(&op) {
            r.retry()
        } else {
            r.begin(op.clone()).map(|attempt| (attempt, op))
        };
    });
    let Some((attempt, op)) = started else {
        return;
    };
    spawn_held(async move {
        let result = send(op).await;
        let mut settle = Settle::Stale;
        try_update(request, |r| {
            settle = r.settle(attempt, &result);
            // A rejected key leaves the request idle; say why, unless an
            // earlier unconfirmed op is kept for its resend.
            if settle == Settle::Rejected && *r == Request::Idle {
                r.reject(
                    "The server rejected the owner key. Add the server again with its current key.",
                );
            }
        });
        if let (Settle::Settled, Ok(value)) = (settle, result) {
            done(value);
        }
    });
}

/// Changes a signal in place unless its owner is gone; for a request that
/// may outlive the view whose signal it writes.
pub fn try_update<T: Clone + 'static>(signal: Signal<T>, f: impl FnOnce(&mut T)) -> bool {
    match signal.try_get() {
        Some(mut value) => {
            f(&mut value);
            signal.try_set(value)
        }
        None => false,
    }
}

/// The bot list, as the server last sent it.
#[derive(Clone, Default, PartialEq)]
pub struct Bots {
    pub loaded: bool,
    pub list: Vec<Bot>,
    pub default_bot_id: Option<Uuid>,
}

pub fn bots() -> Signal<Bots> {
    global::<Bots>()
}

/// The live connection, as the owner sees it.
#[derive(Clone, Default, PartialEq)]
pub enum LiveState {
    #[default]
    Connecting,
    Live,
    /// Retrying; what is loaded stays shown.
    Offline(String),
    Unauthorized,
}

pub fn live_state() -> Signal<LiveState> {
    global::<LiveState>()
}

/// One open chat.
#[derive(Clone, PartialEq)]
pub enum Chat {
    Loading,
    Failed(String),
    Ready(Box<Conversation>),
    /// The stream moved past what is loaded; the stale page stays shown,
    /// live events wait, and a fresh page is being fetched.
    Recovering(Box<Conversation>),
}

impl Chat {
    /// The conversation on screen, current or being recovered.
    pub fn conversation(&self) -> Option<&Conversation> {
        match self {
            Self::Ready(conversation) | Self::Recovering(conversation) => Some(conversation),
            Self::Loading | Self::Failed(_) => None,
        }
    }

    pub fn conversation_mut(&mut self) -> Option<&mut Conversation> {
        match self {
            Self::Ready(conversation) | Self::Recovering(conversation) => Some(conversation),
            Self::Loading | Self::Failed(_) => None,
        }
    }

    /// Starts recovering; `false` when a load is already under way.
    fn start_recovery(&mut self) -> bool {
        match std::mem::replace(self, Chat::Loading) {
            Chat::Ready(conversation) => {
                *self = Chat::Recovering(conversation);
                true
            }
            Chat::Failed(_) => true,
            other @ (Chat::Loading | Chat::Recovering(_)) => {
                *self = other;
                false
            }
        }
    }
}

/// Starts the live connection and the first bot list; both end with the
/// caller's owner (the connected screen).
pub fn start() {
    held().set(Held(Owner::current()));
    cleanup(|| held().set(Held::default()));
    let server = server();
    let live = server.client.clone().live();
    server.live.replace(Some(live.commands.clone()));
    // Back from the background, iOS has usually closed the socket: retry
    // now instead of after the backoff, and catch up on the list.
    let commands = live.commands.clone();
    let was = stored(lifecycle());
    effect(move || {
        let now = lifecycle();
        if was.replace(now) == Lifecycle::Background && now == Lifecycle::Active {
            commands.reconnect();
            refresh_bots();
        }
    });
    refresh_bots();
    let mut events = live.events;
    spawn(async move {
        while let Some(event) = events.recv().await {
            handle(&server, event);
        }
    });
}

fn handle(server: &Rc<Server>, event: LiveEvent) {
    match event {
        LiveEvent::Connected => live_state().set(LiveState::Live),
        LiveEvent::Disconnected(reason) => live_state().set(LiveState::Offline(reason)),
        LiveEvent::Unauthorized => live_state().set(LiveState::Unauthorized),
        LiveEvent::Bots(list) => bots().set(Bots {
            loaded: true,
            list: list.bots,
            default_bot_id: list.default_bot_id,
        }),
        // The phone shows no maintenance banner yet.
        LiveEvent::ServerPause(_) => {}
        LiveEvent::Bot { bot, message } => {
            let Some(chat) = server.chat(bot) else {
                return;
            };
            match message {
                StreamMessage::Frame(frame) => {
                    let mut reset = false;
                    chat.update(|chat| {
                        if let Chat::Ready(conversation) = chat {
                            reset = conversation.apply(&frame).is_err();
                        }
                    });
                    if reset {
                        recover(server, bot, chat);
                    }
                }
                StreamMessage::Reset => recover(server, bot, chat),
                StreamMessage::Deleted => {
                    server.follow(bot, None);
                    chat.set(Chat::Failed("This bot was deleted.".into()));
                }
            }
        }
    }
}

/// Replaces one bot of the list with the server's newer copy.
pub fn update_bot(bot: Bot) {
    bots().update(|bots| {
        if let Some(slot) = bots.list.iter_mut().find(|b| b.id == bot.id) {
            *slot = bot;
        }
    });
}

/// Whether question `id` of `bot` still waits on the owner, as the open
/// chat last saw it. `None` while the chat is closed or loading.
pub fn question_pending(bot: Uuid, id: Uuid) -> Option<bool> {
    let chat = match link().get_untracked() {
        Link::Connected(server) => server.chat(bot)?,
        Link::None => return None,
    };
    chat.try_get()?
        .conversation()
        .map(|c| c.question(id).is_some_and(|q| q.is_pending()))
}

/// Loads the bot list again.
pub fn refresh_bots() -> Spawned<()> {
    let client = server().client.clone();
    spawn(async move {
        match client.bots().await {
            Ok(list) => bots().set(Bots {
                loaded: true,
                list: list.bots,
                default_bot_id: list.default_bot_id,
            }),
            Err(ClientError::Unauthorized) => live_state().set(LiveState::Unauthorized),
            Err(err) => live_state().set(LiveState::Offline(err.to_string())),
        }
    })
}

/// Opens `bot`'s chat: loads its newest page and follows it live until
/// the caller's owner (the chat page) goes.
pub fn open_chat(bot: Uuid) -> Signal<Chat> {
    let chat = signal(Chat::Loading);
    let server = server();
    server.chats.borrow_mut().insert(bot, chat);
    {
        let server = server.clone();
        cleanup(move || {
            server.chats.borrow_mut().remove(&bot);
            server.follow(bot, None);
        });
    }
    load(server, bot, chat);
    chat
}

/// Fetches a fresh page for a chat the stream moved past.
fn recover(server: &Rc<Server>, bot: Uuid, chat: Signal<Chat>) {
    let mut start = false;
    chat.update(|c| start = c.start_recovery());
    if start {
        load(server.clone(), bot, chat);
    }
}

/// Loads the newest page of `bot` into `chat` and follows it from there.
/// While the chat is open a failed load retries with backoff; a first load
/// shows its error meanwhile, a recovery keeps the stale page shown.
fn load(server: Rc<Server>, bot: Uuid, chat: Signal<Chat>) {
    spawn(async move {
        let mut delay = Duration::from_secs(1);
        let open = || server.chat(bot) == Some(chat);
        while open() {
            let result = server
                .client
                .clone()
                .latest(bot)
                .await
                .map(|latest| latest.snapshot);
            // The chat may have closed during the request; its signal is
            // then gone.
            if !open() {
                return;
            }
            match result {
                Ok(snapshot) => {
                    match Conversation::from_snapshot(snapshot) {
                        Ok(conversation) => {
                            server.follow(bot, Some(conversation.cursor()));
                            chat.set(Chat::Ready(Box::new(conversation)));
                        }
                        Err(_) => chat.set(Chat::Failed(
                            "The server sent a page this app cannot read.".into(),
                        )),
                    }
                    return;
                }
                Err(err) => {
                    if chat.with_untracked(|c| matches!(c, Chat::Loading | Chat::Failed(_))) {
                        chat.set(Chat::Failed(err.to_string()));
                    }
                }
            }
            timer(delay).await;
            delay = (delay * 2).min(Duration::from_secs(15));
        }
    });
}

/// Loads the page before the oldest loaded message.
pub fn load_older(bot: Uuid, chat: Signal<Chat>) {
    let Some(before) = chat.with_untracked(|c| match c {
        Chat::Ready(conversation) => conversation.older_before_seq,
        _ => None,
    }) else {
        return;
    };
    let client = server().client.clone();
    spawn(async move {
        if let Ok(page) = client.snapshot(bot, Some(before)).await {
            // The chat may have closed during the request.
            try_update(chat, |c| {
                if let Chat::Ready(conversation) = c {
                    conversation.merge_older(page);
                }
            });
        }
    });
}
