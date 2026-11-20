#![allow(dead_code)]
pub mod guest;
pub mod recorder;
pub mod steer;

/// How long a test waits for a state it expects to reach. It only bounds a
/// hang: a passing wait returns as soon as the state appears, and several
/// suites sharing one machine can slow a step tenfold.
/// The `SECRET_KEY` every test app seals Vault entries with.
pub const SECRET_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
pub const SETTLE: std::time::Duration = std::time::Duration::from_secs(60);
/// A chunked local backup set rebuilt whole in a new directory, as a remote
/// download leaves it, for tests that read or damage its files.
pub fn rebuilt_set(
    set: &std::path::Path,
    key: &else_api::credentials::SecretKey,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join(set.file_name().unwrap());
    let opened = else_api::backup::store::open(set, key).unwrap();
    else_api::backup::store::rebuild(set, &out, opened, u64::MAX).unwrap();
    (root, out)
}
use else_api::config::{Config, DatabaseUrl};
use sqlx::SqlitePool;
pub struct TestDb {
    pub pool: SqlitePool,
    pub url: DatabaseUrl,
    _dir: tempfile::TempDir,
}
impl TestDb {
    pub async fn empty() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let url = DatabaseUrl::new(format!(
            "sqlite://{}",
            dir.path().join("test.sqlite3").display()
        ));
        let pool = else_api::db::connect(&url).await.unwrap();
        Self {
            pool,
            url,
            _dir: dir,
        }
    }
    pub async fn migrated() -> Self {
        // In-process test servers render visuals with the real API binary's worker.
        else_visual::isolate::use_worker(env!("CARGO_BIN_EXE_else-api").into());
        let db = Self::empty().await;
        else_api::db::migrate(&db.pool).await.unwrap();
        db
    }
    pub fn dir(&self) -> &std::path::Path {
        self._dir.path()
    }
}

pub const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

pub struct TestApp {
    pub router: axum::Router,
    pub state: std::sync::Arc<else_api::http::AppState>,
    pub scheduler: std::sync::Arc<else_api::scheduler::Scheduler>,
    pub config: Config,
    pub db: std::sync::Arc<TestDb>,
    /// Stops the API's loops when the app is dropped.
    _background: else_api::runtime::Background,
}

/// What a test substitutes in the production composition.
pub struct Setup {
    pub max_active: usize,
    pub registry: else_api::providers::Registry,
    pub computers: Option<std::sync::Arc<else_api::computers::ComputerManager>>,
    pub mail: Option<std::sync::Arc<dyn else_api::connectors::types::MailTransport>>,
    pub social: Option<std::sync::Arc<dyn else_api::socials::providers::Transport>>,
    pub skill_sources: else_api::skills::Sources,
    pub memory: else_api::runtime::Memory,
    pub backups: bool,
}

impl Default for Setup {
    /// The synthetic provider and a profiled fake, four turns at once.
    fn default() -> Self {
        use std::sync::Arc;
        Self::with_registry(else_api::providers::Registry::new(vec![
            Arc::new(else_api::providers::synthetic::Synthetic::new(
                std::time::Duration::from_millis(5),
            )),
            Arc::new(ProfiledFake),
        ]))
    }
}

impl Setup {
    pub fn with_registry(registry: else_api::providers::Registry) -> Self {
        Self {
            max_active: 4,
            registry,
            computers: None,
            mail: None,
            social: None,
            skill_sources: else_api::skills::Sources::default(),
            memory: else_api::runtime::Memory::Deterministic,
            backups: false,
        }
    }
}

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
        stop: else_api::app::providers::Stop,
    ) -> else_api::app::providers::TurnOutcome {
        else_api::providers::synthetic::Synthetic::new(std::time::Duration::from_millis(5))
            .run(input, sink, stop)
            .await
    }
}

pub struct Reply {
    pub status: axum::http::StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: serde_json::Value,
}

impl TestApp {
    pub async fn installed() -> Self {
        let db = TestDb::migrated().await;
        else_api::db::installation::setup(&db.pool).await.unwrap();
        Self::with_db(db).await
    }

    pub async fn with_db(db: TestDb) -> Self {
        Self::start(std::sync::Arc::new(db), Setup::default()).await
    }

    /// Stop this app and start another over the same database, as after a
    /// process restart.
    pub async fn restart(self) -> Self {
        let db = self.stop().await;
        Self::start(db, Setup::default()).await
    }

    /// Stop the API's loops and wait for them, returning the database.
    pub async fn stop(self) -> std::sync::Arc<TestDb> {
        let Self {
            db, _background, ..
        } = self;
        _background.stop().await;
        db
    }

    /// The configuration of an app over `db`, with its home directory
    /// inside the database's directory.
    pub fn config(db: &TestDb) -> Config {
        let home = db.dir().to_str().unwrap().to_owned();
        let mut config = Config::load(&MapEnv(vec![("HOME", home)])).unwrap();
        config.database_url = db.url.clone();
        config.shutdown_grace = std::time::Duration::from_secs(2);
        config
    }

    pub async fn with_options(db: std::sync::Arc<TestDb>, max_active: usize) -> Self {
        Self::start(
            db,
            Setup {
                max_active,
                ..Setup::default()
            },
        )
        .await
    }

    pub async fn with_registry(
        db: std::sync::Arc<TestDb>,
        max_active: usize,
        registry: else_api::providers::Registry,
    ) -> Self {
        Self::start(
            db,
            Setup {
                max_active,
                ..Setup::with_registry(registry)
            },
        )
        .await
    }

    pub async fn with_parts(
        db: std::sync::Arc<TestDb>,
        max_active: usize,
        registry: else_api::providers::Registry,
        computers: Option<std::sync::Arc<else_api::computers::ComputerManager>>,
    ) -> Self {
        Self::start(
            db,
            Setup {
                max_active,
                computers,
                ..Setup::with_registry(registry)
            },
        )
        .await
    }

    pub async fn with_skill_sources(
        db: std::sync::Arc<TestDb>,
        registry: else_api::providers::Registry,
        skill_sources: else_api::skills::Sources,
    ) -> Self {
        Self::start(
            db,
            Setup {
                skill_sources,
                ..Setup::with_registry(registry)
            },
        )
        .await
    }

    pub async fn with_mail(
        db: std::sync::Arc<TestDb>,
        max_active: usize,
        registry: else_api::providers::Registry,
        mail: std::sync::Arc<dyn else_api::connectors::types::MailTransport>,
    ) -> Self {
        Self::start(
            db,
            Setup {
                max_active,
                mail: Some(mail),
                ..Setup::with_registry(registry)
            },
        )
        .await
    }

    /// The production composition, with the test's parts, over `db` and a
    /// home directory inside it.
    pub async fn start(db: std::sync::Arc<TestDb>, setup: Setup) -> Self {
        let config = Self::config(&db);
        Self::start_in(config, db, setup).await
    }

    /// [`TestApp::start`] with the test's own configuration.
    pub async fn start_in(mut config: Config, db: std::sync::Arc<TestDb>, setup: Setup) -> Self {
        use std::sync::Arc;
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        config.max_active_turns = setup.max_active;
        let parts = else_api::runtime::Parts {
            owner_token: else_api::credentials::OwnerToken::parse(TOKEN).unwrap(),
            secret_key: Arc::new(else_api::credentials::SecretKey::parse(SECRET_KEY).unwrap()),
            pool: db.pool.clone(),
            registry: setup.registry,
            memory: setup.memory,
            computers: else_api::runtime::ComputerSource::Given(match setup.computers {
                Some(computers) => else_api::runtime::Computers::Ready(computers),
                None => {
                    else_api::runtime::Computers::Unavailable("this test runs no computers".into())
                }
            }),
            mail: setup
                .mail
                .unwrap_or_else(|| Arc::new(else_api::connectors::pimalaya::Pimalaya)),
            social: setup
                .social
                .unwrap_or_else(|| Arc::new(else_api::socials::providers::Native)),
            skill_sources: setup.skill_sources,
            cancel_grace: std::time::Duration::from_secs(1),
            backups: if setup.backups {
                else_api::runtime::Backups::Start
            } else {
                else_api::runtime::Backups::Off("this test runs no backups".into())
            },
        };
        let (state, background) =
            else_api::runtime::start(&config, parts, tokio_util::sync::CancellationToken::new())
                .await
                .unwrap();
        let router = else_api::http::router(&config, state.clone());
        Self {
            router,
            scheduler: state.scheduler.clone(),
            state,
            config,
            db,
            _background: background,
        }
    }

    pub async fn wait_turn(&self, turn_id: &str, statuses: &[&str]) -> String {
        self.wait_turn_for(turn_id, statuses, SETTLE).await
    }

    pub async fn wait_turn_for(
        &self,
        turn_id: &str,
        statuses: &[&str],
        timeout: std::time::Duration,
    ) -> String {
        let id: uuid::Uuid = turn_id.parse().unwrap();
        for _ in 0..(timeout.as_millis() / 25) {
            let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
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

    /// What the bot list says the bot's active turn is parked on.
    pub async fn waiting_on(&self, bot: &str) -> serde_json::Value {
        self.owner("GET", &format!("/v1/bots/{bot}"), None)
            .await
            .body["bot"]["activity"]["waiting_on"]
            .clone()
    }

    pub async fn main_bot(&self) -> String {
        self.owner("GET", "/v1/bots", None).await.body["default_bot_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    pub async fn create_bot(&self, name: &str) -> String {
        let reply = self
            .owner(
                "POST",
                "/v1/bots",
                Some(serde_json::json!({ "request_id": uuid::Uuid::new_v4(), "name": name })),
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

struct MapEnv(Vec<(&'static str, String)>);

impl else_api::config::Source for MapEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
    }
}

pub struct ApiProcess {
    pub child: std::process::Child,
    pub base: String,
    pub token: String,
}

#[allow(dead_code)]
pub fn without_dotenv(dir: &std::path::Path) -> &std::path::Path {
    std::fs::write(dir.join(".env"), "").unwrap();
    dir
}

/// The URL `child`, an `else-api serve` bound to port 0, published in
/// `data_dir` once it answers `/readyz`, or `None` after `limit`.
pub async fn ready_url(
    data_dir: &std::path::Path,
    child: &std::process::Child,
    limit: std::time::Duration,
) -> Option<String> {
    let manifest = data_dir.join(else_api::runtime_lock::MANIFEST);
    let client = reqwest::Client::new();
    tokio::time::timeout(limit, async {
        loop {
            let published = std::fs::read(&manifest).ok().and_then(|body| {
                serde_json::from_slice::<else_api::runtime_lock::Manifest>(&body).ok()
            });
            if let Some(published) = published.filter(|m| m.pid == child.id())
                && client
                    .get(format!("{}/readyz", published.url))
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success())
            {
                return published.url;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .ok()
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
        .env("ELSE_MAX_ACTIVE_TURNS", "2")
        .env("ELSE_SHUTDOWN_GRACE_SECS", "2")
        .env("ELSE_BACKUP_INTERVAL_SECS", "0")
        .env("RUST_LOG", "warn")
        .current_dir(data_dir)
        .stdout(std::process::Stdio::null());
    command
}

impl ApiProcess {
    pub async fn start(db: &TestDb, data_dir: &std::path::Path) -> Self {
        if !data_dir.join("owner-token").exists() {
            let status = api_command(db, data_dir, 1).arg("setup").status().unwrap();
            assert!(status.success());
        }
        // Port 0: the server binds and publishes its own address, so no
        // other process can take the port between choosing and binding it.
        let child = api_command(db, data_dir, 0).arg("serve").spawn().unwrap();
        let token = std::fs::read_to_string(data_dir.join("owner-token"))
            .unwrap()
            .trim()
            .to_owned();
        let started = ready_url(data_dir, &child, SETTLE).await;
        let Some(base) = started else {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            panic!("API did not start");
        };
        Self { child, base, token }
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

pub async fn rows(conn: &mut sqlx::SqliteConnection, sql: &str) -> Vec<String> {
    use sqlx::{Row, TypeInfo, ValueRef};
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_all(conn)
        .await
        .unwrap();
    let mut out: Vec<String> = rows
        .iter()
        .map(|row| {
            let values: Vec<serde_json::Value> = Vec::from_fn(row.len(), |i| {
                let value = row.try_get_raw(i).unwrap();
                if value.is_null() {
                    return serde_json::Value::Null;
                }
                match value.type_info().name() {
                    "INTEGER" => serde_json::json!(row.get::<i64, _>(i)),
                    "REAL" => serde_json::json!(row.get::<f64, _>(i)),
                    "BLOB" => serde_json::json!(hex::encode(row.get::<Vec<u8>, _>(i))),
                    _ => serde_json::json!(row.get::<String, _>(i)),
                }
            });
            serde_json::to_string(&values).unwrap()
        })
        .collect();
    out.sort();
    out
}

pub fn remove_database(options: &sqlx::sqlite::SqliteConnectOptions, name: &str) {
    let params = else_api::backup::DatabaseParams {
        options: options.clone(),
    }
    .with_database(name);
    let path = params.options.get_filename();
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}
