//! else.bot for iOS: a client of a remote else.bot server, built with NSH.

pub mod api;
pub mod bots;
pub mod conversation;
mod mentions;
pub mod server;
mod ui;

pub use ui::App;

/// The app's UI set (`nsh.json` `"rust"`).
#[nsh_core::prelude::native_module("else/native")]
fn native(registry: &mut nsh_core::prelude::Registry) {
    registry.register::<App>();
    registry.assets(nsh_core::prelude::assets!());
}
