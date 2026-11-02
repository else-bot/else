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
                pool.close().await;
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
    pub db: TestDb,
    pub state: std::sync::Arc<else_api::http::AppState>,
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
        let config = Config::load(&MapEnv(vec![("HOME", "/tmp")])).unwrap();
        let state = std::sync::Arc::new(else_api::http::AppState::new(
            else_api::credentials::OwnerToken::parse(TOKEN).unwrap(),
            db.pool.clone(),
        ));
        let router = else_api::http::router(&config, state.clone());
        Self { router, db, state }
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
