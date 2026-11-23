//! The app's screens: connect to a server, then a navigation stack of the
//! bot list and the chats pushed over it.

mod asks;
mod bot_list;
mod chat;
mod composer;
mod connect;
mod details;
mod edit_bot;
mod files;
mod list;
mod model;
mod new_bot;
mod parts;
pub mod session;
mod waits;

use std::rc::Rc;

use nsh_base::nav_stack;
use nsh_core::prelude::*;

use crate::server::Profile;
use session::Link;

/// A screen over the bot list: its key (unique in the stack) and what
/// makes it.
#[derive(Clone)]
pub struct Route {
    pub key: SharedString,
    pub screen: Rc<dyn Fn() -> Screen>,
}

impl PartialEq for Route {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

#[derive(Default, Clone, PartialEq)]
pub struct Routes(pub Vec<Route>);

pub fn routes() -> Signal<Routes> {
    global::<Routes>()
}

pub fn push(key: impl Into<SharedString>, screen: impl Fn() -> Screen + 'static) {
    let route = Route {
        key: key.into(),
        screen: Rc::new(screen),
    };
    routes().update(|routes| {
        if !routes.0.iter().any(|r| r.key == route.key) {
            routes.0.push(route);
        }
    });
}

/// The application's root component.
#[component]
pub fn App() -> impl IntoElement {
    if let Some(profile) = Profile::load()
        && let Err(err) = session::resume(profile)
    {
        log::warn!("{err}");
    }
    // Keyed by the server itself, so switching servers builds a new stack.
    let link = computed(|| session::link().get());
    view()
        .flex_1()
        .bg(parts::colors::background())
        .child(dynamic(move || match link.get() {
            Link::Connected(_) => untracked(connected_stack).into_element(),
            Link::None => untracked(connect::screen).into_element(),
        }))
}

fn connected_stack() -> impl IntoElement {
    routes().set(Routes::default());
    session::start();
    let pushed = each(
        move || routes().with(|r| r.0.clone()),
        |route: &Route| route.key.clone(),
        |route, depth| {
            let route = route.get_untracked();
            (route.screen)().on_dismiss(move || {
                routes().update(|r| r.0.truncate(depth.get_untracked()));
            })
        },
    );
    nav_stack().flex_1().child(bot_list::screen()).child(pushed)
}
