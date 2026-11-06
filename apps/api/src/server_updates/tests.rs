use super::*;
use std::os::unix::fs::PermissionsExt as _;

struct Fixture {
    directory: tempfile::TempDir,
    state: crate::http::AppState,
    stop: tokio_util::sync::CancellationToken,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Fixture {
    async fn new(installer: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::create_dir_all(home.join("src")).unwrap();
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::create_dir_all(home.join("server-current/bin")).unwrap();
        std::fs::write(
            home.join("bin/elsebot"),
            include_str!("../../../../scripts/elsebot"),
        )
        .unwrap();
        std::fs::set_permissions(
            home.join("bin/elsebot"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(home.join("src/install.sh"), installer).unwrap();
        let url = crate::config::DatabaseUrl::new(format!(
            "sqlite://{}",
            home.join("test.sqlite3").display()
        ));
        let pool = crate::db::connect(&url).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        crate::db::installation::setup(&pool).await.unwrap();
        let scheduler = crate::scheduler::Scheduler::new(
            pool.clone(),
            crate::providers::Registry::new(Vec::new()),
            1,
        );
        let stop = tokio_util::sync::CancellationToken::new();
        let state = crate::http::AppState::new(
            crate::credentials::OwnerToken::parse("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
                .unwrap(),
            pool.clone(),
            scheduler,
            crate::events::EventHub::start(pool, stop.clone()),
        );
        Self {
            directory,
            state,
            stop,
        }
    }

    fn updater(&self) -> Installation {
        Installation {
            home: Ok(self.directory.path().to_owned()),
        }
    }

    async fn wait(&self, id: Uuid) -> Operation {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut poll = tokio::time::interval(Duration::from_millis(10));
            loop {
                poll.tick().await;
                let operation = read_operation(self.directory.path(), id).unwrap();
                if operation.state != State::Running {
                    return operation;
                }
            }
        })
        .await
        .unwrap()
    }
}

fn request() -> Request {
    Request {
        request_id: Uuid::new_v4(),
        version: "999.0.0".into(),
    }
}

#[tokio::test]
async fn detached_installation_is_idempotent_and_status_survives_a_new_api_owner() {
    let fixture = Fixture::new(
        r#"
printf 'update invoked\n' >> "$ELSE_HOME/invocations"
printf 'installing %s\n' "$*"
while [ ! -f "$ELSE_HOME/finish" ]; do sleep .01; done
printf '#!/bin/sh\necho "else-api 999.0.0"\n' > "$ELSE_HOME/server-current/bin/else-api"
chmod +x "$ELSE_HOME/server-current/bin/else-api"
"#,
    )
    .await;
    let mut updater = fixture.updater();
    let request = request();
    let started = updater
        .start(request.clone(), &fixture.state)
        .await
        .unwrap();
    assert_eq!(started.operation.unwrap().state, State::Running);
    let repeated = updater
        .start(request.clone(), &fixture.state)
        .await
        .unwrap();
    assert_eq!(repeated.operation.unwrap().request_id, request.request_id);
    let other = updater
        .start(self::request(), &fixture.state)
        .await
        .unwrap_err();
    assert_eq!(other.code, "update_running");
    let conflict = updater
        .start(
            Request {
                version: "998.0.0".into(),
                ..request.clone()
            },
            &fixture.state,
        )
        .await
        .unwrap_err();
    assert_eq!(conflict.code, "request_conflict");
    std::fs::write(fixture.directory.path().join("finish"), "").unwrap();
    let operation = fixture.wait(request.request_id).await;
    assert_eq!(operation.state, State::Succeeded, "{}", operation.output);
    assert!(
        operation
            .output
            .contains("--yes --server-only --version 999.0.0")
    );
    let recovered = fixture.updater().snapshot().unwrap().operation.unwrap();
    assert_eq!(recovered.state, State::Succeeded);
    assert_eq!(recovered.request_id, request.request_id);
    assert_eq!(
        std::fs::read_to_string(fixture.directory.path().join("invocations"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let repeated = fixture
        .updater()
        .start(request, &fixture.state)
        .await
        .unwrap();
    assert_eq!(repeated.operation.unwrap().state, State::Succeeded);
}

#[tokio::test]
async fn installer_failure_and_interruption_remain_visible_after_restart() {
    let fixture = Fixture::new("printf 'fixture failure\\n'; exit 23\n").await;
    let request = request();
    fixture
        .updater()
        .start(request.clone(), &fixture.state)
        .await
        .unwrap();
    let failed = fixture.wait(request.request_id).await;
    assert_eq!(failed.state, State::Failed);
    assert!(failed.output.contains("fixture failure"));
    assert_eq!(
        fixture
            .updater()
            .snapshot()
            .unwrap()
            .operation
            .unwrap()
            .state,
        State::Failed
    );
    let job = fixture
        .directory
        .path()
        .join("run/server-updates")
        .join(request.request_id.to_string());
    std::fs::remove_file(job.join("result")).unwrap();
    std::fs::remove_file(job.join("pid")).unwrap();
    let record = Record {
        request_id: request.request_id,
        version: request.version,
        started: now() - START_GRACE - 1,
    };
    write_private(
        &job.join("request.json"),
        &serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    let interrupted = fixture.updater().snapshot().unwrap().operation.unwrap();
    assert_eq!(interrupted.state, State::Failed);
    assert!(interrupted.error.unwrap().contains("stopped"));
}

#[tokio::test]
async fn invalid_versions_and_active_work_never_launch_an_installer() {
    let fixture = Fixture::new("exit 0\n").await;
    for version in [
        "0.0.1",
        crate::VERSION,
        "1.2.3;touch nope",
        "1.2.3-rc1",
        "01.2.3",
        "1.2",
    ] {
        let refused = fixture
            .updater()
            .start(
                Request {
                    version: version.into(),
                    ..request()
                },
                &fixture.state,
            )
            .await
            .unwrap_err();
        assert_eq!(refused.code, "invalid_version", "{version}");
    }
    let bot = Uuid::new_v4();
    let conversation = Uuid::new_v4();
    sqlx::query("INSERT INTO bots(id,name) VALUES(?,'Working')")
        .bind(bot)
        .execute(&fixture.state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id) VALUES(?,?)")
        .bind(conversation)
        .bind(bot)
        .execute(&fixture.state.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO turns(id,bot_id,conversation_id,request_id,request_hash,turn_no,owner_item_id,provider_id,status) VALUES(?,?,?,?,zeroblob(32),1,?,'synthetic','running')")
        .bind(Uuid::new_v4()).bind(bot).bind(conversation).bind(Uuid::new_v4()).bind(Uuid::new_v4()).execute(&fixture.state.db).await.unwrap();
    let refused = fixture
        .updater()
        .start(request(), &fixture.state)
        .await
        .unwrap_err();
    assert_eq!(refused.code, "busy");
    assert!(!fixture.directory.path().join("run/server-updates").exists());
}

#[tokio::test]
async fn log_tails_are_bounded_and_private() {
    let fixture = Fixture::new("exit 42\n").await;
    let request = request();
    fixture
        .updater()
        .start(request.clone(), &fixture.state)
        .await
        .unwrap();
    fixture.wait(request.request_id).await;
    let job = fixture
        .directory
        .path()
        .join("run/server-updates")
        .join(request.request_id.to_string());
    std::fs::write(job.join("output.log"), vec![b'x'; OUTPUT_CAP as usize * 2]).unwrap();
    assert_eq!(
        fixture
            .updater()
            .snapshot()
            .unwrap()
            .operation
            .unwrap()
            .output
            .len(),
        OUTPUT_CAP as usize
    );
    for name in ["request.json", "worker.sh", "output.log", "result"] {
        assert_eq!(
            std::fs::metadata(job.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "{name}"
        );
    }
}
