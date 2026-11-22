//! The connected server, as signals on the engine thread: the bot list,
//! the live connection's state, and each open chat. One live connection
//! feeds them all; it ends with the connected screen.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use else_wire::bots::Bot;
use nsh_core::platform::Lifecycle;
use nsh_core::prelude::*;
use uuid::Uuid;

use crate::api::{Client, ClientError, LiveEvent};
use crate::conversation::Conversation;
use crate::server::Profile;

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

pub fn connect(profile: Profile) -> Result<(), String> {
    let client = Client::new(&profile)?;
    profile.save()?;
    link().set(Link::Connected(Rc::new(Server { profile, client })));
    Ok(())
}

pub fn disconnect() {
    Profile::forget();
    batch(|| {
        bots().set(Bots::default());
        live_state().set(LiveState::Connecting);
        link().set(Link::None);
    });
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

/// The chats on screen, so live events reach them.
#[derive(Clone, Default)]
struct Chats(HashMap<Uuid, Signal<Chat>>);

impl PartialEq for Chats {
    fn eq(&self, _: &Self) -> bool {
        false
    }
}

fn chats() -> Signal<Chats> {
    global::<Chats>()
}

/// The channels the live connection follows, each from its cursor.
#[derive(Clone, Default)]
struct Wanted(Option<tokio::sync::watch::Sender<HashMap<Uuid, String>>>);

impl PartialEq for Wanted {
    fn eq(&self, _: &Self) -> bool {
        false
    }
}

fn wanted() -> Signal<Wanted> {
    global::<Wanted>()
}

fn follow(bot: Uuid, cursor: Option<String>) {
    wanted().with_untracked(|wanted| {
        if let Some(sender) = &wanted.0 {
            sender.send_modify(|channels| match cursor {
                Some(cursor) => {
                    channels.insert(bot, cursor);
                }
                None => {
                    channels.remove(&bot);
                }
            });
        }
    });
}

/// Starts the live connection and the first bot list; both end with the
/// caller's owner (the connected screen).
pub fn start() {
    let client = server().client.clone();
    let live = client.clone().live();
    wanted().set(Wanted(Some(live.wanted.clone())));
    cleanup(move || wanted().set(Wanted(None)));
    // Back from the background, iOS has usually closed the socket: retry
    // now instead of after the backoff, and catch up on the list.
    let wake = live.wake.clone();
    let was = stored(lifecycle());
    effect(move || {
        let now = lifecycle();
        if was.replace(now) == Lifecycle::Background && now == Lifecycle::Active {
            wake.notify_one();
            refresh_bots();
        }
    });
    refresh_bots();
    let mut events = live.events;
    let keep = live.wanted;
    spawn(async move {
        let _keep = keep;
        while let Some(event) = events.recv().await {
            handle(event);
        }
    });
}

fn handle(event: LiveEvent) {
    match event {
        LiveEvent::Connected => live_state().set(LiveState::Live),
        LiveEvent::Disconnected(reason) => live_state().set(LiveState::Offline(reason)),
        LiveEvent::Unauthorized => live_state().set(LiveState::Unauthorized),
        LiveEvent::Bots(list) => bots().set(Bots {
            loaded: true,
            list: list.bots,
            default_bot_id: list.default_bot_id,
        }),
        LiveEvent::Bot { bot, id, message } => {
            let Some(chat) = chats().with_untracked(|c| c.0.get(&bot).copied()) else {
                return;
            };
            let mut reset = false;
            chat.update(|chat| {
                if let Chat::Ready(conversation) = chat {
                    reset = conversation.apply(&id, &message).is_err();
                }
            });
            if reset {
                recover(bot, chat);
            }
        }
        LiveEvent::BotReset { bot } => {
            if let Some(chat) = chats().with_untracked(|c| c.0.get(&bot).copied()) {
                recover(bot, chat);
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
    chats().update(|c| {
        c.0.insert(bot, chat);
    });
    cleanup(move || {
        chats().update(|c| {
            c.0.remove(&bot);
        });
        follow(bot, None);
    });
    load(bot, chat);
    chat
}

/// Fetches a fresh page for a chat the stream moved past.
fn recover(bot: Uuid, chat: Signal<Chat>) {
    let mut start = false;
    chat.update(|c| start = c.start_recovery());
    if start {
        load(bot, chat);
    }
}

/// Loads the newest page of `bot` into `chat` and follows it from there.
/// While the chat is open a failed load retries with backoff; a first load
/// shows its error meanwhile, a recovery keeps the stale page shown.
pub fn load(bot: Uuid, chat: Signal<Chat>) {
    let client = server().client.clone();
    spawn(async move {
        let mut delay = Duration::from_secs(1);
        let open = move || chats().with_untracked(|c| c.0.get(&bot) == Some(&chat));
        while open() {
            match client.clone().latest(bot).await {
                Ok(snapshot) => {
                    match Conversation::from_snapshot(snapshot) {
                        Ok(conversation) => {
                            follow(bot, Some(conversation.cursor()));
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
        if let Ok(page) = client.older(bot, before).await {
            chat.update(|c| {
                if let Chat::Ready(conversation) = c {
                    conversation.merge_older(page);
                }
            });
        }
    });
}
