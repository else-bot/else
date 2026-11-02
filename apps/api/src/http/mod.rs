//! HTTP transport. Handlers translate requests into application operations.

pub mod auth;
pub mod bots;
pub mod error;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::routing::get;
use axum::{Json, Router, middleware};
use serde_json::json;
use sqlx::PgPool;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::config::Config;
use crate::credentials::OwnerToken;
use error::ApiError;

/// Shared request state. Credentials are held only as digests.
pub struct AppState {
    pub owner_token: OwnerToken,
    pub db: PgPool,
    /// Nonsecret identity of this API process, used to bind event cursors.
    pub server_instance_id: Uuid,
    pub auth_failures: auth::FailureLimiter,
}

impl AppState {
    pub fn new(owner_token: OwnerToken, db: PgPool) -> Self {
        Self {
            owner_token,
            db,
            server_instance_id: Uuid::new_v4(),
            auth_failures: auth::FailureLimiter::default(),
        }
    }
}

pub fn router(config: &Config, state: Arc<AppState>) -> Router {
    // Every route under /v1 requires the owner token.
    // `layer` (not `route_layer`) also covers the fallback, so unknown /v1
    // paths are authenticated before they report 404.
    let v1 = Router::new()
        .route("/capabilities", get(capabilities))
        .merge(bots::routes())
        .fallback(|| async { ApiError::not_found() })
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_owner,
        ));

    Router::new()
        .route("/healthz", get(liveness))
        .route("/readyz", get(readiness))
        .nest("/v1", v1)
        .fallback(|| async { ApiError::not_found() })
        .with_state(state)
        .layer(middleware::from_fn(auth::check_origin))
        .layer(DefaultBodyLimit::max(config.max_body_bytes))
        // The default span records method and path only, never headers.
        .layer(TraceLayer::new_for_http())
}

/// Liveness only: it reports that the process serves HTTP, not that its
/// dependencies are ready.
async fn liveness() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

/// Readiness: the database is reachable. Unauthenticated and content-free.
async fn readiness(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if crate::db::ready(&state.db).await {
        Ok(Json(json!({ "status": "ready" })))
    } else {
        Err(ApiError::unavailable(
            "database_unavailable",
            "The database is not reachable.",
        ))
    }
}

async fn capabilities(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "api_version": "v1",
        "server_instance_id": state.server_instance_id,
        "default_provider_id": null,
        "providers": [],
        "limits": {},
    }))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::config::tests::MapSource;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use base64::Engine;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    pub const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn app() -> Router {
        assert_eq!(
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(TOKEN)
                .unwrap()
                .len(),
            32
        );
        let config = Config::load(&MapSource::new(&[("HOME", "/h")])).unwrap();
        // A lazy pool never connects unless a handler queries it.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgresql://unused@127.0.0.1:1/unused")
            .unwrap();
        router(
            &config,
            Arc::new(AppState::new(OwnerToken::parse(TOKEN).unwrap(), pool)),
        )
    }

    async fn send(app: Router, request: Request<Body>) -> (StatusCode, serde_json::Value) {
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap_or_default())
    }

    fn get(path: &str) -> axum::http::request::Builder {
        Request::get(path).header(header::HOST, "127.0.0.1:7878")
    }

    #[tokio::test]
    async fn liveness_needs_no_token() {
        let (status, _) = send(app(), get("/healthz").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_route_uses_error_shape() {
        let (status, body) = send(app(), get("/nope").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "not_found");
        assert_eq!(body["error"]["retryable"], false);
    }

    #[tokio::test]
    async fn protected_route_requires_exactly_one_valid_bearer() {
        let app = app();
        let cases: Vec<Vec<String>> = vec![
            vec![],
            vec!["Bearer".into()],
            vec!["Basic dXNlcjpwYXNz".into()],
            vec![format!("Bearer {}", "B".repeat(43))],
            vec![format!("Bearer {TOKEN}x")],
            vec![format!("Bearer {TOKEN}"), format!("Bearer {TOKEN}")],
        ];
        for headers in cases {
            let mut request = get("/v1/capabilities");
            for value in &headers {
                request = request.header(header::AUTHORIZATION, value);
            }
            let (status, body) = send(app.clone(), request.body(Body::empty()).unwrap()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{headers:?}");
            assert_eq!(body["error"]["code"], "unauthorized");
        }
        // A token in the query string is never accepted.
        let (status, _) = send(
            app.clone(),
            get(&format!("/v1/capabilities?access_token={TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, body) = send(
            app,
            get("/v1/capabilities")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["api_version"], "v1");
    }

    #[tokio::test]
    async fn unknown_v1_routes_authenticate_before_not_found() {
        let (status, _) = send(app(), get("/v1/bots/unknown").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = send(
            app(),
            get("/v1/bots/unknown")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn browser_origin_and_foreign_host_are_rejected() {
        let (status, _) = send(
            app(),
            get("/v1/capabilities")
                .header(header::ORIGIN, "http://evil.example")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = send(
            app(),
            Request::get("/healthz")
                .header(header::HOST, "evil.example:7878")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // A tunnel's different local port is fine.
        let (status, _) = send(
            app(),
            Request::get("/healthz")
                .header(header::HOST, "localhost:17001")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn repeated_failures_are_rate_limited() {
        let app = app();
        for _ in 0..30 {
            let (status, _) = send(
                app.clone(),
                get("/v1/capabilities").body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let (status, _) = send(
            app.clone(),
            get("/v1/capabilities").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        // The owner is never locked out by someone else's failures.
        let (status, _) = send(
            app,
            get("/v1/capabilities")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
}
