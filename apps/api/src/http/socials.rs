use super::{
    AppState,
    error::{ApiError, ApiJson, ApiPath, SecretJson},
};
use crate::socials::{Manager, store, types::*};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/connectors/socials/providers",
            get(|| async { Json(definitions()) }),
        )
        .route("/connectors/socials/connections", get(list).post(connect))
        .route(
            "/connectors/socials/connections/{id}",
            get(detail).patch(active).delete(remove),
        )
        .route(
            "/connectors/socials/connections/{id}/credentials",
            put(credential),
        )
        .route(
            "/connectors/socials/connections/{id}/destinations",
            get(destinations),
        )
        .route(
            "/bots/{bot}/social-triggers/{id}",
            axum::routing::patch(toggle_trigger).delete(delete_trigger),
        )
        .route("/connectors/socials/setups/{id}/complete", post(complete))
        .route(
            "/connectors/socials/setups/{id}",
            axum::routing::delete(cancel),
        )
}
pub(super) fn error(e: Error) -> ApiError {
    match e {
        Error::NotFound => ApiError::not_found(),
        Error::Forbidden => ApiError::new(StatusCode::FORBIDDEN, "social_forbidden", e.to_string()),
        Error::Inactive | Error::Conflict | Error::Duplicate => {
            ApiError::new(StatusCode::CONFLICT, "social_conflict", e.to_string())
        }
        Error::Invalid(text) => ApiError::bad_request("invalid_social_request", text),
        Error::Busy => {
            ApiError::new(StatusCode::TOO_MANY_REQUESTS, "social_busy", e.to_string()).retryable()
        }
        Error::Db(_) => ApiError::internal(),
        _ => ApiError::unavailable("social_unavailable", e.to_string()),
    }
}
pub(super) fn manager(s: &AppState) -> Result<&Arc<Manager>, ApiError> {
    s.socials.as_ref().ok_or_else(|| {
        ApiError::unavailable(
            "social_unavailable",
            "Native socials require the API installation encryption key.",
        )
    })
}
async fn list(State(s): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({"connections":store::connections(&s.db).await.map_err(error)?}),
    ))
}
async fn detail(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let connection = store::connection(&s.db, id).await.map_err(error)?;
    Ok(Json(json!({ "connection": connection })))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connect {
    provider: Provider,
    token: String,
    #[serde(default)]
    instructions: String,
}
async fn connect(
    State(s): State<Arc<AppState>>,
    SecretJson(request): SecretJson<Connect>,
) -> Result<Json<Value>, ApiError> {
    let m = manager(&s)?;
    let token = Token::parse(request.token).map_err(error)?;
    let identity = m.validate(request.provider, &token).await.map_err(error)?;
    let c = store::create(
        &s.db,
        &m.key,
        request.provider,
        &identity,
        &token,
        &request.instructions,
    )
    .await
    .map_err(error)?;
    m.changed(c.id).await;
    Ok(Json(json!({"connection":c})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Active {
    active: bool,
}
async fn active(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(request): ApiJson<Active>,
) -> Result<Json<Value>, ApiError> {
    let m = manager(&s)?;
    let c = store::set_state(&s.db, id, if request.active { "active" } else { "paused" })
        .await
        .map_err(error)?;
    m.changed(id).await;
    Ok(Json(json!({"connection":c})))
}
async fn remove(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let m = manager(&s)?;
    store::set_state(&s.db, id, "removed")
        .await
        .map_err(error)?;
    m.changed(id).await;
    Ok(Json(json!({"disconnected":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    token: String,
}
async fn credential(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
    SecretJson(request): SecretJson<Credential>,
) -> Result<Json<Value>, ApiError> {
    let m = manager(&s)?;
    let c = store::connection(&s.db, id).await.map_err(error)?;
    let token = Token::parse(request.token).map_err(error)?;
    let identity = m.validate(c.provider, &token).await.map_err(error)?;
    if identity.id != c.remote_id {
        return Err(error(Error::Invalid(
            "The replacement token belongs to a different bot. Add it as a separate connection.",
        )));
    }
    store::replace(&s.db, &m.key, &c, &token)
        .await
        .map_err(error)?;
    m.changed(id).await;
    Ok(Json(json!({"updated":true})))
}
async fn destinations(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let list = manager(&s)?.discover(id).await.map_err(error)?;
    Ok(Json(json!({"destinations":list})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Toggle {
    enabled: bool,
}
async fn toggle_trigger(
    State(s): State<Arc<AppState>>,
    ApiPath((bot, id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(input): ApiJson<Toggle>,
) -> Result<Json<Value>, ApiError> {
    super::turns::active_bot(&s, bot).await?;
    let change = store::set_trigger_enabled(&s.db, bot, id, input.enabled)
        .await
        .map_err(error)?;
    super::social_tools::reconnect(&s, &change).await;
    Ok(Json(json!({ "trigger": change.value })))
}
async fn delete_trigger(
    State(s): State<Arc<AppState>>,
    ApiPath((bot, id)): ApiPath<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    super::turns::active_bot(&s, bot).await?;
    let change = store::delete_trigger(&s.db, bot, id).await.map_err(error)?;
    super::social_tools::reconnect(&s, &change).await;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Complete {
    connection_id: Uuid,
}
async fn complete(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<Complete>,
) -> Result<Json<Value>, ApiError> {
    manager(&s)?;
    let setup = store::settle_setup(&s.db, id, Some(input.connection_id))
        .await
        .map_err(error)?;
    s.scheduler.wake();
    Ok(Json(json!({"setup":setup})))
}
async fn cancel(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let setup = store::settle_setup(&s.db, id, None).await.map_err(error)?;
    s.scheduler.wake();
    Ok(Json(json!({"setup":setup})))
}
