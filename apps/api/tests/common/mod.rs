//! Shared helpers for tests that need real PostgreSQL.
//!
//! Each test gets its own freshly created database on the development server
//! (`TEST_DATABASE_URL`, else the `.env` settings), dropped afterwards. The
//! owner's application database is never touched.
#![allow(dead_code)]

use else_api::config::{Config, DatabaseUrl, ProcessEnv};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

pub struct TestDb {
    pub pool: PgPool,
    pub url: DatabaseUrl,
    name: String,
    admin: PgConnectOptions,
}

fn admin_options() -> PgConnectOptions {
    let _ = dotenvy::dotenv();
    let url = match std::env::var("TEST_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => Config::load(&ProcessEnv)
            .expect("configuration")
            .require_database()
            .expect("PostgreSQL tests need TEST_DATABASE_URL or the .env POSTGRES_* settings")
            .expose()
            .to_owned(),
    };
    url.parse().expect("valid PostgreSQL URL")
}

impl TestDb {
    /// A new, empty database. Migrations are not applied.
    pub async fn empty() -> Self {
        let admin = admin_options();
        let name = format!("else_test_{}", uuid::Uuid::new_v4().simple());
        let admin_pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(admin.clone())
            .await
            .expect("PostgreSQL for tests is not reachable; run `docker compose up -d --wait`");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&admin_pool)
            .await
            .unwrap();
        admin_pool.close().await;
        let options = admin.clone().database(&name);
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect_with(options.clone())
            .await
            .unwrap();
        let url = DatabaseUrl::new(options_url(&admin, &name));
        Self {
            pool,
            url,
            name,
            admin,
        }
    }

    /// A new database with all migrations applied.
    pub async fn migrated() -> Self {
        let db = Self::empty().await;
        else_api::db::migrate(&db.pool).await.unwrap();
        db
    }
}

fn options_url(admin: &PgConnectOptions, name: &str) -> String {
    let base = std::env::var("TEST_DATABASE_URL").ok().unwrap_or_else(|| {
        Config::load(&ProcessEnv)
            .unwrap()
            .require_database()
            .unwrap()
            .expose()
            .to_owned()
    });
    let _ = admin;
    // Replace the path component with the test database name.
    let (prefix, _) = base.rsplit_once('/').expect("URL with a database path");
    format!("{prefix}/{name}")
}

impl Drop for TestDb {
    fn drop(&mut self) {
        // Tests use the multi-threaded runtime so cleanup can block in place.
        let admin = self.admin.clone();
        let name = self.name.clone();
        let pool = self.pool.clone();
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                // `close` waits for checked-out connections (such as the event
                // listener's). Bound it; `WITH (FORCE)` ends the rest.
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), pool.close()).await;
                if let Ok(admin_pool) = PgPoolOptions::new()
                    .max_connections(1)
                    .connect_with(admin)
                    .await
                {
                    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
                    )))
                    .execute(&admin_pool)
                    .await;
                    admin_pool.close().await;
                }
            })
        });
    }
}

pub const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// The real router over a test database, driven in-process.
pub struct TestApp {
    pub router: axum::Router,
    pub state: std::sync::Arc<else_api::http::AppState>,
    pub scheduler: std::sync::Arc<else_api::scheduler::Scheduler>,
    stop: tokio_util::sync::CancellationToken,
    // Declared last so the scheduler stops before the database is dropped.
    pub db: std::sync::Arc<TestDb>,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// A provider that uses saved profiles, standing in for Codex in tests. It
/// accepts only the documented Codex profile values.
pub struct ProfiledFake;

#[async_trait::async_trait]
impl else_api::app::providers::Provider for ProfiledFake {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn label(&self) -> &'static str {
        "Codex (test double)"
    }
    fn availability(&self) -> else_api::app::providers::Availability {
        else_api::app::providers::Availability::Available
    }
    fn uses_profile(&self) -> bool {
        true
    }
    fn validate_profile(&self, p: &else_api::app::providers::Profile) -> Result<(), String> {
        if p.model != "gpt-6.1-sol" {
            return Err(format!("unsupported model {}", p.model));
        }
        if !["low", "medium", "high", "xhigh"].contains(&p.reasoning_effort.as_str()) {
            return Err("unsupported reasoning effort".into());
        }
        if !["default", "priority"].contains(&p.service_tier.as_str()) {
            return Err("unsupported service tier".into());
        }
        Ok(())
    }
    async fn run(
        &self,
        input: else_api::app::providers::TurnInput,
        sink: &dyn else_api::app::providers::TurnSink,
        cancel: tokio_util::sync::CancellationToken,
    ) -> else_api::app::providers::TurnOutcome {
        else_api::providers::synthetic::Synthetic::new(std::time::Duration::from_millis(5))
            .run(input, sink, cancel)
            .await
    }
}

pub struct Reply {
    pub status: axum::http::StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: serde_json::Value,
}

impl TestApp {
    /// Migrated database with explicit setup completed.
    pub async fn installed() -> Self {
        let db = TestDb::migrated().await;
        else_api::db::installation::setup(&db.pool).await.unwrap();
        Self::with_db(db)
    }

    pub fn with_db(db: TestDb) -> Self {
        Self::with_options(std::sync::Arc::new(db), 4)
    }

    /// Simulate an API restart: stop this scheduler, run startup
    /// reconciliation, and serve the same database again.
    pub async fn restart(self) -> Self {
        let db = self.db.clone();
        let scheduler = self.scheduler.clone();
        drop(self);
        // Wait for the old scheduler's turns to record their outcomes.
        for _ in 0..200 {
            if std::sync::Arc::strong_count(&scheduler) == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        else_api::db::turns::recover_after_restart(&db.pool)
            .await
            .unwrap();
        Self::with_options(db, 4)
    }

    /// The real router and a running scheduler with the synthetic provider
    /// (5 ms per chunk) and the profile-bearing test double.
    pub fn with_options(db: std::sync::Arc<TestDb>, max_active: usize) -> Self {
        use std::sync::Arc;
        let registry = else_api::providers::Registry::new(vec![
            Arc::new(else_api::providers::synthetic::Synthetic::new(
                std::time::Duration::from_millis(5),
            )),
            Arc::new(ProfiledFake),
        ]);
        Self::with_registry(db, max_active, registry)
    }

    pub fn with_registry(
        db: std::sync::Arc<TestDb>,
        max_active: usize,
        registry: else_api::providers::Registry,
    ) -> Self {
        Self::with_parts(db, max_active, registry, None)
    }

    /// With bot computers attached to the API state.
    pub fn with_parts(
        db: std::sync::Arc<TestDb>,
        max_active: usize,
        registry: else_api::providers::Registry,
        computers: Option<std::sync::Arc<else_api::computers::ComputerManager>>,
    ) -> Self {
        use std::sync::Arc;
        // `RUST_LOG=warn cargo test -- --nocapture` shows API logs.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        let config = Config::load(&MapEnv(vec![("HOME", "/tmp")])).unwrap();
        let scheduler = else_api::scheduler::Scheduler::with_cancel_grace(
            db.pool.clone(),
            registry,
            max_active,
            std::time::Duration::from_secs(1),
        );
        let stop = tokio_util::sync::CancellationToken::new();
        tokio::spawn(
            scheduler
                .clone()
                .run(stop.clone(), std::time::Duration::from_secs(2)),
        );
        let mut state = else_api::http::AppState::new(
            else_api::credentials::OwnerToken::parse(TOKEN).unwrap(),
            db.pool.clone(),
            scheduler.clone(),
            else_api::events::EventHub::start(db.pool.clone(), stop.clone()),
        );
        if let Some(computers) = computers {
            state = state.with_computers(computers);
        }
        // A fixed test master key, for task secrets.
        state = state.with_secret_key(Arc::new(
            else_api::credentials::SecretKey::parse("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=")
                .unwrap(),
        ));
        let state = Arc::new(state);
        let router = else_api::http::router(&config, state.clone());
        Self {
            router,
            db,
            state,
            scheduler,
            stop,
        }
    }

    /// Poll the database until a turn reaches one of `statuses`.
    pub async fn wait_turn(&self, turn_id: &str, statuses: &[&str]) -> String {
        self.wait_turn_for(turn_id, statuses, std::time::Duration::from_secs(10))
            .await
    }

    pub async fn wait_turn_for(
        &self,
        turn_id: &str,
        statuses: &[&str],
        timeout: std::time::Duration,
    ) -> String {
        let id: uuid::Uuid = turn_id.parse().unwrap();
        for _ in 0..(timeout.as_millis() / 25) {
            let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = $1")
                .bind(id)
                .fetch_one(&self.db.pool)
                .await
                .unwrap();
            if statuses.contains(&status.as_str()) {
                return status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let all: Vec<(i64, String, Option<String>)> =
            sqlx::query_as("SELECT turn_no, status, error_code FROM turns ORDER BY created_at")
                .fetch_all(&self.db.pool)
                .await
                .unwrap();
        panic!("turn {turn_id} did not reach {statuses:?}; turns: {all:?}");
    }

    pub async fn main_bot(&self) -> String {
        self.owner("GET", "/v1/bots", None).await.body["default_bot_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    pub async fn create_bot(&self, name: &str, role: &str) -> String {
        let reply = self
            .owner(
                "POST",
                "/v1/bots",
                Some(serde_json::json!({ "request_id": uuid::Uuid::new_v4(), "name": name, "role": role })),
            )
            .await;
        assert_eq!(reply.status, axum::http::StatusCode::CREATED);
        reply.body["bot"]["id"].as_str().unwrap().to_owned()
    }

    pub async fn send(
        &self,
        bot: &str,
        request_id: uuid::Uuid,
        text: &str,
        provider: Option<&str>,
    ) -> Reply {
        self.owner(
            "POST",
            &format!("/v1/bots/{bot}/turns"),
            Some(serde_json::json!({ "request_id": request_id, "text": text, "provider_id": provider })),
        )
        .await
    }

    pub async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
        token: Option<&str>,
    ) -> Reply {
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "127.0.0.1:7878");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let request = match body {
            Some(body) => request
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string())),
            None => request.body(axum::body::Body::empty()),
        }
        .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        }
    }

    pub async fn owner(&self, method: &str, path: &str, body: Option<serde_json::Value>) -> Reply {
        self.call(method, path, body, Some(TOKEN)).await
    }
}

struct MapEnv(Vec<(&'static str, &'static str)>);

impl else_api::config::Source for MapEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.to_string())
    }
}

/// A real `else-api serve` process over a test database and a private data
/// directory, with the synthetic provider enabled.
pub struct ApiProcess {
    pub child: std::process::Child,
    pub base: String,
    pub token: String,
}

/// A working directory for a spawned `else-api` that stops `.env` lookup:
/// the binary loads the first `.env` found from its working directory
/// upward, and the repository's (or the owner's) must never configure a
/// test installation. Writes an empty `.env` into `dir` and returns it.
#[allow(dead_code)]
pub fn without_dotenv(dir: &std::path::Path) -> &std::path::Path {
    std::fs::write(dir.join(".env"), "").unwrap();
    dir
}

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn api_command(db: &TestDb, data_dir: &std::path::Path, port: u16) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_else-api"));
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", data_dir)
        .env("DATABASE_URL", db.url.expose())
        .env("ELSE_DATA_DIR", data_dir)
        .env("ELSE_API_BIND", format!("127.0.0.1:{port}"))
        .env("ELSE_SYNTHETIC_PROVIDER", "1")
        .env("ELSE_SHUTDOWN_GRACE_SECS", "2")
        // Scheduled backups are tested on their own (`tests/backup.rs`).
        .env("ELSE_BACKUP_INTERVAL_SECS", "0")
        .env("RUST_LOG", "warn")
        .current_dir(data_dir)
        .stdout(std::process::Stdio::null());
    command
}

impl ApiProcess {
    /// Run `setup` once for this data directory, then start `serve`.
    pub async fn start(db: &TestDb, data_dir: &std::path::Path) -> Self {
        if !data_dir.join("owner-token").exists() {
            let status = api_command(db, data_dir, 1).arg("setup").status().unwrap();
            assert!(status.success());
        }
        let port = free_port();
        let child = api_command(db, data_dir, port)
            .arg("serve")
            .spawn()
            .unwrap();
        let token = std::fs::read_to_string(data_dir.join("owner-token"))
            .unwrap()
            .trim()
            .to_owned();
        let process = Self {
            child,
            base: format!("http://127.0.0.1:{port}"),
            token,
        };
        let client = reqwest::Client::new();
        for _ in 0..200 {
            if client
                .get(format!("{}/readyz", process.base))
                .send()
                .await
                .is_ok()
            {
                return process;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("API did not start");
    }

    pub async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> (u16, serde_json::Value) {
        let mut request = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (
            status,
            response.json().await.unwrap_or(serde_json::Value::Null),
        )
    }

    /// SIGKILL: no shutdown code runs, like a crash or power loss.
    pub fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for ApiProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
