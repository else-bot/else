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
    let store = Store::new(path.clone(), bot, Some(key.clone()));
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
        Store::new(path.clone(), another_bot, Some(key.clone()))
            .load()
            .await
            .is_err()
    );
    std::fs::copy(&stored, other.join(filename(bot))).unwrap();
    assert!(
        Store::new(other, bot, Some(key.clone()))
            .load()
            .await
            .is_err()
    );
    store.remove().await.unwrap();
    assert!(store.load().await.unwrap().is_none());
}
