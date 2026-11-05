pub mod attachments;
pub mod auth;
pub mod backups;
pub mod bots;
pub mod categories;
pub mod computers;
pub mod connectors;
pub mod conversation;
mod desktop_stream;
pub mod error;
pub mod guest_tools;
pub mod maintenance;
pub mod mcp;
pub mod memory;
pub mod onboarding;
pub mod outgoing_drafts;
pub(crate) mod outgoing_tools;
mod owner_tools;
pub mod questions;
pub mod routines;
pub mod settings;
mod social_tools;
pub mod socials;
pub mod task_secrets;
pub mod tools;
pub mod turns;
pub mod ws;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router, middleware};
use serde_json::json;
use sqlx::SqlitePool;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::app::providers::Availability;
use crate::config::Config;
use crate::credentials::OwnerToken;
use crate::scheduler::Scheduler;
use error::ApiError;

/// Shared request state and host-only services.
pub struct AppState {
    pub owner_token: OwnerToken,
    pub db: SqlitePool,
    pub scheduler: Arc<Scheduler>,
    pub events: crate::events::EventHub,
    /// Nonsecret identity of this API process, used to bind event cursors.
    pub server_instance_id: Uuid,
    pub auth_failures: auth::FailureLimiter,
    /// Bot computers, when a runtime is available.
    pub computers: Option<Arc<crate::computers::ComputerManager>>,
    /// The installation key for encrypted task and connector credentials.
    pub secret_key: Option<Arc<crate::credentials::SecretKey>>,
    pub backups: Option<crate::backup::service::Backups>,
    pub connectors: Option<Arc<crate::connectors::ConnectorManager>>,
    pub socials: Option<Arc<crate::socials::Manager>>,
    pub(crate) update_hold: crate::actor::Actor<Option<crate::maintenance::Closed>>,
    desktop_streams: desktop_stream::Streams,
}

impl AppState {
    pub fn new(
        owner_token: OwnerToken,
        db: SqlitePool,
        scheduler: Arc<Scheduler>,
        events: crate::events::EventHub,
    ) -> Self {
        Self {
            owner_token,
            db,
            scheduler,
            events,
            server_instance_id: Uuid::new_v4(),
            auth_failures: auth::FailureLimiter::default(),
            computers: None,
            secret_key: None,
            connectors: None,
            backups: None,
            socials: None,
            update_hold: crate::actor::Actor::default(),
            desktop_streams: desktop_stream::Streams::default(),
        }
    }

    /// Refuse new desktop viewer streams and end open ones; resolves once
    /// each has released its held input, control, and lease. Call before
    /// HTTP shutdown: upgraded connections are not part of axum's drain.
    pub fn stop_desktop_streams(&self) -> impl Future<Output = ()> + '_ {
        self.desktop_streams.stop()
    }

    pub fn with_secret_key(mut self, key: Arc<crate::credentials::SecretKey>) -> Self {
        self.connectors = Some(Arc::new(crate::connectors::ConnectorManager::new(
            self.db.clone(),
            key.clone(),
            self.scheduler.clone(),
        )));
        self.socials = Some(Arc::new(
            crate::socials::Manager::new(self.db.clone(), key.clone())
                .with_scheduler(self.scheduler.clone()),
        ));
        self.secret_key = Some(key);
        self
    }

    /// Replace the email connector manager, for a test mail transport.
    pub fn with_connectors(mut self, connectors: Arc<crate::connectors::ConnectorManager>) -> Self {
        self.connectors = Some(connectors);
        self
    }

    pub fn with_socials(mut self, socials: Arc<crate::socials::Manager>) -> Self {
        self.socials = Some(socials);
        self
    }

    pub fn with_computers(mut self, computers: Arc<crate::computers::ComputerManager>) -> Self {
        self.computers = Some(computers);
        self
    }
}

pub fn router(config: &Config, state: Arc<AppState>) -> Router {
    // Every route under /v1 requires the owner token.
    // `layer` (not `route_layer`) also covers the fallback, so unknown /v1
    // paths are authenticated before they report 404.
    let v1 = Router::new()
        .route("/capabilities", get(capabilities))
        .merge(bots::routes())
        .merge(categories::routes())
        .merge(turns::routes())
        .merge(attachments::routes())
        .merge(conversation::routes())
        .merge(ws::routes())
        .merge(memory::routes())
        .merge(computers::routes())
        .merge(connectors::routes())
        .merge(socials::routes())
        .merge(settings::routes())
        .merge(backups::routes())
        .merge(task_secrets::routes())
        .merge(questions::routes())
        .merge(outgoing_drafts::routes())
        .merge(routines::routes())
        .merge(maintenance::routes())
        .merge(onboarding::routes())
        .fallback(|| async { ApiError::not_found() })
        .method_not_allowed_fallback(|| async { method_not_allowed() })
        .layer(middleware::from_fn_with_state(
            state.clone(),
            maintenance_gate,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_owner,
        ));

    let max_body = config.max_body_bytes;
    Router::new()
        .route("/healthz", get(liveness))
        .route("/readyz", get(readiness))
        .nest("/v1", v1)
        .nest(
            "/tools/v1",
            tools::routes()
                .fallback(|| async { ApiError::not_found() })
                .layer(middleware::from_fn_with_state(state.clone(), tools_gate))
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    auth::require_task_grant,
                )),
        )
        // Paths such as `/v1/` that the nested router does not match are
        // still authenticated before reporting 404.
        .fallback(fallback)
        .method_not_allowed_fallback(|| async { method_not_allowed() })
        .with_state(state)
        .layer(middleware::from_fn(auth::check_origin))
        .layer(middleware::from_fn(move |request, next| {
            reject_large_body(max_body, request, next)
        }))
        .layer(DefaultBodyLimit::max(max_body))
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request| {
                tracing::info_span!("request", method = %request.method(), path = request.uri().path())
            }),
        )
}

fn method_not_allowed() -> ApiError {
    ApiError::new(
        axum::http::StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "This method is not allowed for this path.",
    )
}

async fn maintenance_gate(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    if crate::maintenance::allowed(request.method(), request.uri().path()) {
        return next.run(request).await;
    }
    let Some(_admission) = state.scheduler.gate().enter() else {
        return maintenance_response();
    };
    next.run(request).await
}

fn maintenance_response() -> Response {
    ApiError::unavailable(
        "maintenance",
        "else.bot is paused for a backup or an update; try again in a moment.",
    )
    .into_response()
}

async fn tools_gate(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let Some(_admission) = state.scheduler.gate().enter() else {
        return maintenance_response();
    };
    next.run(request).await
}

async fn fallback(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let path = request.uri().path();
    if path == "/v1" || path.starts_with("/v1/") {
        if let Err(response) = auth::authenticate(&state, request.headers()).await {
            return response;
        }
    }
    ApiError::not_found().into_response()
}

async fn reject_large_body(max: usize, request: Request, next: Next) -> Response {
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let max = if attachments::is_upload(request.method(), request.uri().path()) {
        max.max(attachments::MAX_UPLOAD_BODY_BYTES)
    } else {
        max
    };
    if declared.is_some_and(|len| len > max as u64) {
        return ApiError::new(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "The request body is too large.",
        )
        .into_response();
    }
    next.run(request).await
}

/// Liveness only: it reports that the process serves HTTP, not that its
/// dependencies are ready.
async fn liveness() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "version": crate::VERSION }))
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
    let registry = state.scheduler.registry();
    let (configured, defaults, onboarding_required) = tokio::join!(
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            sqlx::query_scalar("SELECT provider_id FROM provider_profiles").fetch_all(&state.db),
        ),
        settings::defaults(&state),
        onboarding::required(&state),
    );
    let configured: Option<Vec<String>> = configured.ok().and_then(Result::ok);
    let mut default_id = None;
    let providers: Vec<_> = registry
        .all()
        .iter()
        .map(|provider| {
            let has_profile = configured
                .as_ref()
                .map(|ids| ids.iter().any(|id| id == provider.id()));
            let (available, reason) = match provider.availability() {
                Availability::Unavailable(reason) => (false, Some(reason)),
                Availability::Available
                    if provider.uses_profile() && has_profile == Some(false) =>
                {
                    (
                        false,
                        Some(format!(
                            "No saved {} execution profile. Save an explicit model first \
                         (PATCH /v1/settings/providers/{}).",
                            provider.label(),
                            provider.id()
                        )),
                    )
                }
                Availability::Available => (true, None),
            };
            if available && default_id.is_none() {
                default_id = Some(provider.id());
            }
            let cli = provider.availability();
            json!({
                "id": provider.id(),
                "label": provider.label(),
                "available": available,
                "unavailable_reason": reason,
                "cli_available": cli == Availability::Available,
                "cli_unavailable_reason": match cli {
                    Availability::Unavailable(reason) => Some(reason),
                    Availability::Available => None,
                },
                "catalog": provider.catalog(),
                "requires_profile": provider.uses_profile(),
                "profile_configured": has_profile,
            })
        })
        .collect();
    let defaults = defaults.ok().flatten();
    let onboarding_required = onboarding_required.unwrap_or(false);
    Json(json!({
        "api_version": "v1",
        "bot_avatar_on_create": true,
        "server_instance_id": state.server_instance_id,
        "default_provider_id": default_id,
        "defaults": defaults,
        "onboarding_required": onboarding_required,
        "providers": providers,
        "limits": {
            "max_text_bytes": crate::app::messages::MAX_TEXT_BYTES,
            "max_bot_name_chars": crate::app::bots::MAX_NAME_CHARS,
        },
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
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(100))
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let scheduler = Scheduler::new(pool.clone(), crate::providers::Registry::new(vec![]), 1);
        router(
            &config,
            Arc::new(AppState::new(
                OwnerToken::parse(TOKEN).unwrap(),
                pool,
                scheduler,
                crate::events::EventHub::polling_only(),
            )),
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
    async fn oversized_and_wrong_method_requests_use_error_shape() {
        let (status, body) = send(
            app(),
            get("/healthz")
                .header(header::CONTENT_LENGTH, "70000")
                .body(Body::from(vec![b'x'; 70000]))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body["error"]["code"], "too_large");
        let (status, body) = send(
            app(),
            Request::post("/healthz")
                .header(header::HOST, "127.0.0.1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(body["error"]["code"], "method_not_allowed");
        let (status, _) = send(
            app(),
            Request::patch("/v1/capabilities")
                .header(header::HOST, "127.0.0.1")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
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
        assert_eq!(body["bot_avatar_on_create"], true);
    }

    #[tokio::test]
    async fn unknown_v1_routes_authenticate_before_not_found() {
        for path in ["/v1", "/v1/"] {
            let (status, _) = send(app(), get(path).body(Body::empty()).unwrap()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        }
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
