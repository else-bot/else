use super::owner_tools::{Card, Declined, ask};
use super::tool_defs::Call;
use crate::app::tools::ToolId;
use crate::http::tool_result::{self, Done, Refusal, ToolResult};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{delete, get, patch, post, put},
};
use else_wire::email;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

use super::{
    AppState, Routes,
    error::{ApiError, ApiJson, ApiPath},
};
use crate::{
    connectors::{ConnectorManager, oauth, store, types::*},
    grants::Grant,
};

mod disconnect;
mod schemas;

pub(super) use schemas::{
    accounts_schema, action_schema, action_status_schema, connect_schema, disconnect_schema,
    trigger_delete_schema, trigger_set_schema,
};

pub fn routes() -> Routes {
    Routes {
        open: Router::new()
            .route("/connectors", get(definitions))
            .route("/connectors/email/accounts", get(accounts))
            .route("/connectors/email/accounts/{account_id}", get(account))
            .route(
                "/connectors/email/accounts/{account_id}/messages",
                get(view_message),
            ),
        gated: Router::new()
            .route("/connectors/email/accounts", post(connect))
            .route(
                "/connectors/email/accounts/{account_id}",
                patch(activate).delete(disconnect),
            )
            .route(
                "/connectors/email/accounts/{account_id}/credentials",
                put(credentials),
            )
            .route(
                "/connectors/email/accounts/{account_id}/config",
                put(configure_folders),
            )
            .route(
                "/connectors/email/accounts/{account_id}/bots/{bot_id}",
                put(permissions).delete(revoke),
            )
            .route(
                "/connectors/email/accounts/{account_id}/bots/{bot_id}/wake",
                put(wake),
            )
            .route(
                "/connectors/email/accounts/{account_id}/bots/{bot_id}/trigger",
                axum::routing::delete(delete_trigger),
            )
            .route(
                "/connectors/email/accounts/{account_id}/introduction",
                post(introduction),
            )
            .route("/connectors/email/oauth/start", post(oauth_start))
            .route("/connectors/email/oauth/complete", post(oauth_complete))
            .route("/connectors/email/oauth/{request_id}", delete(oauth_cancel)),
    }
}

fn unavailable(error: store::StoreError) -> ApiError {
    match error {
        store::StoreError::NotFound => ApiError::not_found(),
        store::StoreError::AddressTaken => ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "mail_account_exists",
            "This email address is already connected.",
        ),
        store::StoreError::Forbidden
        | store::StoreError::NotRunning
        | store::StoreError::Inactive => ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "mail_forbidden",
            "This bot or account cannot perform that operation.",
        ),
        store::StoreError::Invalid(message) => {
            ApiError::bad_request("invalid_mail_request", message)
        }
        _ => ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "connector_conflict",
            "The connector operation could not be completed. Check the account state and request identity.",
        ),
    }
}
impl From<MailError> for ApiError {
    fn from(error: MailError) -> Self {
        let code = error.code.as_str();
        let status = error.code.http_status();
        let message = error.code.http_message();
        if matches!(
            status,
            StatusCode::SERVICE_UNAVAILABLE | StatusCode::TOO_MANY_REQUESTS
        ) {
            ApiError::new(status, code, code).retryable()
        } else {
            ApiError::new(status, code, message.or(error.detail).unwrap_or(code))
        }
    }
}

async fn definitions() -> Json<email::Definitions> {
    use email::Authentication::{AppPassword, OAuth2};

    let providers = [
        Provider::Gmail,
        Provider::Microsoft,
        Provider::Yahoo,
        Provider::Icloud,
        Provider::Imap,
    ]
    .into_iter()
    .map(|provider| email::EmailPreset {
        provider: provider.into(),
        preset: provider
            .config("you@example.com".into())
            .ok()
            .as_ref()
            .map(Into::into),
        authentication: match provider {
            Provider::Gmail | Provider::Microsoft => vec![OAuth2],
            Provider::Yahoo => vec![AppPassword, OAuth2],
            Provider::Icloud | Provider::Imap => vec![AppPassword],
        },
        oauth_wizard: matches!(provider, Provider::Gmail | Provider::Microsoft),
    })
    .collect();
    Json(email::Definitions {
        connectors: vec![email::Definition {
            id: "email".into(),
            version: 1,
            providers,
            credentials: "owner_only_write_only".into(),
            notifications: "imap_idle".into(),
            permissions: ["read", "write", "send", "wake"].map(String::from).into(),
        }],
    })
}

async fn accounts(State(state): State<Arc<AppState>>) -> Result<Json<email::Accounts>, ApiError> {
    let accounts = store::accounts(&state.db).await.map_err(unavailable)?;
    Ok(Json(email::Accounts {
        accounts: accounts.iter().map(Into::into).collect(),
    }))
}
/// One message for the owner's reader. It goes to the mail server with the
/// saved credential, never through a bot, and leaves the message unread.
async fn view_message(
    State(state): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
    query: Result<axum::extract::Query<MessageRef>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<email::EmailMessage>, ApiError> {
    let axum::extract::Query(message) = query
        .map_err(|_| ApiError::bad_request("invalid_query", "Use folder, uid_validity and uid."))?;
    let manager = &state.connectors;
    let view =
        manager
            .view(id, message)
            .await
            .map_err(|error| match error.code.reader_response() {
                Some((status, message)) => ApiError::new(status, error.code.as_str(), message),
                None => ApiError::from(error),
            })?;
    Ok(Json(view))
}
async fn account(
    State(state): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<email::AccountDetail>, ApiError> {
    let account = store::account(&state.db, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(ApiError::not_found)?;
    let grants = store::grants(&state.db, id)
        .await
        .map_err(unavailable)?
        .into_iter()
        .map(|(bot_id, permissions)| email::Grant {
            bot_id,
            permissions: (&permissions).into(),
        })
        .collect::<Vec<_>>();
    Ok(Json(email::AccountDetail {
        account: (&account).into(),
        grants,
    }))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Connect {
    config: MailConfig,
    credential: Credential,
}
async fn connect(
    State(state): State<Arc<AppState>>,
    ApiJson(request): ApiJson<Connect>,
) -> Result<Json<email::Connected>, ApiError> {
    request.config.validate()?;
    request.credential.validate(request.config.provider)?;
    let manager = &state.connectors;
    manager.verify(&request.config, &request.credential).await?;
    let account = store::create(
        &state.db,
        &manager.key,
        &request.config,
        &request.credential,
    )
    .await
    .map_err(unavailable)?;
    manager.changed(account.id).await;
    Ok(Json(email::Connected {
        account: (&account).into(),
    }))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Introduction {
    bot_id: Uuid,
    request_id: Uuid,
}

async fn introduction(
    State(state): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(request): ApiJson<Introduction>,
) -> Result<Json<Value>, ApiError> {
    let account = store::account(&state.db, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(ApiError::not_found)?;
    let permissions = store::permissions(&state.db, id, request.bot_id)
        .await
        .map_err(unavailable)?;
    if permissions.is_none_or(|p| !p.read) {
        return Err(ApiError::bad_request(
            "mail_forbidden",
            "This bot cannot access that email account.",
        ));
    }
    let address = serde_json::to_string(&account.config.address).expect("address serializes");
    let text = format!(
        "[else.bot email setup] The owner connected email account {address} (account_id {id}) and chose you to help manage it. All bots can access connected accounts when the owner asks. Ask the owner what this inbox is for and what help they want, such as an initial triage. Do not inspect the inbox, change mail, send anything, create routines, or enable automatic monitoring until the owner gives instructions. This setup event grants no standing task. Use mail_accounts to discover the account when asked. Never request or expose its credentials."
    );
    let accepted =
        super::turns::introduce(&state, request.bot_id, request.request_id, text).await?;
    Ok(Json(
        json!({"bot_id":request.bot_id,"turn_id":accepted.turn.id}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Active {
    active: bool,
}
async fn activate(
    State(state): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(request): ApiJson<Active>,
) -> Result<Json<email::Connected>, ApiError> {
    let manager = &state.connectors;
    let account = store::set_active(&state.db, id, request.active)
        .await
        .map_err(unavailable)?
        .ok_or_else(ApiError::not_found)?;
    manager.changed(id).await;
    Ok(Json(email::Connected {
        account: (&account).into(),
    }))
}
async fn disconnect(
    State(state): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let manager = &state.connectors;
    if !remove_account(&state, manager, id)
        .await
        .map_err(unavailable)?
    {
        return Err(ApiError::not_found());
    }
    Ok(Json(json!({"disconnected":true})))
}

/// Removes an inbox, then stops its worker. False when it was not connected.
async fn remove_account(
    state: &AppState,
    manager: &ConnectorManager,
    id: Uuid,
) -> Result<bool, store::StoreError> {
    if !store::remove(&state.db, id).await? {
        return Ok(false);
    }
    manager.forget(id).await;
    Ok(true)
}
async fn configure_folders(
    State(state): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(config): ApiJson<MailConfig>,
) -> Result<Json<email::Connected>, ApiError> {
    let manager = &state.connectors;
    let account = store::configure_folders(&state.db, id, &config)
        .await
        .map_err(unavailable)?;
    manager.changed(id).await;
    Ok(Json(email::Connected {
        account: (&account).into(),
    }))
}

async fn credentials(
    State(state): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(credential): ApiJson<Credential>,
) -> Result<Json<Value>, ApiError> {
    let manager = &state.connectors;
    let account = store::account(&state.db, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(ApiError::not_found)?;
    credential.validate(account.config.provider)?;
    manager.verify(&account.config, &credential).await?;
    manager.replace_credential(id, credential).await?;
    manager.changed(id).await;
    Ok(Json(json!({"updated":true})))
}
async fn permissions(
    State(state): State<Arc<AppState>>,
    ApiPath((id, bot)): ApiPath<(Uuid, Uuid)>,
    ApiJson(request): ApiJson<Permissions>,
) -> Result<Json<email::PermissionsEnvelope>, ApiError> {
    if (request.wake || request.write || request.send) && !request.read
        || request.send && !request.write
    {
        return Err(ApiError::bad_request(
            "invalid_permissions",
            "Write and wake require read; send also requires write.",
        ));
    }
    store::set_permissions(&state.db, id, bot, &request)
        .await
        .map_err(unavailable)?;
    Ok(Json(email::PermissionsEnvelope {
        permissions: email::Permissions::from(&request),
    }))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wake {
    wake: bool,
}
async fn wake(
    State(state): State<Arc<AppState>>,
    ApiPath((id, bot)): ApiPath<(Uuid, Uuid)>,
    ApiJson(request): ApiJson<Wake>,
) -> Result<Json<Value>, ApiError> {
    store::set_wake(&state.db, id, bot, request.wake)
        .await
        .map_err(unavailable)?;
    Ok(Json(json!({"wake":request.wake})))
}
async fn delete_trigger(
    State(state): State<Arc<AppState>>,
    ApiPath((id, bot)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let deleted = store::delete_trigger_by_owner(&state.db, id, bot)
        .await
        .map_err(unavailable)?;
    Ok(Json(json!({"deleted":deleted})))
}
async fn revoke(
    State(state): State<Arc<AppState>>,
    ApiPath((id, bot)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    store::set_permissions(&state.db, id, bot, &Permissions::default())
        .await
        .map_err(unavailable)?;
    Ok(Json(json!({"revoked":true})))
}
async fn oauth_start(
    State(state): State<Arc<AppState>>,
    ApiJson(request): ApiJson<oauth::Start>,
) -> Result<Json<email::EmailAuthorization>, ApiError> {
    let accounts = store::accounts(&state.db).await.map_err(unavailable)?;
    if accounts.iter().any(|a| {
        a.config.address.eq_ignore_ascii_case(&request.address) && !a.health.needs_new_credential()
    }) {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            "mail_account_exists",
            "This email address is already connected.",
        ));
    }
    // A sign-in may only renew an inbox's grant, which takes no new slot.
    // Completing one that creates an account still checks the limit.
    let may_renew = accounts.iter().any(|a| {
        a.config.provider == request.provider
            && a.health.needs_new_credential()
            && (request.address.is_empty()
                || a.config.address.eq_ignore_ascii_case(&request.address))
    });
    if accounts.len() >= store::MAX_ACCOUNTS as usize && !may_renew {
        return Err(MailError::new(MailErrorCode::ConnectorLimit).into());
    }
    Ok(Json(state.connectors.oauth.start(request).await?))
}
async fn oauth_complete(
    State(state): State<Arc<AppState>>,
    ApiJson(request): ApiJson<oauth::Complete>,
) -> Result<Json<email::Connected>, ApiError> {
    let manager = &state.connectors;
    let (provider, address, credential) = manager.oauth.complete(request).await?;
    let config = provider.config(address)?;
    config.validate()?;
    credential.validate(provider)?;
    // Signing in again to an inbox whose grant expired keeps the account,
    // so its permissions, triggers and history stay.
    let expired = store::accounts(&state.db)
        .await
        .map_err(unavailable)?
        .into_iter()
        .find(|a| {
            a.config.provider == provider
                && a.config.address.eq_ignore_ascii_case(&config.address)
                && a.health.needs_new_credential()
        });
    let id = match expired {
        Some(account) => {
            manager.replace_credential(account.id, credential).await?;
            account.id
        }
        None => {
            store::create(&state.db, &manager.key, &config, &credential)
                .await
                .map_err(unavailable)?
                .id
        }
    };
    manager.changed(id).await;
    let account = store::account(&state.db, id)
        .await
        .map_err(unavailable)?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(email::Connected {
        account: (&account).into(),
    }))
}

async fn oauth_cancel(
    State(state): State<Arc<AppState>>,
    ApiPath(request_id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    state.connectors.oauth.cancel(request_id).await;
    Ok(Json(json!({"cancelled":true})))
}

/// A draft the owner should read belongs in a draft_email card, where they can
/// edit and send it, never in the mailbox's Drafts folder.
fn saves_mailbox_draft(command: &Value) -> bool {
    match command["operation"].as_str() {
        Some("draft") => true,
        Some("reply" | "forward") => command["send"] != Value::Bool(true),
        _ => false,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionRequest {
    account_id: Uuid,
    action_id: Option<Uuid>,
    command: MailCommand,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusRequest {
    account_id: Uuid,
    action_id: Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectRequest {
    provider: Provider,
    address: String,
    purpose: Option<String>,
}

/// What a bot does once the owner signed in to the inbox it asked for.
pub const SIGNED_IN_NEXT: &str = "The owner signed in, and new mail there now wakes you with \
    default instructions. Call mail_accounts to find the inbox; Google may have connected a \
    different address than requested. To do something specific on new mail, call \
    mail_trigger_set; if the owner only wants you to read it, call mail_trigger_delete.";

/// Asks the owner, on a chat card, to sign in to an inbox. The desktop runs
/// the provider's OAuth flow and approves the card once the account exists.
async fn connect_inbox(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    use crate::app::questions::{EmailConnection, Spec};
    let request = serde_json::from_value::<ConnectRequest>(args.clone())
        .ok()
        .map(|r| ConnectRequest {
            address: r.address.trim().to_lowercase(),
            ..r
        })
        .filter(|r| {
            r.purpose
                .as_ref()
                .is_none_or(|p| p.len() <= 300 && !p.contains('\0'))
                && r.provider
                    .config(r.address.clone())
                    .and_then(|config| config.validate())
                    .is_ok()
        });
    let Some((request, sign_in)) =
        request.and_then(|r| r.provider.card_sign_in().map(|sign_in| (r, sign_in)))
    else {
        return Err(Refusal::text(
            "Use provider gmail, microsoft, or icloud with a valid inbox address. The \
                 owner adds other providers with an app password in Settings, Email.",
        ));
    };
    let accounts = match store::accounts(&state.db).await {
        Ok(accounts) => accounts,
        Err(_) => return Err(Refusal::text(MailErrorCode::MailUnavailable.as_str())),
    };
    let existing = accounts
        .iter()
        .find(|a| a.config.address.eq_ignore_ascii_case(&request.address));
    // An inbox whose saved password or OAuth grant no longer works gets
    // the card again, so the owner can enter a new password or sign in.
    let replaces = existing
        .filter(|account| account.health.needs_new_credential())
        .map(|account| account.generation);
    if let Some(account) = existing.filter(|_| replaces.is_none()) {
        return Ok(Done::json(
            json!({"status":"connected","account_id":account.id,
                "next":"Use mail_accounts and mail_action with this account. If the owner \
                    wants you to react to new mail, call mail_trigger_set."}),
        ));
    }
    let address = &request.address;
    let sign_in = match (sign_in, replaces) {
        (CardSignIn::Browser(company), None) => format!("Sign in with {company} as {address}."),
        (CardSignIn::Browser(company), Some(_)) => {
            format!("Sign in with {company} as {address} again.")
        }
        (CardSignIn::AppPassword { issuer, help }, Some(_)) => format!(
            "{issuer} rejected the saved password for {address}. Enter a new app-specific \
             password. Make one at {help}."
        ),
        (CardSignIn::AppPassword { help, .. }, None) => {
            format!("Enter an app-specific password for {address}. Make one at {help}.")
        }
    };
    let context = format!(
        "{}\n{sign_in} Every bot can use the connected inbox when you ask. Credentials stay \
         on the server. New mail there will wake this bot.",
        request
            .purpose
            .as_deref()
            .unwrap_or("Connect this inbox for the current request."),
    );
    let spec = Spec::email_connection(
        EmailConnection {
            provider: request.provider,
            address: request.address.clone(),
            replaces,
        },
        format!("Connect {}", request.address),
        context,
    );
    let card = Card::waiting(
        |question| {
            json!({"status":"pending_owner_auth","parked":true,
                "question_id":question,
                "next":"The owner sees a sign-in card. Say so briefly and end this attempt. \
                    You continue after they sign in."})
        },
        "awaiting email sign-in",
    );
    let declined =
        Declined::SafeMessage("The owner declined this connection. Continue without it.");
    ask(state, grant, &spec, card, declined).await?;
    Ok(Done::json(
        json!({"status":"connected", "next": SIGNED_IN_NEXT}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TriggerSet {
    account_id: Uuid,
    instructions: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TriggerDelete {
    account_id: Uuid,
}

/// Asks the owner to approve the exact instructions: text read from mail
/// during an owner request must not become standing instructions unseen.
async fn approve_trigger(
    state: &AppState,
    grant: &Grant,
    args: &Value,
    request: &TriggerSet,
) -> Result<Result<(), Refusal>, store::StoreError> {
    use crate::app::questions::{ApprovalKind, Spec};
    let bot_id = store::trigger_manager(&state.db, grant.turn_id).await?;
    let instructions = store::wake_instructions(&request.instructions)?;
    let readable = store::permissions(&state.db, request.account_id, bot_id)
        .await?
        .is_some_and(|p| p.read);
    if !readable {
        return Err(store::StoreError::Forbidden);
    }
    let address = store::accounts(&state.db)
        .await?
        .into_iter()
        .find(|account| account.id == request.account_id)
        .map(|account| account.config.address)
        .ok_or(store::StoreError::NotFound)?;
    let spec = Spec::tool_approval(
        ApprovalKind::External,
        ToolId::MailTriggerSet.def(),
        args,
        None,
        format!("Wake on new mail in {address}"),
        format!("New mail in {address} will wake this bot to: {instructions}"),
    );
    let card = Card::approval(
        "The owner sees an approval card for this trigger. End this attempt. If approved, call \
         mail_trigger_set with exactly the same arguments once.",
    );
    let declined = Declined::SafeMessage("The owner declined this trigger. Don't set it.");
    Ok(ask(state, grant, &spec, card, declined).await.map(drop))
}

async fn trigger_call(
    state: &AppState,
    grant: &Grant,
    op: Op,
    name: &str,
    args: &Value,
) -> ToolResult {
    let result = if matches!(op, Op::TriggerSet) {
        let request: TriggerSet = tool_result::args(args, || {
            Refusal::text("Use account_id and instructions only.")
        })?;
        match approve_trigger(state, grant, args, &request).await {
            Ok(Err(refusal)) => return Err(refusal),
            Ok(Ok(())) => store::set_trigger(
                &state.db,
                grant.turn_id,
                request.account_id,
                &request.instructions,
            )
            .await
            .map(|trigger| {
                json!({"trigger":email::MailTrigger::from(&trigger),
                    "next":"Tell the owner which inbox wakes you and what you will do. They can \
                        pause or delete it in your sidebar."})
            }),
            Err(error) => Err(error),
        }
    } else {
        let request: TriggerDelete =
            tool_result::args(args, || Refusal::text("Use account_id only."))?;
        store::delete_trigger(&state.db, grant.turn_id, request.account_id)
            .await
            .map(|deleted| json!({"deleted":deleted}))
    };
    match result {
        Ok(value) => Ok(Done::json(value).logged(format!("native mail tool {name}"))),
        Err(store::StoreError::Db(error)) => {
            tracing::warn!(%error, "a mail trigger tool failed");
            Err(Refusal::text(MailErrorCode::MailUnavailable.as_str()))
        }
        Err(store::StoreError::Forbidden) => Err(Refusal::text(
            "Only a request from the owner can change mail triggers, on an inbox you can read.",
        )),
        Err(error) => Err(Refusal::text(error.to_string())),
    }
}

/// The email tools, each bound to its `ToolId` in `tool_defs`.
#[derive(Clone, Copy)]
pub(super) enum Op {
    Accounts,
    Connect,
    Disconnect,
    Action,
    ActionStatus,
    TriggerSet,
    TriggerDelete,
}

pub(super) async fn call(c: Call<'_>, op: Op) -> ToolResult {
    let Call {
        state,
        grant,
        args,
        tool,
    } = c;
    match op {
        Op::Connect => return connect_inbox(state, grant, args).await,
        Op::Disconnect => return disconnect::disconnect_inbox(state, grant, args).await,
        Op::TriggerSet | Op::TriggerDelete => {
            return trigger_call(state, grant, op, tool.name, args).await;
        }
        Op::Action => match args["command"]["operation"].as_str() {
            Some("open_attachments") => {
                return super::mail_attachments::open(state, grant, args).await;
            }
            Some("attachment_search" | "attachment_page") => {
                return super::mail_attachments::query(state, grant, args).await;
            }
            _ => {}
        },
        Op::Accounts | Op::ActionStatus => {}
    }
    let result = tool_call(state, grant, op, args).await;
    match result {
        Ok(value) => Ok(Done::json(value).logged("Email tool completed.")),
        Err(error) if error.code == MailErrorCode::MailAuthenticationFailed => {
            let error = error.detail(
                "the server rejected the inbox's saved password. For an iCloud inbox, call \
                 mail_connect with provider icloud and its address so the owner enters a new \
                 one. Otherwise the owner replaces it in Settings, Email.",
            );
            Err(Refusal::text(error.to_string()))
        }
        Err(error) if error.code == MailErrorCode::OauthReauthenticationRequired => {
            let error = error.detail(
                "the inbox's sign-in expired. Call mail_connect with its provider and address \
                 so the owner signs in again.",
            );
            Err(Refusal::text(error.to_string()))
        }
        Err(error) => Err(Refusal::text(error.to_string())),
    }
}

async fn tool_call(
    state: &AppState,
    grant: &Grant,
    op: Op,
    args: &Value,
) -> Result<Value, MailError> {
    let manager = &state.connectors;
    if matches!(op, Op::Accounts) {
        if !args.is_null() && args.as_object().is_none_or(|object| !object.is_empty()) {
            return Err(MailError::new(MailErrorCode::InvalidMailRequest));
        }
        let (accounts, source) = tokio::join!(
            store::bot_accounts(&state.db, grant.bot_id),
            store::event_account(&state.db, grant.turn_id)
        );
        let (accounts, source) = (accounts?, source?);
        let visible = accounts.into_iter().filter_map(|(account,p)| {
            (p.read && source.is_none_or(|id| id == account.id)).then(|| json!({"id":account.id,"address":account.config.address,"provider":account.config.provider,"state":account.state,"health":account.health,"permissions":p}))
        }).collect::<Vec<_>>();
        return Ok(json!({"accounts":visible}));
    }
    let source = store::event_account(&state.db, grant.turn_id).await?;
    let in_scope = |account_id: Uuid| source.is_none_or(|id| id == account_id);
    if matches!(op, Op::ActionStatus) {
        let request: StatusRequest = serde_json::from_value(args.clone())
            .map_err(|_| MailError::new(MailErrorCode::InvalidMailRequest))?;
        if !in_scope(request.account_id) {
            return Err(MailError::new(MailErrorCode::MailForbidden));
        }
        if store::permissions(&state.db, request.account_id, grant.bot_id)
            .await?
            .is_none_or(|p| !p.read)
        {
            return Err(MailError::new(MailErrorCode::MailForbidden));
        }
        return store::ACTIONS
            .receipt(
                &state.db,
                request.account_id,
                grant.bot_id,
                request.action_id,
            )
            .await
            .map_err(store::StoreError::from)?
            .ok_or_else(|| MailError::new(MailErrorCode::MailActionNotFound));
    }
    let request: ActionRequest = serde_json::from_value(args.clone()).map_err(|_| {
        if saves_mailbox_draft(&args["command"]) {
            MailError::new(MailErrorCode::MailDraftsNeedACard).detail(
                "mail_action saves no drafts. Call draft_email with account_id and reply_to so \
                 the owner reviews it in a card.",
            )
        } else {
            MailError::new(MailErrorCode::InvalidMailRequest)
        }
    })?;
    if !in_scope(request.account_id) {
        return Err(MailError::new(MailErrorCode::MailForbidden));
    }
    if source.is_none() && request.command.sends() {
        return Err(MailError::new(MailErrorCode::MailSendNeedsApprovedDraft));
    }
    if matches!(
        request.command,
        MailCommand::OpenAttachments { .. }
            | MailCommand::FindSent { .. }
            | MailCommand::Locate { .. }
            | MailCommand::View { .. }
    ) {
        return Err(MailError::new(MailErrorCode::InvalidMailRequest));
    }
    manager
        .execute(
            request.account_id,
            grant.bot_id,
            grant.turn_id,
            request.action_id,
            request.command,
        )
        .await
}
