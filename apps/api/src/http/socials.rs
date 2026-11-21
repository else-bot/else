use super::{
    AppState, Routes,
    error::{ApiError, ApiJson, ApiPath, SecretJson},
};
use crate::socials::{pairing::Purpose, store, types::*};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, patch, post, put},
};
use else_wire::socials as wire;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

pub fn routes() -> Routes {
    Routes {
        open: Router::new()
            .route(
                "/connectors/socials/providers",
                get(|| async { Json(definitions()) }),
            )
            .route("/connectors/socials/connections", get(list))
            .route("/connectors/socials/connections/{id}", get(detail))
            .route(
                "/connectors/socials/connections/{id}/destinations",
                get(destinations),
            )
            .route(
                "/connectors/socials/setups/{id}/pairing",
                get(|s, id, after| pairing(s, id, after, Purpose::Setup)),
            )
            .route(
                "/connectors/socials/connections/{id}/pairing",
                get(|s, id, after| pairing(s, id, after, Purpose::Repair)),
            ),
        gated: Router::new()
            .route("/connectors/socials/connections", post(connect))
            .route(
                "/connectors/socials/connections/{id}",
                patch(active).delete(remove),
            )
            .route(
                "/connectors/socials/connections/{id}/credentials",
                put(credential),
            )
            .route(
                "/bots/{bot}/social-triggers/{id}",
                patch(toggle_trigger).delete(delete_trigger),
            )
            .route("/connectors/socials/setups/{id}/complete", post(complete))
            .route(
                "/connectors/socials/setups/{id}/pairing",
                post(|s, id| start_pairing(s, id, Purpose::Setup)),
            )
            .route(
                "/connectors/socials/connections/{id}/pairing",
                post(|s, id| start_pairing(s, id, Purpose::Repair)).delete(stop_repair_pairing),
            )
            .route(
                "/connectors/socials/setups/{id}",
                axum::routing::delete(cancel),
            ),
    }
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
async fn list(State(s): State<Arc<AppState>>) -> Result<Json<wire::Connections>, ApiError> {
    let connections = store::connections(&s.db).await.map_err(error)?;
    Ok(Json(wire::Connections {
        connections: connections.iter().map(Into::into).collect(),
    }))
}
async fn detail(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<wire::ConnectionEnvelope>, ApiError> {
    let connection = store::connection(&s.db, id).await.map_err(error)?;
    Ok(Json(wire::ConnectionEnvelope {
        connection: (&connection).into(),
    }))
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
) -> Result<Json<wire::ConnectionEnvelope>, ApiError> {
    let m = &s.socials;
    if request.provider.setup() != SetupKind::Token {
        return Err(error(Error::Invalid(
            "Pair this provider from its connection card instead of submitting a token.",
        )));
    }
    let token = Token::parse(request.token).map_err(error)?;
    let identity = m.validate(request.provider, &token).await.map_err(error)?;
    let c = store::create(
        &s.db,
        &m.key,
        request.provider,
        &identity,
        &Secret::Token(token),
        None,
        &request.instructions,
    )
    .await
    .map_err(error)?;
    m.changed(c.id).await;
    Ok(Json(wire::ConnectionEnvelope {
        connection: (&c).into(),
    }))
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
) -> Result<Json<wire::ConnectionEnvelope>, ApiError> {
    let m = &s.socials;
    let to = if request.active {
        ConnectionState::Active
    } else {
        ConnectionState::Paused
    };
    let c = match store::set_state(&s.db, id, to).await.map_err(error)? {
        store::StateChange::Changed(c) => {
            signal(m, id);
            c
        }
        store::StateChange::Unchanged(c) => c,
    };
    Ok(Json(wire::ConnectionEnvelope {
        connection: (&c).into(),
    }))
}
/// Tell the connection's worker about a committed change from a task of its
/// own, so a dropped request cannot lose the signal that stops in-flight work.
fn signal(m: &Arc<crate::socials::Manager>, id: Uuid) {
    let m = m.clone();
    tokio::spawn(async move { m.changed(id).await });
}
async fn remove(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let m = &s.socials;
    store::set_state(&s.db, id, ConnectionState::Removed)
        .await
        .map_err(error)?;
    signal(m, id);
    Ok(Json(json!({"disconnected":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    token: String,
}
async fn credential(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
    SecretJson(request): SecretJson<Replacement>,
) -> Result<Json<Value>, ApiError> {
    let m = &s.socials;
    let c = store::connection(&s.db, id).await.map_err(error)?;
    if c.provider.setup() != SetupKind::Token {
        return Err(error(Error::Invalid(
            "Pair this connection again from Settings instead of submitting a token.",
        )));
    }
    let token = Token::parse(request.token).map_err(error)?;
    let identity = m.validate(c.provider, &token).await.map_err(error)?;
    if identity.id != c.remote_id {
        return Err(error(Error::Invalid(
            "The replacement token belongs to a different bot. Add it as a separate connection.",
        )));
    }
    store::replace(&s.db, &m.key, &c, &Secret::Token(token))
        .await
        .map_err(error)?;
    signal(m, id);
    Ok(Json(json!({"updated":true})))
}
async fn destinations(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let list = s.socials.discover(id).await.map_err(error)?;
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
) -> Result<Json<wire::TriggerEnvelope>, ApiError> {
    super::turns::active_bot(&s, bot).await?;
    let change = store::set_trigger_enabled(&s.db, bot, id, input.enabled)
        .await
        .map_err(error)?;
    super::social_tools::reconnect(&s, &change).await;
    Ok(Json(wire::TriggerEnvelope {
        trigger: (&change.value).into(),
    }))
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
    /// The owner's words for the bot, written on the card.
    #[serde(default)]
    note: Option<String>,
}
async fn complete(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(input): ApiJson<Complete>,
) -> Result<Json<wire::SetupEnvelope>, ApiError> {
    let m = &s.socials;
    let setup = store::settle_setup(&s.db, id, Some(input.connection_id), input.note.as_deref())
        .await
        .map_err(error)?;
    m.stop_pairing(Purpose::Setup(id)).await;
    s.scheduler.wake();
    Ok(Json(wire::SetupEnvelope {
        setup: (&setup).into(),
    }))
}
async fn cancel(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<wire::SetupEnvelope>, ApiError> {
    let setup = store::settle_setup(&s.db, id, None, None)
        .await
        .map_err(error)?;
    s.socials.stop_pairing(Purpose::Setup(id)).await;
    s.scheduler.wake();
    Ok(Json(wire::SetupEnvelope {
        setup: (&setup).into(),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct After {
    #[serde(default)]
    after: u64,
}
/// Start pairing for the setup or connection at the path, as `purpose`
/// names it.
async fn start_pairing(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
    purpose: fn(Uuid) -> Purpose,
) -> Result<Json<wire::PairingEnvelope>, ApiError> {
    let view = s.socials.start_pairing(purpose(id)).await.map_err(error)?;
    Ok(Json(wire::PairingEnvelope { pairing: view }))
}
async fn pairing(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
    Query(after): Query<After>,
    purpose: fn(Uuid) -> Purpose,
) -> Result<Json<wire::PairingEnvelope>, ApiError> {
    let view = s
        .socials
        .pairing(purpose(id), after.after)
        .await
        .map_err(error)?;
    Ok(Json(wire::PairingEnvelope { pairing: view }))
}
async fn stop_repair_pairing(
    State(s): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    s.socials.stop_pairing(Purpose::Repair(id)).await;
    Ok(StatusCode::NO_CONTENT)
}
