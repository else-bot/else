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
