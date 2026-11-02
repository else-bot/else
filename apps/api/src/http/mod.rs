//! HTTP transport. Handlers translate requests into application operations.

pub mod error;

use axum::extract::DefaultBodyLimit;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use error::ApiError;

pub fn router(config: &Config) -> Router {
    Router::new()
        .route("/healthz", get(liveness))
        .fallback(|| async { ApiError::not_found() })
        .layer(DefaultBodyLimit::max(config.max_body_bytes))
        // The default span records method and path only, never headers.
        .layer(TraceLayer::new_for_http())
}

/// Liveness only: it reports that the process serves HTTP, not that its
/// dependencies are ready.
async fn liveness() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::MapSource;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app() -> Router {
        router(&Config::load(&MapSource::new(&[])).unwrap())
    }

    #[tokio::test]
    async fn liveness_responds() {
        let response = app()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_route_uses_error_shape() {
        let response = app()
            .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["error"]["code"], "not_found");
        assert_eq!(value["error"]["retryable"], false);
    }
}
