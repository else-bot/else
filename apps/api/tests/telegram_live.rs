//! Live Telegram check against a disposable bot and public channel. It never
//! polls for updates, so it can run while another server owns the bot.
//!
//! ELSE_TELEGRAM_TOKEN_FILE=path ELSE_TELEGRAM_CHAT=t.me/channel \
//!   cargo test -p else-api --test telegram_live -- --ignored --nocapture

use else_api::socials::{
    providers::{Native, Session, Transport},
    types::*,
};
use uuid::Uuid;

#[tokio::test]
#[ignore = "needs a live Telegram bot token and public channel"]
async fn a_public_channel_link_resolves_and_receives_a_post() {
    let path = std::env::var("ELSE_TELEGRAM_TOKEN_FILE").expect("ELSE_TELEGRAM_TOKEN_FILE");
    let raw = std::fs::read_to_string(path).expect("token file");
    let chat = std::env::var("ELSE_TELEGRAM_CHAT").expect("ELSE_TELEGRAM_CHAT");
    let token = Token::parse(raw.trim().to_owned()).unwrap();
    let identity = Native.identity(Provider::Telegram, &token).await.unwrap();
    let c = Connection {
        id: Uuid::new_v4(),
        provider: Provider::Telegram,
        remote_id: identity.id,
        display_name: identity.display_name,
        username: identity.username,
        state: "active".into(),
        health: "connected".into(),
        generation: 1,
        instructions: String::new(),
        message_content: false,
    };
    let session = Session::new(Provider::Telegram, token);
    let username = public_username(&chat).unwrap().expect("a public chat");
    let found = Native.resolve(&c, &session, &username).await.unwrap();
    println!("{chat} -> {found:?}");
    assert!(found.id.starts_with('-'), "a channel ID is negative");
    assert!(found.can_send, "the bot may post in {chat}");
    let sent = Native
        .send(&session, &found.id, "Test", None)
        .await
        .unwrap();
    println!("sent message {}", sent["message_id"]);
}
