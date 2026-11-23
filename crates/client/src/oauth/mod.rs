//! Browser sign-ins the owner completes for the server: a loopback listener
//! catches the provider's redirect, and the API redeems it. Shared by the
//! desktop and the phone.

pub mod callback;
pub mod email;
pub mod mcp;
