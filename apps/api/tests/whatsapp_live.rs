//! Live WhatsApp check: pairs against WhatsApp's servers until the first QR
//! code arrives, then cancels. It never links a device, so it needs no
//! account.
//!
//! ELSE_WHATSAPP_LIVE=1 cargo test -p else-api --test whatsapp_live -- --ignored --nocapture

use else_api::socials::{
    pairing::{Pairing, Progress},
    providers::{Native, Transport, whatsapp::Vault},
    store,
    types::*,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[tokio::test]
#[ignore = "reaches WhatsApp's servers"]
async fn pairing_shows_a_code_from_whatsapp() {
    if std::env::var_os("ELSE_WHATSAPP_LIVE").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let url = else_api::config::DatabaseUrl::new(format!(
        "sqlite://{}",
        dir.path().join("live.sqlite3").display()
    ));
    let pool = else_api::db::connect(&url).await.unwrap();
    else_api::db::migrate(&pool).await.unwrap();
    let device = store::new_device(&pool).await.unwrap();
    let vault = Vault::new(pool.clone(), device, &DeviceKey::generate().unwrap());
    let (progress, mut seen) = Progress::new();
    let stop = CancellationToken::new();
    let watcher = async {
        let code = tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                seen.changed().await.expect("pairing ended before a code");
                if let Pairing::Code { code, expires_in } = seen.borrow_and_update().pairing.clone()
                {
                    return (code, expires_in);
                }
            }
        })
        .await;
        stop.cancel();
        code
    };
    let (result, code) = tokio::join!(Native.pair(&pool, vault, &progress, stop.clone()), watcher);
    let (code, expires_in) = code.expect("a QR code within a minute");
    println!(
        "code {}… expires in {expires_in}s",
        &code[..code.len().min(24)]
    );
    assert!(code.contains(','), "a pairing code lists its parts");
    assert!(result.is_err(), "a cancelled pairing links nothing");
}
