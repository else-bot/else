pub mod attachments;
pub mod auth;
pub mod backups;
mod bot_tools;
pub mod bots;
pub mod categories;
pub mod computers;
pub mod connectors;
pub mod context;
pub mod conversation;
mod delegation_tools;
mod desktop_stream;
pub mod error;
pub mod guest_tools;
mod mail_attachments;
pub mod maintenance;
pub mod mcp;
mod mcp_result;
pub mod mcp_wait;
pub mod memory;
mod memory_tools;
pub mod onboarding;
pub mod outgoing_drafts;
pub(crate) mod outgoing_tools;
mod owner_tools;
pub mod questions;
pub mod routines;
pub mod rules;
pub mod settings;
mod shared_mcp;
mod skills;
mod social_tools;
pub mod socials;
pub mod task_secrets;
pub(crate) mod tool_defs;
mod tool_result;
mod tool_search;
pub mod tools;
pub mod turns;
mod usage;
mod visual_tools;
mod waits;
pub mod watchers;
pub mod webhooks;
pub mod ws;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router, middleware};
use else_wire::settings::{Capabilities, Limits, ProviderInfo, ProviderState, ServerInfo, Shared};
use serde_json::json;
use sqlx::SqlitePool;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::app::providers::Availability;
use crate::config::Config;
use crate::credentials::OwnerToken;
use crate::scheduler::Scheduler;
use error::ApiError;

/// Shared request state and host-only services, built by
/// [`crate::runtime::start`].
pub struct AppState {
    pub owner_token: OwnerToken,
    pub db: SqlitePool,
    pub scheduler: Arc<Scheduler>,
    /// Provider adapters, shared with the scheduler.
    pub registry: crate::providers::Registry,
    /// The provider model cache, written by every model refresh.
    pub model_cache: crate::providers::models::Cache,
    /// The maintenance gate, shared with the scheduler and backups.
    pub gate: Arc<crate::maintenance::Gate>,
    /// Task grants issued to running turns. Revoked when a turn ends.
    pub grants: Arc<crate::grants::Grants>,
    pub events: crate::events::EventHub,
    /// Nonsecret identity of this API process, used to bind event cursors.
    pub server_instance_id: Uuid,
    pub auth_failures: auth::FailureLimiter,
    pub computers: crate::runtime::Computers,
    /// The installation key for encrypted task and connector credentials.
    pub secret_key: Arc<crate::credentials::SecretKey>,
    /// The backup service, or why it did not start (for `else_status`).
    pub backups: Result<crate::backup::service::Backups, String>,
    pub mcps: crate::mcp::Library,
    /// Where `skill_install` fetches skills from.
    pub skill_sources: crate::skills::Sources,
    pub connectors: Arc<crate::connectors::ConnectorManager>,
    pub socials: Arc<crate::socials::Manager>,
    pub(crate) update_hold: crate::actor::Actor<Option<crate::maintenance::Closed>>,
    pub(crate) server_updates: crate::actor::Actor<crate::server_updates::Installation>,
    pub(crate) desktop_streams: desktop_stream::Streams,
}

impl AppState {
    /// Refuse new desktop viewer streams and end open ones; resolves once
    /// each has released its held input, control, and lease. Call before
    /// HTTP shutdown: upgraded connections are not part of axum's drain.
    pub fn stop_desktop_streams(&self) -> impl Future<Output = ()> + '_ {
        self.desktop_streams.stop()
    }
}

/// A module's `/v1` routes, split by whether they wait out a maintenance
/// pause.
#[derive(Default)]
pub struct Routes {
    /// Served while paused: reads, Stop, stopping a computer, sending a
    /// message (its handler queues it for after the pause), and the update
    /// endpoints (which close and reopen the gate themselves).
    pub open: Router<Arc<AppState>>,
    /// Refused with `503 maintenance` while paused, and admitted through
    /// the gate otherwise, so a backup waits for them to finish.
    pub gated: Router<Arc<AppState>>,
}

impl Routes {
    pub fn open(open: Router<Arc<AppState>>) -> Self {
        Self {
            open,
            gated: Router::new(),
        }
    }

    pub fn gated(gated: Router<Arc<AppState>>) -> Self {
        Self {
            open: Router::new(),
            gated,
        }
    }

    fn merge(self, other: Self) -> Self {
        Self {
            open: self.open.merge(other.open),
            gated: self.gated.merge(other.gated),
        }
    }
}

pub fn router(config: &Config, state: Arc<AppState>) -> Router {
    let routes = [
        Routes::open(Router::new().route("/capabilities", get(capabilities))),
        bots::routes(),
        categories::routes(),
        turns::routes(),
        attachments::routes(),
        conversation::routes(),
        ws::routes(),
        memory::routes(),
        computers::routes(),
        connectors::routes(),
        socials::routes(),
        settings::routes(),
        backups::routes(),
        shared_mcp::routes(),
        skills::routes(),
        usage::routes(),
        task_secrets::routes(),
        questions::routes(),
        outgoing_drafts::routes(),
        routines::routes(),
        webhooks::routes(),
        watchers::routes(),
        rules::routes(),
        maintenance::routes(),
        onboarding::routes(),
        waits::routes(),
    ]
    .into_iter()
    .fold(Routes::default(), Routes::merge);
    // Every route under /v1 requires the owner token.
    // `layer` (not `route_layer`) also covers the fallback, so unknown /v1
    // paths are authenticated before they report 404.
    let v1 = routes
        .open
        .merge(routes.gated.route_layer(middleware::from_fn_with_state(
            state.clone(),
            maintenance_gate,
        )))
        .fallback(|| async { ApiError::not_found() })
        .method_not_allowed_fallback(|| async { method_not_allowed() })
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
        .nest(
            "/webhooks/v1",
            webhooks::trigger_routes()
                .fallback(|| async { ApiError::not_found() })
                .layer(middleware::from_fn_with_state(state.clone(), tools_gate)),
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
    let Some(_admission) = state.gate.enter() else {
        return maintenance_response();
    };
    next.run(request).await
}

const MAINTENANCE: &str = "else.bot is paused for a backup or an update; try again in a moment.";

fn maintenance_response() -> Response {
    ApiError::unavailable("maintenance", MAINTENANCE).into_response()
}

/// A tool call from a running turn (it carries the turn's grant) keeps
/// working while a backup waits for that turn to finish; other callers,
/// such as webhooks, wait for the pause to end.
async fn tools_gate(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let gate = &state.gate;
    if request.extensions().get::<crate::grants::Grant>().is_some() {
        let Some(_admission) = gate.enter_turn() else {
            return maintenance_response();
        };
        return next.run(request).await;
    }
    let Some(_admission) = gate.enter() else {
        return maintenance_response();
    };
    next.run(request).await
}

async fn fallback(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let path = request.uri().path();
    if (path == "/v1" || path.starts_with("/v1/"))
        && let Err(response) = auth::authenticate(&state, request.headers()).await
    {
        return *response;
    }
    ApiError::not_found().into_response()
}

async fn reject_large_body(max: usize, request: Request, next: Next) -> Response {
    let declared = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let (method, path) = (request.method(), request.uri().path());
    let max = if attachments::is_upload(method, path) {
        max.max(attachments::MAX_UPLOAD_BODY_BYTES)
    } else if settings::is_icon_upload(method, path) {
        max.max(settings::MAX_ICON_BODY_BYTES)
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
    let (configured, defaults, onboarding_required, server_name, server_icon) = tokio::join!(
        configured_profiles(&state.db),
        settings::defaults(&state),
        onboarding::required(&state),
        crate::db::installation::server_name(&state.db),
        crate::db::installation::server_icon(&state.db),
    );
    let providers: Vec<_> = state
        .registry
        .all()
        .iter()
        .map(|provider| provider_info(provider.as_ref(), configured.as_deref()))
        .collect();
    let default_provider_id = providers
        .iter()
        .find(|view| view.state.available)
        .map(|view| view.state.id.clone());
    Json(json!(Capabilities {
        api_version: Some("v1".into()),
        bot_avatar_on_create: true,
        answers_notice: true,
        server_instance_id: Some(state.server_instance_id),
        server: Some(ServerInfo {
            name: server_name.ok().flatten(),
            // Leave it out when unreadable so desktops keep their current icon.
            icon: server_icon.unwrap_or(Shared::Unsupported),
        }),
        default_provider_id,
        defaults: defaults.ok().flatten(),
        onboarding_required: onboarding_required.unwrap_or(false),
        providers,
        limits: Limits {
            max_text_bytes: crate::app::messages::MAX_TEXT_BYTES,
            max_bot_name_chars: crate::app::bots::MAX_NAME_CHARS,
        },
    }))
}

/// A provider as capabilities list it, given the providers with a saved
/// profile.
fn provider_info(
    provider: &dyn crate::app::providers::Provider,
    configured: Option<&[String]>,
) -> ProviderInfo {
    ProviderInfo {
        state: provider_view(provider, configured, false),
        catalog: provider.catalog().as_ref().clone(),
    }
}

/// Check one provider's CLI again, start listing its models if it can
/// run, and return it as capabilities list it.
pub(super) async fn recheck_provider(
    state: &AppState,
    provider: &Arc<dyn crate::app::providers::Provider>,
) -> ProviderInfo {
    let recheck = provider.reprobe();
    let _ = tokio::time::timeout(crate::providers::RECHECK_WAIT, recheck).await;
    if provider.availability() == Availability::Available {
        let (provider, cache) = (provider.clone(), state.model_cache.clone());
        tokio::spawn(async move {
            crate::providers::models::refresh(std::slice::from_ref(&provider), &cache).await;
        });
    }
    let configured = configured_profiles(&state.db).await;
    provider_info(provider.as_ref(), configured.as_deref())
}

/// Provider IDs with a saved execution profile; `None` when they cannot be
/// read in time.
async fn configured_profiles(db: &SqlitePool) -> Option<Vec<String>> {
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        sqlx::query_scalar("SELECT provider_id FROM provider_profiles").fetch_all(db),
    )
    .await
    .ok()
    .and_then(Result::ok)
}

/// Per provider: a saved default, and how many bots and routines carry
/// their own model (paused routines too, since run now still runs them).
/// `None` when they cannot be read in time.
async fn saved_models(db: &SqlitePool) -> Option<Vec<(String, bool, i64, i64)>> {
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        sqlx::query_as(
            "SELECT provider_id, MAX(is_default), SUM(bot), SUM(routine) FROM (
                 SELECT provider_id, 1 AS is_default, 0 AS bot, 0 AS routine
                 FROM provider_profiles
                 UNION ALL
                 SELECT p.provider_id, 0, 1, 0 FROM bot_profiles p
                 JOIN bots b ON b.id = p.bot_id WHERE b.status = 'active'
                 UNION ALL
                 SELECT json_extract(r.model, '$.provider_id'), 0, 0, 1 FROM routines r
                 JOIN bots b ON b.id = r.bot_id
                 WHERE r.model IS NOT NULL AND r.deleted_at IS NULL
                     AND b.status = 'active'
             ) GROUP BY provider_id",
        )
        .fetch_all(db),
    )
    .await
    .ok()
    .and_then(Result::ok)
}

/// A provider's state. Without a saved default, `has_own_models` (a bot or
/// routine naming its own model) still makes it available.
fn provider_view(
    provider: &dyn crate::app::providers::Provider,
    configured: Option<&[String]>,
    has_own_models: bool,
) -> ProviderState {
    let has_profile = configured.map(|ids| ids.iter().any(|id| id == provider.id()));
    let cli = provider.availability();
    let reason = match &cli {
        Availability::Unavailable(reason) => Some(reason.clone()),
        Availability::Available
            if provider.uses_profile() && has_profile == Some(false) && !has_own_models =>
        {
            Some(format!(
                "No saved {} execution profile. Save an explicit model first \
                 (PATCH /v1/settings/providers/{}).",
                provider.label(),
                provider.id()
            ))
        }
        Availability::Available => None,
    };
    ProviderState {
        id: provider.id().to_owned(),
        label: provider.label().to_owned(),
        available: reason.is_none(),
        unavailable_reason: reason,
        cli_available: cli == Availability::Available,
        cli_unavailable_reason: match cli {
            Availability::Unavailable(reason) => Some(reason),
            Availability::Available => None,
        },
        requires_profile: provider.uses_profile(),
        profile_configured: has_profile,
    }
}

fn status_schema() -> serde_json::Value {
    json!({
        "description": "This else.bot installation's version and the state of each part \
            the owner sees: every provider CLI (available, or the exact reason it is not, \
            such as a path that fails or a missing login), whether it has a saved model \
            profile, bot computers, and backups. Call it before reporting that a provider, \
            model, computer, or backup is missing or broken, and quote its reasons.",
        "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        "annotations": tool_defs::read_only(),
    })
}

/// What a bot reports when else.bot itself seems broken: the version and
/// each part's state with the reason it gives.
pub(crate) async fn status(state: &AppState) -> serde_json::Value {
    let (saved, backups) = tokio::join!(saved_models(&state.db), async {
        let backups = state.backups.as_ref().ok()?;
        let read = tokio::time::timeout(std::time::Duration::from_secs(2), backups.snapshot());
        Some(
            read.await
                .map_err(|_| "backup status did not answer in 2 seconds".to_owned()),
        )
    });
    let defaults: Option<Vec<String>> = saved.as_ref().map(|rows| {
        rows.iter()
            .filter(|row| row.1)
            .map(|row| row.0.clone())
            .collect()
    });
    let providers: Vec<_> = state
        .registry
        .all()
        .iter()
        .map(|provider| {
            let id = provider.id();
            let row = saved
                .as_ref()
                .and_then(|rows| rows.iter().find(|row| row.0 == id));
            let (bots, routines) = row.map_or((0, 0), |row| (row.2, row.3));
            let mut view = json!(provider_view(
                provider.as_ref(),
                defaults.as_deref(),
                bots + routines > 0
            ));
            if saved.is_some() {
                view["bots_with_own_model"] = json!(bots);
                view["routines_with_own_model"] = json!(routines);
            }
            view
        })
        .collect();
    let backups = match backups {
        None => json!({ "available": false, "unavailable_reason": state.backups.as_ref().err() }),
        Some(Err(why)) => json!({ "available": false, "error": why }),
        Some(Ok(Err(err))) => json!({ "available": false, "error": err.to_string() }),
        Some(Ok(Ok(snapshot))) => json!({
            "available": snapshot.unavailable_reason.is_none(),
            "unavailable_reason": snapshot.unavailable_reason,
            "enabled": snapshot.enabled,
            "running": snapshot.running,
            "next_backup_at": snapshot.next_backup_at,
            "last_error": snapshot.last_error,
            "latest": snapshot.recent.first(),
        }),
    };
    json!({
        "version": crate::VERSION,
        "providers": providers,
        "profiles_readable": saved.is_some(),
        "computers": {
            "available": state.computers.manager().is_some(),
            "unavailable_reason": state.computers.unavailable_reason(),
        },
        "backups": backups,
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    pub const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    async fn app() -> Router {
        let home = tempfile::tempdir().unwrap();
        let (config, state, background) = crate::runtime::start_in(home.path()).await;
        // The router owns the API's files and loops, so they end with it.
        router(&config, state).layer(axum::Extension(Arc::new((home, background))))
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
            app().await,
            get("/healthz")
                .header(header::CONTENT_LENGTH, "70000")
                .body(Body::from(vec![b'x'; 70000]))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body["error"]["code"], "too_large");
        let (status, body) = send(
            app().await,
            Request::post("/healthz")
                .header(header::HOST, "127.0.0.1")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(body["error"]["code"], "method_not_allowed");
        let (status, _) = send(
            app().await,
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
        let (status, _) = send(app().await, get("/healthz").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_route_uses_error_shape() {
        let (status, body) = send(app().await, get("/nope").body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "not_found");
        assert_eq!(body["error"]["retryable"], false);
    }

    #[tokio::test]
    async fn protected_route_requires_exactly_one_valid_bearer() {
        let app = app().await;
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
            let (status, _) = send(app().await, get(path).body(Body::empty()).unwrap()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
        }
        let (status, _) = send(
            app().await,
            get("/v1/bots/unknown").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = send(
            app().await,
            get("/v1/bots/unknown")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn tailscale_hosts_require_the_matching_listener_and_reject_browsers() {
        let ip = "100.102.228.67".parse().unwrap();
        for (host, browser, expected) in [
            ("100.102.228.67:7878", false, StatusCode::OK),
            ("100.102.228.68:7878", false, StatusCode::BAD_REQUEST),
            ("100.102.228.67:7878", true, StatusCode::FORBIDDEN),
        ] {
            let mut request = Request::get("/healthz").header(header::HOST, host);
            if browser {
                request = request.header(header::ORIGIN, "http://evil.example");
            }
            let (status, _) = send(
                app().await.layer(axum::Extension(auth::ListenerIp(ip))),
                request.body(Body::empty()).unwrap(),
            )
            .await;
            assert_eq!(status, expected, "{host}");
        }
        let (status, _) = send(
            app().await,
            Request::get("/healthz")
                .header(header::HOST, "100.102.228.67:7878")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn browser_origin_and_foreign_host_are_rejected() {
        let (status, _) = send(
            app().await,
            get("/v1/capabilities")
                .header(header::ORIGIN, "http://evil.example")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = send(
            app().await,
            Request::get("/healthz")
                .header(header::HOST, "evil.example:7878")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = send(
            app().await,
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
        let app = app().await;
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
