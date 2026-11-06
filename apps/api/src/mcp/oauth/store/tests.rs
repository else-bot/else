use super::super::fixtures::{credential, key};
use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn credentials_are_encrypted_private_and_bound_to_bot_and_package() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("first-package");
    let other = dir.path().join("other-package");
    std::fs::create_dir(&path).unwrap();
    std::fs::create_dir(&other).unwrap();
    let bot = Uuid::new_v4();
    let key = Arc::new(key());
    let store = Store::for_bot(path.clone(), bot, Some(key.clone()));
    store.save(&credential()).await.unwrap();
    let stored = path.join(filename(bot));
    let bytes = std::fs::read(&stored).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("private-access-token"));
    assert!(!String::from_utf8_lossy(&bytes).contains("private-refresh-token"));
    assert_eq!(
        std::fs::metadata(&stored).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let opened = store.load().await.unwrap().unwrap();
    assert_eq!(opened.access_token, "private-access-token");
    assert_eq!(
        opened.refresh_token.as_deref(),
        Some("private-refresh-token")
    );
    let another_bot = Uuid::new_v4();
    std::fs::copy(&stored, path.join(filename(another_bot))).unwrap();
    assert!(
        Store::for_bot(path.clone(), another_bot, Some(key.clone()))
            .load()
            .await
            .is_err()
    );
    std::fs::copy(&stored, other.join(filename(bot))).unwrap();
    assert!(
        Store::for_bot(other, bot, Some(key.clone()))
            .load()
            .await
            .is_err()
    );
    store.remove().await.unwrap();
    assert!(store.load().await.unwrap().is_none());
}

#[tokio::test]
async fn the_shared_store_adopts_the_latest_expiring_per_bot_sign_in_and_removes_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("github");
    std::fs::create_dir(&path).unwrap();
    let key = Arc::new(key());
    let (older, newer) = (Uuid::new_v4(), Uuid::new_v4());
    let fresh = credential();
    let mut stale = credential();
    stale.access_token = "older-bot-token".into();
    stale.expires_at = fresh.expires_at.map(|at| at - 3600);
    Store::for_bot(path.clone(), newer, Some(key.clone()))
        .save(&fresh)
        .await
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    Store::for_bot(path.clone(), older, Some(key.clone()))
        .save(&stale)
        .await
        .unwrap();

    let shared = Store::new(path.clone(), Some(key));
    let adopted = shared.load().await.unwrap().unwrap();
    assert_eq!(adopted.access_token, "private-access-token");
    for bot in [older, newer] {
        assert!(
            !path.join(filename(bot)).exists(),
            "per-bot files are removed"
        );
    }
    assert!(path.join(filename(Uuid::nil())).is_file());
    shared.remove().await.unwrap();
    assert!(
        shared.load().await.unwrap().is_none(),
        "disconnect stays disconnected"
    );
}
