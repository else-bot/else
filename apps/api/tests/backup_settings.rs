mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use else_api::backup::capture::Coordinator;
use else_api::backup::service::Backups;
use else_api::config::{Config, Source};
use else_api::credentials::{OwnerToken, SecretKey};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

const SECRET: &str = "secret-that-must-never-be-returned";
const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

struct Env(BTreeMap<String, String>);
impl Source for Env {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

struct Harness {
    app: common::TestApp,
    config: Config,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl Harness {
    async fn new() -> Self {
        let mut app = common::TestApp::installed().await;
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&Env(BTreeMap::from([
            ("ELSE_DATA_DIR".into(), dir.path().display().to_string()),
            ("DATABASE_URL".into(), app.db.url.expose().to_owned()),
            ("ELSE_BACKUP_INTERVAL_SECS".into(), "0".into()),
        ])))
        .unwrap();
        let cancel = CancellationToken::new();
        let key = Arc::new(SecretKey::parse(KEY).unwrap());
        let (backups, task) = Backups::start(
            &config,
            app.db.pool.clone(),
            key.clone(),
            Coordinator::default(),
            cancel.clone(),
        )
        .await
        .unwrap();
        let mut state = else_api::http::AppState::new(
            OwnerToken::parse(common::TOKEN).unwrap(),
            app.db.pool.clone(),
            app.scheduler.clone(),
            app.state.events.clone(),
        )
        .with_secret_key(key);
        state.backups = Some(backups);
        app.state = Arc::new(state);
        app.router = else_api::http::router(&config, app.state.clone());
        Self {
            app,
            config,
            cancel,
            task,
            _dir: dir,
        }
    }
    async fn restart(&mut self) {
        self.cancel.cancel();
        (&mut self.task).await.unwrap();
        self.cancel = CancellationToken::new();
        let key = Arc::new(SecretKey::parse(KEY).unwrap());
        let (backups, task) = Backups::start(
            &self.config,
            self.app.db.pool.clone(),
            key.clone(),
            Coordinator::default(),
            self.cancel.clone(),
        )
        .await
        .unwrap();
        self.task = task;
        let mut state = else_api::http::AppState::new(
            OwnerToken::parse(common::TOKEN).unwrap(),
            self.app.db.pool.clone(),
            self.app.scheduler.clone(),
            self.app.state.events.clone(),
        )
        .with_secret_key(key);
        state.backups = Some(backups);
        self.app.state = Arc::new(state);
        self.app.router = else_api::http::router(&self.config, self.app.state.clone());
    }
    async fn snapshot(&self) -> Value {
        let response = self.app.owner("GET", "/v1/settings/backups", None).await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.body);
        response.body
    }
    async fn wait_idle(&self) -> Value {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let snapshot = self.snapshot().await;
                if snapshot["running"] == false {
                    return snapshot;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }
}

fn destination(endpoint: &str) -> Value {
    json!({ "service":"compatible", "endpoint":endpoint, "bucket":"backups", "prefix":"workspace/", "region":"auto", "access_key_id":"owner-access", "secret_access_key":SECRET, "path_style":true })
}
fn update(revision: i64, endpoint: &str) -> Value {
    json!({ "expected_revision":revision, "enabled":true, "interval_secs":0, "keep":2, "destination":destination(endpoint) })
}

struct Store {
    endpoint: String,
    task: tokio::task::JoinHandle<BTreeMap<String, Vec<u8>>>,
    stop: CancellationToken,
}
impl Drop for Store {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
impl Store {
    async fn new(deny: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let stop = CancellationToken::new();
        let quit = stop.clone();
        let task = tokio::spawn(async move {
            let mut objects = BTreeMap::<String, Vec<u8>>::new();
            loop {
                let accepted = tokio::select! { _ = quit.cancelled() => break, value = listener.accept() => value };
                let (mut stream, _) = accepted.unwrap();
                let mut request = Vec::new();
                let header_end = loop {
                    let mut piece = [0u8; 8192];
                    let n = stream.read(&mut piece).await.unwrap();
                    if n == 0 {
                        break None;
                    }
                    request.extend_from_slice(&piece[..n]);
                    if let Some(index) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(index + 4);
                    }
                };
                let Some(header_end) = header_end else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]).to_string();
                let mut words = headers.lines().next().unwrap().split_whitespace();
                let method = words.next().unwrap();
                let target = words.next().unwrap();
                let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while request.len() < header_end + length {
                    let mut piece = [0u8; 8192];
                    let n = stream.read(&mut piece).await.unwrap();
                    assert_ne!(n, 0);
                    request.extend_from_slice(&piece[..n]);
                }
                let key = url
                    .path()
                    .strip_prefix("/backups/")
                    .unwrap_or("")
                    .to_owned();
                let mut status = "200 OK";
                let mut body = Vec::new();
                if deny {
                    status = "403 Forbidden";
                    body = format!(
                        "<Error><Code>AccessDenied</Code><Message>{SECRET}</Message></Error>"
                    )
                    .into_bytes();
                } else if method == "GET" && key.is_empty() {
                    if url.query_pairs().any(|(k, _)| k == "uploads") {
                        body = b"<ListMultipartUploadsResult/>".to_vec();
                    } else {
                        let prefix = url
                            .query_pairs()
                            .find(|(k, _)| k == "prefix")
                            .map(|(_, v)| v.into_owned())
                            .unwrap_or_default();
                        let listing = objects
                            .iter()
                            .filter(|(k, _)| k.starts_with(&prefix))
                            .map(|(k, v)| {
                                format!(
                                    "<Contents><Key>{k}</Key><Size>{}</Size></Contents>",
                                    v.len()
                                )
                            })
                            .collect::<String>();
                        body = format!("<ListBucketResult><IsTruncated>false</IsTruncated>{listing}</ListBucketResult>").into_bytes();
                    }
                } else if method == "POST" && url.query_pairs().any(|(k, _)| k == "uploadId") {
                    body = format!(
                        "<Error><Code>InternalError</Code><Message>{SECRET}</Message></Error>"
                    )
                    .into_bytes();
                } else if method == "PUT" {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    if headers.to_ascii_lowercase().contains("if-none-match: *")
                        && objects.contains_key(&key)
                    {
                        status = "412 Precondition Failed";
                    } else {
                        objects.insert(key, request[header_end..header_end + length].to_vec());
                    }
                } else if method == "GET" || method == "HEAD" {
                    match objects.get(&key) {
                        Some(value) => body = value.clone(),
                        None => status = "404 Not Found",
                    }
                } else if method == "DELETE" {
                    objects.remove(&key);
                    status = "204 No Content";
                } else {
                    panic!("unexpected mock request {method} {target}");
                }
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                if method != "HEAD" {
                    stream.write_all(&body).await.unwrap();
                }
                stream.shutdown().await.unwrap();
            }
            objects
        });
        Self {
            endpoint,
            task,
            stop,
        }
    }
}

#[tokio::test]
async fn settings_are_owner_only_validated_revisioned_and_secret_redacted() {
    let mut h = Harness::new().await;
    let unauth = h.app.call("GET", "/v1/settings/backups", None, None).await;
    assert_eq!(unauth.status, StatusCode::UNAUTHORIZED);
    let initial = h.snapshot().await;
    assert_eq!(initial["revision"], 0);
    assert_eq!(initial["enabled"], false);
    assert!(initial["next_backup_at"].is_null());
    let pages: i64 = sqlx::query_scalar("PRAGMA page_count")
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    let page_size: i64 = sqlx::query_scalar("PRAGMA page_size")
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        initial["estimate"]["bytes"].as_u64().unwrap(),
        (pages * page_size) as u64
    );
    let saved = h
        .app
        .owner(
            "PUT",
            "/v1/settings/backups",
            Some(update(0, "https://storage.example.com")),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["destination"]["has_secret"], true);
    assert!(!saved.body.to_string().contains(SECRET));
    assert!(
        !saved.body["destination"]
            .as_object()
            .unwrap()
            .contains_key("secret_access_key")
    );
    let (salt, cipher): (Vec<u8>, Vec<u8>) =
        sqlx::query_as("SELECT secret_salt, secret_cipher FROM backup_settings")
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert_eq!(salt.len(), 32);
    assert!(!cipher.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()));
    let stale = h
        .app
        .owner(
            "PUT",
            "/v1/settings/backups",
            Some(update(0, "https://other.example.com")),
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT);
    let mut body = update(1, "https://storage.example.com");
    body["destination"]["secret_access_key"] = Value::Null;
    body["keep"] = json!(4);
    let saved = h.app.owner("PUT", "/v1/settings/backups", Some(body)).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["keep"], 4);
    let key = SecretKey::parse(KEY).unwrap();
    let loaded = else_api::backup::settings::load(&h.app.db.pool, &key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        loaded
            .destination
            .unwrap()
            .s3_config()
            .secret_access_key
            .as_str(),
        SECRET
    );
    let wrong = SecretKey::parse("AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=").unwrap();
    assert!(
        else_api::backup::settings::load(&h.app.db.pool, &wrong)
            .await
            .is_err()
    );
    h.restart().await;
    let restarted = h.snapshot().await;
    assert_eq!(restarted["revision"], 2);
    assert_eq!(restarted["keep"], 4);
    assert_eq!(restarted["destination"]["has_secret"], true);
    for (field, value) in [("keep", json!(0)), ("interval_secs", json!(1))] {
        let mut body = update(2, "https://storage.example.com");
        body[field] = value;
        assert_eq!(
            h.app
                .owner("PUT", "/v1/settings/backups", Some(body))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    let mut body = update(2, "https://storage.example.com");
    body["destination"]["endpoint"] = json!("https://user:password@example.com/path");
    let bad = h.app.owner("PUT", "/v1/settings/backups", Some(body)).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(!bad.body.to_string().contains("password"));
}

#[tokio::test]
async fn connection_probe_checks_permissions_and_removes_only_its_own_object() {
    let h = Harness::new().await;
    let mut store = Store::new(false).await;
    let response = h
        .app
        .owner(
            "POST",
            "/v1/settings/backups/test",
            Some(json!({"expected_revision":0,"destination":destination(&store.endpoint)})),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(
        h.snapshot().await["revision"],
        0,
        "testing does not save configuration"
    );
    store.stop.cancel();
    let objects = (&mut store.task).await;
    assert!(objects.unwrap().is_empty(), "the probe is removed");
}

#[tokio::test]
async fn rejected_connection_never_returns_remote_response_secrets() {
    let h = Harness::new().await;
    let store = Store::new(true).await;
    let response = h
        .app
        .owner(
            "POST",
            "/v1/settings/backups/test",
            Some(json!({"expected_revision":0,"destination":destination(&store.endpoint)})),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(!response.body.to_string().contains(SECRET));
    assert!(
        response.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("permissions")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_backup_is_nonblocking_verified_destination_scoped_and_retained() {
    let h = Harness::new().await;
    let store = Store::new(false).await;
    let save = h
        .app
        .owner(
            "PUT",
            "/v1/settings/backups",
            Some(update(0, &store.endpoint)),
        )
        .await;
    assert_eq!(save.status, StatusCode::OK, "{}", save.body);
    assert!(
        save.body["next_backup_at"].is_null(),
        "manual policy has no scheduled run"
    );
    for count in 1..=3 {
        let start = h.app.owner("POST", "/v1/settings/backups/run", None).await;
        assert_eq!(start.status, StatusCode::ACCEPTED, "{}", start.body);
        let duplicate = h.app.owner("POST", "/v1/settings/backups/run", None).await;
        assert_eq!(duplicate.status, StatusCode::CONFLICT);
        let current = tokio::time::timeout(Duration::from_secs(1), h.snapshot())
            .await
            .unwrap();
        assert_eq!(current["running"], true);
        let saving = h
            .app
            .owner(
                "PUT",
                "/v1/settings/backups",
                Some(update(1, &store.endpoint)),
            )
            .await;
        assert_eq!(
            saving.status,
            StatusCode::CONFLICT,
            "settings cannot race an active upload"
        );
        let done = h.wait_idle().await;
        assert!(done["last_error"].is_null(), "{done}");
        assert_eq!(done["remote_count"], count.min(2));
        assert!(done["remote_bytes"].as_u64().unwrap() > 0);
        assert!(
            done["estimate"]["basis"]
                .as_str()
                .unwrap()
                .contains("latest complete")
        );
        assert_eq!(done["recent"][0]["remote_status"], "completed");
    }
    let mut next = update(1, &store.endpoint);
    next["enabled"] = json!(false);
    next["destination"]["prefix"] = json!("other-workspace/");
    next["destination"]["secret_access_key"] = Value::Null;
    let saved = h.app.owner("PUT", "/v1/settings/backups", Some(next)).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(
        saved.body["remote_count"], 0,
        "a different prefix has separate status"
    );
    assert_eq!(saved.body["recent"], json!([]));
    assert!(saved.body["next_backup_at"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pausing_remote_backups_preserves_the_local_capture_schedule_and_retention() {
    let mut h = Harness::new().await;
    let mut body = update(0, "https://storage.example.com");
    body["enabled"] = json!(false);
    let saved = h.app.owner("PUT", "/v1/settings/backups", Some(body)).await;
    assert_eq!(saved.status, StatusCode::OK);
    h.config.backup.interval = Duration::from_millis(50);
    h.config.backup.keep = 2;
    h.restart().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM backup_runs WHERE local_status = 'completed'",
            )
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
            if count >= 4 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let snapshot = h.snapshot().await;
    assert_eq!(snapshot["enabled"], false);
    assert_eq!(snapshot["remote_count"], 0);
    assert!(snapshot["next_backup_at"].is_null());
    h.cancel.cancel();
    (&mut h.task).await.unwrap();
    assert_eq!(
        else_api::backup::completed_sets(&h.config.backup.dir)
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_uploads_pending_sets_without_waiting_for_the_next_remote_interval() {
    let mut h = Harness::new().await;
    let store = Store::new(false).await;
    let response = h
        .app
        .owner(
            "PUT",
            "/v1/settings/backups",
            Some(update(0, &store.endpoint)),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        h.app
            .owner("POST", "/v1/settings/backups/run", None)
            .await
            .status,
        StatusCode::ACCEPTED
    );
    assert_eq!(h.wait_idle().await["remote_count"], 1);
    let settings =
        else_api::backup::settings::load(&h.app.db.pool, &SecretKey::parse(KEY).unwrap())
            .await
            .unwrap()
            .unwrap();
    let s3 = else_api::backup::s3::S3::new(settings.destination.unwrap().s3_config()).unwrap();
    let worker = else_api::backup::worker::BackupWorker::new(
        h.app.db.pool.clone(),
        else_api::backup::DatabaseParams::parse(h.app.db.url.expose()).unwrap(),
        Arc::new(SecretKey::parse(KEY).unwrap()),
        h.config.backup.clone(),
    )
    .with_replicator(else_api::backup::replicate::Replicator::new(
        s3,
        h.app.db.pool.clone(),
        2,
    ));
    worker
        .run_local_until(&CancellationToken::new())
        .await
        .unwrap();
    let mut policy = update(1, &store.endpoint);
    policy["interval_secs"] = json!(3600);
    assert_eq!(
        h.app
            .owner("PUT", "/v1/settings/backups", Some(policy))
            .await
            .status,
        StatusCode::OK
    );
    h.restart().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = h.snapshot().await;
            if snapshot["remote_count"] == 2 && snapshot["running"] == false {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM backup_runs WHERE local_status = 'completed'")
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert_eq!(
        count, 2,
        "pending sets upload without creating another local capture"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_environment_destination_keeps_local_backups_and_api_available() {
    let mut h = Harness::new().await;
    sqlx::query("DELETE FROM backup_settings")
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    h.config.s3 = Some(else_api::backup::s3::S3Config {
        endpoint: "https://storage.example.com/invalid/path".into(),
        region: "auto".into(),
        bucket: "backups".into(),
        prefix: "workspace/".into(),
        access_key_id: "owner".into(),
        secret_access_key: zeroize::Zeroizing::new(SECRET.into()),
        path_style: true,
    });
    h.config.backup.interval = Duration::from_millis(50);
    h.restart().await;
    let snapshot = h.snapshot().await;
    assert_eq!(snapshot["enabled"], false);
    assert!(snapshot["destination"].is_null());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM backup_runs WHERE local_status = 'completed'",
            )
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
            if count > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let persisted: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backup_settings")
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        persisted, 0,
        "a corrected environment can be retried on restart"
    );
}

#[tokio::test]
async fn a_failed_capture_retries_capture_before_the_full_remote_interval() {
    let mut h = Harness::new().await;
    let store = Store::new(false).await;
    let mut policy = update(0, &store.endpoint);
    policy["enabled"] = json!(false);
    policy["interval_secs"] = json!(86400);
    assert_eq!(
        h.app
            .owner("PUT", "/v1/settings/backups", Some(policy))
            .await
            .status,
        StatusCode::OK
    );
    sqlx::query("UPDATE backup_settings SET enabled = 1")
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    h.config.backup.dir = h._dir.path().join("blocked-backups");
    std::fs::write(&h.config.backup.dir, b"not a directory").unwrap();
    h.restart().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = h.snapshot().await;
            if !snapshot["last_error"].is_null() && snapshot["running"] == false {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    std::fs::remove_file(&h.config.backup.dir).unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    tokio::time::resume();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = h.snapshot().await;
            if snapshot["remote_count"] == 1 && snapshot["running"] == false {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(h.snapshot().await["last_error"].is_null());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_successful_local_capture_does_not_clear_a_failed_remote_upload() {
    let mut h = Harness::new().await;
    let store = Store::new(true).await;
    h.config.backup.interval = Duration::from_millis(200);
    h.restart().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM backup_runs WHERE local_status = 'completed'",
            )
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
            if count > 0 && h.snapshot().await["running"] == false {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let saved = h
        .app
        .owner(
            "PUT",
            "/v1/settings/backups",
            Some(update(0, &store.endpoint)),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(
        h.app
            .owner("POST", "/v1/settings/backups/run", None)
            .await
            .status,
        StatusCode::ACCEPTED
    );
    let failed = h.wait_idle().await;
    assert!(
        failed["last_error"]
            .as_str()
            .unwrap()
            .contains("Remote upload failed")
    );
    let before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM backup_runs WHERE local_status = 'completed'")
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM backup_runs WHERE local_status = 'completed'",
            )
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
            if count > before && h.snapshot().await["running"] == false {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let snapshot = h.snapshot().await;
    assert!(
        snapshot["last_error"]
            .as_str()
            .unwrap()
            .contains("Remote upload failed")
    );
    assert!(!snapshot.to_string().contains(SECRET));
}

#[tokio::test]
async fn multipart_error_documents_do_not_expose_remote_response_secrets() {
    let store = Store::new(false).await;
    let destination = else_api::backup::settings::Destination::resolve(
        serde_json::from_value(destination(&store.endpoint)).unwrap(),
        None,
    )
    .unwrap();
    let client = else_api::backup::s3::S3::new(destination.s3_config()).unwrap();
    let error = client
        .complete_multipart(
            "workspace/test",
            "fixture-upload",
            &[(1, "fixture-etag".into())],
        )
        .await
        .unwrap_err();
    assert!(!error.to_string().contains(SECRET));
    assert!(!format!("{error:?}").contains(SECRET));
}

#[tokio::test]
async fn the_estimate_follows_the_latest_whole_assistant_set_over_a_newer_database_only_one() {
    let h = Harness::new().await;
    for (set, finished, bytes, whole) in [
        ("whole", "2026-11-10T12:00:00.000+00:00", 13_000_000_000_i64, true),
        ("before-update", "2026-11-10T13:00:00.000+00:00", 30_000_000, false),
    ] {
        sqlx::query(
            "INSERT INTO backup_runs (id, set_id, finished_at, local_status, ciphertext_bytes, coverage)
             VALUES (?1, ?2, ?3, 'completed', ?4, ?5)",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(set)
        .bind(finished)
        .bind(bytes)
        .bind(json!({ "whole_assistant": whole }).to_string())
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    }
    assert_eq!(h.snapshot().await["estimate"]["bytes"], 13_000_000_000_u64);
}
