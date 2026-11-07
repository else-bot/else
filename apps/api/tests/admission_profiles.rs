mod common;

use std::{sync::Arc, time::Duration};

use common::{ProfiledFake, TestDb};
use else_api::{
    app::providers::Profile,
    db::bot_profiles,
    providers::{Registry, codex::Codex, synthetic::Synthetic},
};
use uuid::Uuid;

async fn bot(db: &TestDb) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO bots(id,name) VALUES(?1,'Admission test')")
        .bind(id)
        .execute(&db.pool)
        .await
        .unwrap();
    id
}

fn synthetic() -> Arc<Synthetic> {
    Arc::new(Synthetic::new(Duration::from_millis(1)))
}

async fn default_provider(db: &TestDb, provider: &str) {
    sqlx::query("INSERT INTO installation(singleton,default_provider_id) VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET default_provider_id=excluded.default_provider_id")
        .bind(provider)
        .execute(&db.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn admission_snapshots_bot_override_before_installation_default() {
    let db = TestDb::migrated().await;
    let bot = bot(&db).await;
    let registry = Registry::new(vec![synthetic(), Arc::new(ProfiledFake)]);
    default_provider(&db, "codex").await;
    sqlx::query("INSERT INTO provider_profiles(provider_id,model,reasoning_effort,service_tier,revision) VALUES('codex','gpt-6.1-sol','medium','default',7)")
        .execute(&db.pool)
        .await
        .unwrap();
    let selected =
        bot_profiles::for_admission(&mut db.pool.acquire().await.unwrap(), &registry, bot)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(selected.provider_id, "codex");
    let (revision, profile) = selected.profile.unwrap();
    assert_eq!(revision, 7);
    assert_eq!(profile.0.reasoning_effort, "medium");

    let own = Profile {
        model: "gpt-6.1-sol".into(),
        reasoning_effort: "high".into(),
        service_tier: "priority".into(),
    };
    let saved = bot_profiles::put(&db.pool, bot, "codex", &own)
        .await
        .unwrap();
    default_provider(&db, "synthetic").await;
    let selected =
        bot_profiles::for_admission(&mut db.pool.acquire().await.unwrap(), &registry, bot)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(selected.provider_id, "codex");
    let (revision, profile) = selected.profile.unwrap();
    assert_eq!(revision, saved.revision);
    assert_eq!(profile.0, own);
}

#[tokio::test]
async fn missing_or_unknown_choice_uses_registry_default_without_requiring_a_profile() {
    let db = TestDb::migrated().await;
    let bot = bot(&db).await;
    let registry = Registry::new(vec![synthetic()]);
    for choice in [None, Some("removed-provider")] {
        if let Some(choice) = choice {
            default_provider(&db, choice).await;
        }
        let selected =
            bot_profiles::for_admission(&mut db.pool.acquire().await.unwrap(), &registry, bot)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(selected.provider_id, "synthetic");
        assert!(selected.profile.is_none());
    }
    assert!(
        bot_profiles::for_admission(
            &mut db.pool.acquire().await.unwrap(),
            &Registry::new(vec![]),
            bot,
        )
        .await
        .unwrap()
        .is_none()
    );
}

#[tokio::test]
async fn known_choice_waits_when_its_profile_or_provider_is_unavailable() {
    let db = TestDb::migrated().await;
    let bot = bot(&db).await;
    default_provider(&db, "codex").await;
    let missing_profile = Registry::new(vec![synthetic(), Arc::new(ProfiledFake)]);
    let unavailable = Registry::new(vec![synthetic(), Arc::new(Codex::detect(None, None, None))]);
    for registry in [missing_profile, unavailable] {
        assert!(
            bot_profiles::for_admission(&mut db.pool.acquire().await.unwrap(), &registry, bot,)
                .await
                .unwrap()
                .is_none(),
            "a known choice must not silently fall back to the available synthetic provider"
        );
    }
}
