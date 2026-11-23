//! The client side of the else.bot API, shared by the desktop and the
//! phone: the server's address and owner token, the HTTP client with its
//! endpoints and live connection, and the conversation those events build.
//! It holds no UI; each app draws from it.

pub mod api;
pub mod connect;
pub mod layout;
pub mod model;
pub mod oauth;
pub mod origin;
pub mod reply;
pub mod request;
pub mod turn_status;
pub mod waits;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
