//! The server API: the client shared with the phone (`else_client::api`),
//! plus the desktop's own stream of a bot's computer screen.

pub use else_client::api::*;

mod desktop;
#[cfg(target_os = "macos")]
pub use desktop::MAX_TEXT_CHARS;
pub use desktop::{DesktopCommand, DesktopCommands, DesktopEvent, MAX_CLIPBOARD_BYTES, desktop};
