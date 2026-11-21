//! HTTP client for the else.bot API.
//!
//! Requests run on a private Tokio runtime, away from GPUI's render path.
//! Redirects are never followed, so the bearer token can only reach the one
//! configured origin; it is attached as a sensitive header, never a URL.
//! HTTPS and WSS share one TLS configuration that verifies certificates
//! with the platform's trust store.

use std::sync::Arc;
use std::time::Duration;

use else_wire::notices::{NoticeResponse, OwnerNotice};
use else_wire::outgoing::{DraftResponse, OutgoingDraft};
use else_wire::questions::QuestionAnswer;
use else_wire::routines::{Routine, RoutineEnvelope};
use else_wire::rules::RulesFile;
use else_wire::settings::{
    BotProfile, BotProfileEnvelope, Capabilities, ErrorBody, Onboarded, ProfileChoice,
    ServerEnvelope, Shared, SharedIconBody, Stored,
};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use uuid::Uuid;

mod attachments;
mod backups;
pub use backups::*;
mod computers;
mod usage;
mod vault;
pub use vault::*;
mod email;
mod live;
mod mcp;
pub use live::{Live, LiveCommand, LiveCommands, LiveEvent, StreamMessage};
mod desktop;
mod skills;
pub mod socials;
#[cfg(target_os = "macos")]
pub use desktop::MAX_TEXT_CHARS;
pub use desktop::{DesktopCommand, DesktopCommands, DesktopEvent, MAX_CLIPBOARD_BYTES};
use else_wire::bots::{
    Bot, BotEnvelope, BotList, Category, CategoryEnvelope, CategoryList, ReplyPosition,
};
use serde_json::json;

use crate::config::{Origin, OwnerToken};
use else_wire::bots::Avatar;
use else_wire::conversation::Sent;
use else_wire::conversation::Snapshot;
use else_wire::items::Reaction;

pub struct SendRequest {
    pub request_id: Uuid,
    pub text: String,
    pub provider_id: Option<String>,
    pub reaction: Option<Reaction>,
    pub parts: Vec<else_wire::items::MessagePart>,
    pub open_draft: Option<crate::model::outgoing::DraftAttachment>,
    /// The owner ask this message replies to.
    pub answers_notice: Option<Uuid>,
}

/// What the owner does with a bot's request to take over its computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffAction {
    Take,
    HandBack,
    Dismiss,
}

/// The complete create payload, retained unchanged when delivery is uncertain.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CreateBotRequest {
    pub request_id: Uuid,
    pub name: String,
    pub profile: Option<ProfileChoice>,
    pub category_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<Avatar>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReadCommand {
    Through { position: ReplyPosition },
    MarkRead { position: Option<ReplyPosition> },
    MarkUnread,
}

/// A bot's newest conversation page and the response it was read from.
pub struct Latest {
    pub snapshot: Snapshot,
    pub raw: String,
}

/// Errors shown to the owner. None of them contain the token.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ClientError {
    #[error("The API rejected the owner token. Use Change server to update it.")]
    Unauthorized,
    #[error("Cannot reach the API at {origin} ({cause}). Is `else-api serve` running?")]
    Unreachable { origin: String, cause: String },
    #[error("{0}")]
    Transport(String),
    #[error("{message}")]
    Api {
        status: u16,
        code: String,
        message: String,
        retryable: bool,
    },
    #[error("Unexpected response from the API: {0}")]
    Protocol(String),
}

impl ClientError {
    fn from_http(status: StatusCode, bytes: &[u8]) -> Self {
        let fallback = || format!("HTTP {status}");
        match serde_json::from_slice::<ErrorBody>(bytes) {
            Ok(body) => Self::Api {
                status: status.as_u16(),
                code: body.error.code,
                message: body.error.message,
                retryable: body.error.retryable,
            },
            Err(_) => Self::Protocol(fallback()),
        }
    }

    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => Some(code),
            _ => None,
        }
    }

    pub fn is_request_conflict(&self) -> bool {
        matches!(self, Self::Api { code, .. } if code == "request_conflict")
    }

    /// Whether the server never handled the request: it refused the token,
    /// or the transport check failed before anything was sent.
    pub fn never_handled(&self) -> bool {
        matches!(self, Self::Unauthorized | Self::Transport(_))
    }

    /// Whether the outcome of a mutation is unknown (the request may or may
    /// not have been applied), as opposed to definitely rejected.
    pub fn outcome_unknown(&self) -> bool {
        match self {
            Self::Unreachable { .. } | Self::Protocol(_) => true,
            Self::Api { code, .. }
                if code == "provider_unavailable" || code == "secrets_unavailable" =>
            {
                false
            }
            Self::Api { status, .. } => *status >= 500,
            Self::Unauthorized | Self::Transport(_) => false,
        }
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    tls: Arc<rustls::ClientConfig>,
    origin: Origin,
    auth: HeaderValue,
    runtime: tokio::runtime::Handle,
}

impl Client {
    pub fn new(
        origin: Origin,
        token: &OwnerToken,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, String> {
        use rustls_platform_verifier::BuilderVerifierExt as _;
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .and_then(|builder| builder.with_platform_verifier())
            .map(|builder| builder.with_no_client_auth())
            .map_err(|err| format!("Cannot set up TLS: {err}"))?;
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls.clone())
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|err| format!("Cannot set up the HTTP client: {err}"))?;
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
            .expect("token is a valid header value");
        auth.set_sensitive(true);
        Ok(Self {
            http,
            tls: Arc::new(tls),
            origin,
            auth,
            runtime,
        })
    }

    /// Run a future on the client's runtime and await it from any executor.
    pub async fn run<T: Send + 'static>(
        &self,
        fut: impl std::future::Future<Output = T> + Send + 'static,
    ) -> T {
        self.runtime.spawn(fut).await.expect("API task panicked")
    }

    pub(crate) fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.origin.as_str()))
            .timeout(Duration::from_secs(15))
            .header(AUTHORIZATION, self.auth.clone())
    }

    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    async fn verify_transport(&self) -> Result<(), ClientError> {
        if let Some(ip) = self.origin.tailscale_ip() {
            let status = else_tailnet::Status::read()
                .await
                .map_err(ClientError::Transport)?;
            if !status.contains(ip) {
                return Err(ClientError::Transport(
                    "This IP is not a device on your connected Tailscale network. Check the server's Tailscale IP or use HTTPS.".into(),
                ));
            }
        }
        Ok(())
    }

    async fn send_request(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, ClientError> {
        self.verify_transport().await?;
        request.send().await.map_err(|err| {
            if err.is_connect() || err.is_timeout() || err.is_request() {
                ClientError::Unreachable {
                    origin: self.origin.as_str().to_owned(),
                    cause: root_cause(&err),
                }
            } else {
                ClientError::Protocol(err.without_url().to_string())
            }
        })
    }

    pub(crate) async fn send_json<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<T, ClientError> {
        let response = self.send_request(request).await?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(ClientError::Unauthorized);
        }
        if status.is_redirection() {
            return Err(ClientError::Protocol(format!(
                "redirect ({status}) refused; the token is never sent to another origin"
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|err| ClientError::Protocol(err.without_url().to_string()))?;
        if !status.is_success() {
            return Err(ClientError::from_http(status, &bytes));
        }
        let body = if status == StatusCode::NO_CONTENT && bytes.is_empty() {
            b"null".as_slice()
        } else {
            bytes.as_ref()
        };
        serde_json::from_slice(body).map_err(|err| ClientError::Protocol(err.to_string()))
    }

    pub async fn capabilities(self: Arc<Self>) -> Result<Capabilities, ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this.request(Method::GET, "/v1/capabilities");
            this.send_json(request).await
        })
        .await
    }

    /// Name the server for every desktop, or only if nobody named it yet;
    /// returns the name the server keeps.
    pub async fn name_server(
        self: Arc<Self>,
        name: String,
        only_if_unnamed: bool,
    ) -> Result<Option<String>, ClientError> {
        let body = json!({ "name": name, "only_if_unnamed": only_if_unnamed });
        self.call::<ServerEnvelope>(Method::PUT, "/v1/settings/server".into(), Some(body))
            .await
            .map(|v| v.server.name)
    }

    /// Give the server `icon` for every desktop, or only if nobody chose
    /// one yet.
    pub async fn set_server_icon(
        self: Arc<Self>,
        icon: crate::server_icon::ServerIcon,
        only_if_unset: bool,
    ) -> Result<(), ClientError> {
        use base64::Engine as _;
        let picture = match &icon {
            crate::server_icon::ServerIcon::Picture(image) => {
                let png = crate::server_icon::shareable_png(&image.bytes);
                Some(base64::engine::general_purpose::STANDARD.encode(png))
            }
            _ => None,
        };
        let body = json!({
            "icon": Stored::from(&icon),
            "picture": picture,
            "only_if_unset": only_if_unset,
        });
        self.call_unit(Method::PUT, "/v1/settings/server/icon".into(), Some(body))
            .await
    }

    /// The server's picture when its icon is still the one with `digest`.
    pub async fn server_picture(
        self: Arc<Self>,
        digest: String,
    ) -> Result<Option<Vec<u8>>, ClientError> {
        use base64::Engine as _;
        let body = self
            .call::<SharedIconBody>(Method::GET, "/v1/settings/server/icon".into(), None)
            .await?;
        let current = matches!(
            body.icon,
            Shared::Set(Stored::Picture { digest: Some(d) }) if d == digest
        );
        let png = body
            .picture
            .filter(|_| current)
            .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
            .filter(|png| crate::server_icon::digest(png) == digest);
        Ok(png)
    }

    async fn call<T: DeserializeOwned + Send + 'static>(
        self: Arc<Self>,
        method: Method,
        path: String,
        body: Option<serde_json::Value>,
    ) -> Result<T, ClientError> {
        let this = self.clone();
        self.run(async move {
            let mut request = this.request(method, &path);
            if let Some(body) = body {
                request = request.json(&body);
            }
            this.send_json(request).await
        })
        .await
    }

    /// A request whose response body nobody reads: an empty 204 or any JSON.
    async fn call_unit(
        self: Arc<Self>,
        method: Method,
        path: String,
        body: Option<serde_json::Value>,
    ) -> Result<(), ClientError> {
        self.call::<IgnoredAny>(method, path, body).await.map(drop)
    }

    pub async fn snapshot(
        self: Arc<Self>,
        bot: Uuid,
        before_seq: Option<i64>,
    ) -> Result<Snapshot, ClientError> {
        let path = match before_seq {
            Some(seq) => format!("/v1/bots/{bot}/conversation?limit=50&before_seq={seq}"),
            None => format!("/v1/bots/{bot}/conversation?limit=50"),
        };
        self.call(Method::GET, path, None).await
    }

    /// The newest page of `bot`'s conversation, with the response as sent
    /// so it can be saved and read back.
    pub async fn latest(self: Arc<Self>, bot: Uuid) -> Result<Latest, ClientError> {
        let path = format!("/v1/bots/{bot}/conversation?limit=50");
        let value: serde_json::Value = self.call(Method::GET, path, None).await?;
        let snapshot =
            Snapshot::deserialize(&value).map_err(|err| ClientError::Protocol(err.to_string()))?;
        Ok(Latest {
            snapshot,
            raw: value.to_string(),
        })
    }

    /// The owner messages and bot replies of `bot` that show `query`.
    pub async fn search(
        self: Arc<Self>,
        bot: Uuid,
        query: String,
    ) -> Result<else_wire::conversation::SearchHits, ClientError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("q", &query)
            .finish();
        self.call(
            Method::GET,
            format!("/v1/bots/{bot}/conversation/search?{query}"),
            None,
        )
        .await
    }

    /// Up to `older` shown items at or before `anchor_seq` and `newer` after
    /// it, strictly between the loaded items `below` and `above`.
    pub async fn window(
        self: Arc<Self>,
        bot: Uuid,
        anchor_seq: i64,
        (older, newer): (i64, i64),
        (below, above): (Option<i64>, Option<i64>),
    ) -> Result<else_wire::conversation::Window, ClientError> {
        let mut query = format!("anchor_seq={anchor_seq}&older={older}&newer={newer}");
        if let Some(below) = below {
            query.push_str(&format!("&after_seq={below}"));
        }
        if let Some(above) = above {
            query.push_str(&format!("&before_seq={above}"));
        }
        self.call(
            Method::GET,
            format!("/v1/bots/{bot}/conversation/window?{query}"),
            None,
        )
        .await
    }

    pub async fn send(
        self: Arc<Self>,
        bot: Uuid,
        SendRequest {
            request_id,
            text,
            provider_id,
            reaction,
            parts,
            open_draft,
            answers_notice,
        }: SendRequest,
    ) -> Result<Sent, ClientError> {
        let mut body =
            json!({ "request_id": request_id, "text": text, "provider_id": provider_id });
        if let Ok(zone) = iana_time_zone::get_timezone() {
            body["timezone"] = json!(zone);
        }
        if let Some(reaction) = reaction {
            body["reaction"] = json!(reaction);
        }
        if !parts.is_empty() {
            body["parts"] = json!(parts);
        }
        if let Some(draft) = open_draft {
            body["open_draft"] = json!(draft);
        }
        if let Some(notice) = answers_notice {
            body["answers_notice"] = json!(notice);
        }
        self.call(Method::POST, format!("/v1/bots/{bot}/turns"), Some(body))
            .await
    }

    pub async fn approve_draft(
        self: Arc<Self>,
        bot: Uuid,
        draft: Uuid,
        edit: crate::model::outgoing::DraftEdit,
        always: bool,
    ) -> Result<OutgoingDraft, ClientError> {
        let mut body = json!(edit);
        if always {
            body["always"] = json!(true);
        }
        self.draft_action(Method::POST, bot, draft, "approve", Some(body))
            .await
    }

    pub async fn stop_approving_always(
        self: Arc<Self>,
        bot: Uuid,
        draft: Uuid,
    ) -> Result<OutgoingDraft, ClientError> {
        self.draft_action(Method::DELETE, bot, draft, "always", None)
            .await
    }

    pub async fn discard_draft(
        self: Arc<Self>,
        bot: Uuid,
        draft: Uuid,
    ) -> Result<OutgoingDraft, ClientError> {
        self.draft_action(Method::POST, bot, draft, "discard", None)
            .await
    }

    async fn draft_action(
        self: Arc<Self>,
        method: Method,
        bot: Uuid,
        draft: Uuid,
        action: &str,
        body: Option<serde_json::Value>,
    ) -> Result<OutgoingDraft, ClientError> {
        self.call::<DraftResponse>(
            method,
            format!("/v1/bots/{bot}/outgoing-drafts/{draft}/{action}"),
            body,
        )
        .await
        .map(|response| response.draft)
    }

    /// Resolve a send whose response was lost. `Ok(None)` means the server
    /// has no record of it, so it was not accepted.
    pub async fn by_request(
        self: Arc<Self>,
        bot: Uuid,
        request_id: Uuid,
    ) -> Result<Option<Sent>, ClientError> {
        let path = format!("/v1/bots/{bot}/turns/by-request/{request_id}");
        match self.call(Method::GET, path, None).await {
            Ok(sent) => Ok(Some(sent)),
            Err(ClientError::Api { status: 404, .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn cancel(
        self: Arc<Self>,
        bot: Uuid,
        request_id: Uuid,
    ) -> Result<String, ClientError> {
        let body = json!({ "request_id": request_id });
        self.call::<serde_json::Value>(Method::POST, format!("/v1/bots/{bot}/cancel"), Some(body))
            .await
            .map(|v| v["state"].as_str().unwrap_or_default().to_owned())
    }

    /// Save requested and owner-added fields, or decline with `None`.
    pub async fn answer_secret(
        self: Arc<Self>,
        bot: Uuid,
        request: Uuid,
        secret: Option<SaveSecret>,
    ) -> Result<(), ClientError> {
        let (action, body) = match secret {
            Some(secret) => ("fulfill", Some(json!(secret))),
            None => ("cancel", None),
        };
        let path = format!("/v1/bots/{bot}/secret-requests/{request}/{action}");
        self.call_unit(Method::POST, path, body).await
    }

    /// Act on a bot's request to take over its computer.
    pub async fn settle_handoff(
        self: Arc<Self>,
        bot: Uuid,
        handoff: Uuid,
        action: HandoffAction,
    ) -> Result<(), ClientError> {
        let action = match action {
            HandoffAction::Take => "take",
            HandoffAction::HandBack => "hand-back",
            HandoffAction::Dismiss => "dismiss",
        };
        let path = format!("/v1/bots/{bot}/computer-handoffs/{handoff}/{action}");
        self.call_unit(Method::POST, path, None).await
    }

    /// Stop one thing a bot waits on: an MCP job, a computer job, or a
    /// task it delegated. A wait that already ended is `409 wait_ended`.
    pub async fn cancel_wait(self: Arc<Self>, bot: Uuid, wait: Uuid) -> Result<(), ClientError> {
        let path = format!("/v1/bots/{bot}/waits/{wait}/cancel");
        self.call_unit(Method::POST, path, None).await
    }

    pub async fn dismiss_owner_notice(
        self: Arc<Self>,
        bot: Uuid,
        notice: Uuid,
    ) -> Result<OwnerNotice, ClientError> {
        self.call::<NoticeResponse>(
            Method::POST,
            format!("/v1/bots/{bot}/owner-notices/{notice}/dismiss"),
            None,
        )
        .await
        .map(|envelope| envelope.notice)
    }

    /// Answer a bot's question by its selected choice ids. A stale card gets
    /// 409 `question_settled` from the server and never changes anything.
    pub async fn answer_question(
        self: Arc<Self>,
        bot: Uuid,
        question: Uuid,
        answer: QuestionAnswer,
    ) -> Result<(), ClientError> {
        let path = format!("/v1/bots/{bot}/questions/{question}/answer");
        let body = match answer {
            QuestionAnswer::Single(choice) => json!({ "choice": choice }),
            QuestionAnswer::Multiple(choices) => {
                json!({ "choices": choices })
            }
        };
        self.call_unit(Method::POST, path, Some(body)).await
    }

    pub async fn create_bot(
        self: Arc<Self>,
        request: CreateBotRequest,
    ) -> Result<Bot, ClientError> {
        self.call::<BotEnvelope>(Method::POST, "/v1/bots".into(), Some(json!(request)))
            .await
            .map(|v| v.bot)
    }

    pub async fn create_category(
        self: Arc<Self>,
        request_id: Uuid,
        name: String,
    ) -> Result<Category, ClientError> {
        let body = json!({ "request_id": request_id, "name": name });
        self.call::<CategoryEnvelope>(Method::POST, "/v1/categories".into(), Some(body))
            .await
            .map(|v| v.category)
    }

    pub async fn rename_category(
        self: Arc<Self>,
        id: Uuid,
        name: String,
    ) -> Result<Category, ClientError> {
        let body = json!({ "name": name });
        self.call::<CategoryEnvelope>(Method::PATCH, format!("/v1/categories/{id}"), Some(body))
            .await
            .map(|v| v.category)
    }

    /// Place categories in this order for every client.
    pub async fn reorder_categories(
        self: Arc<Self>,
        ids: Vec<Uuid>,
    ) -> Result<Vec<Category>, ClientError> {
        let body = json!({ "category_ids": ids });
        self.call::<CategoryList>(Method::PUT, "/v1/categories/order".into(), Some(body))
            .await
            .map(|v| v.categories)
    }

    pub async fn delete_category(self: Arc<Self>, id: Uuid) -> Result<(), ClientError> {
        self.call_unit(Method::DELETE, format!("/v1/categories/{id}"), None)
            .await
    }

    /// Assign a bot to a category, or unassign it with `None`.
    pub async fn set_category(
        self: Arc<Self>,
        bot: Uuid,
        category: Option<Uuid>,
    ) -> Result<Bot, ClientError> {
        let body = json!({ "category_id": category });
        self.call::<BotEnvelope>(
            Method::PATCH,
            format!("/v1/bots/{bot}/category"),
            Some(body),
        )
        .await
        .map(|v| v.bot)
    }

    /// Mute or unmute a bot. Repeating it is a no-op.
    pub async fn set_muted(self: Arc<Self>, bot: Uuid, muted: bool) -> Result<Bot, ClientError> {
        self.call::<BotEnvelope>(
            Method::PATCH,
            format!("/v1/bots/{bot}"),
            Some(json!({ "muted": muted })),
        )
        .await
        .map(|v| v.bot)
    }

    /// Change a bot's name, avatar, or both in one save; `None` keeps the
    /// current value. Saving values it already has is a no-op, so a retry
    /// after an unknown outcome is safe.
    pub async fn update_bot(
        self: Arc<Self>,
        bot: Uuid,
        name: Option<String>,
        avatar: Option<Avatar>,
    ) -> Result<Bot, ClientError> {
        let mut body = serde_json::Map::new();
        if let Some(name) = name {
            body.insert("name".into(), json!(name));
        }
        if let Some(avatar) = avatar {
            body.insert("avatar".into(), json!(avatar));
        }
        let body = serde_json::Value::Object(body);
        self.call::<BotEnvelope>(Method::PATCH, format!("/v1/bots/{bot}"), Some(body))
            .await
            .map(|v| v.bot)
    }

    /// The bot's effective profile; `None` before any defaults exist.
    pub async fn bot_profile(
        self: Arc<Self>,
        bot: Uuid,
    ) -> Result<Option<BotProfile>, ClientError> {
        self.call::<BotProfileEnvelope<Option<BotProfile>>>(
            Method::GET,
            format!("/v1/bots/{bot}/profile"),
            None,
        )
        .await
        .map(|v| v.profile)
    }

    /// Give the bot its own profile, used from its next message.
    pub async fn set_bot_profile(
        self: Arc<Self>,
        bot: Uuid,
        choice: ProfileChoice,
    ) -> Result<BotProfile, ClientError> {
        self.call::<BotProfileEnvelope<BotProfile>>(
            Method::PUT,
            format!("/v1/bots/{bot}/profile"),
            Some(json!(choice)),
        )
        .await
        .map(|v| v.profile)
    }

    /// Complete the welcome screen: name the main bot and set its provider
    /// and model (also the defaults). Returns the main bot's ID.
    pub async fn onboard(
        self: Arc<Self>,
        name: String,
        profile: ProfileChoice,
    ) -> Result<Uuid, ClientError> {
        let body = json!({ "name": name, "profile": profile });
        self.call::<Onboarded>(Method::POST, "/v1/onboarding".into(), Some(body))
            .await
            .map(|done| done.main_bot_id)
    }

    /// Switch a routine on or off. Setting its current value is a no-op, so
    /// a retry is safe; switching on schedules the next run from now.
    pub async fn set_routine_enabled(
        self: Arc<Self>,
        bot: Uuid,
        routine: Uuid,
        enabled: bool,
    ) -> Result<Routine, ClientError> {
        self.patch_routine(bot, routine, json!({ "enabled": enabled }))
            .await
    }

    /// Let a routine's runs end with no news, or make each run report.
    /// Setting its current value is a no-op, so a retry is safe.
    pub async fn set_routine_quiet(
        self: Arc<Self>,
        bot: Uuid,
        routine: Uuid,
        quiet: bool,
    ) -> Result<Routine, ClientError> {
        self.patch_routine(bot, routine, json!({ "quiet": quiet }))
            .await
    }

    async fn patch_routine(
        self: Arc<Self>,
        bot: Uuid,
        routine: Uuid,
        body: serde_json::Value,
    ) -> Result<Routine, ClientError> {
        self.call::<RoutineEnvelope>(
            Method::PATCH,
            format!("/v1/bots/{bot}/routines/{routine}"),
            Some(body),
        )
        .await
        .map(|v| v.routine)
    }

    /// The bot's rules file and every saved version.
    pub async fn rules(self: Arc<Self>, bot: Uuid) -> Result<RulesFile, ClientError> {
        self.call::<RulesFile>(Method::GET, format!("/v1/bots/{bot}/rules"), None)
            .await
    }

    /// Restore an earlier rules version as a new one; 0 is the empty file.
    /// Restoring the current text is a no-op. The server refuses when the
    /// rules moved past `expected`, the version the owner was looking at.
    pub async fn restore_rules(
        self: Arc<Self>,
        bot: Uuid,
        version: i64,
        expected: i64,
    ) -> Result<(), ClientError> {
        self.call_unit(
            Method::POST,
            format!("/v1/bots/{bot}/rules/restore"),
            Some(json!({ "version": version, "expected_version": expected })),
        )
        .await
    }

    /// Forget a routine's failed runs. Repeating it is a no-op.
    pub async fn dismiss_routine_failures(
        self: Arc<Self>,
        bot: Uuid,
        routine: Uuid,
    ) -> Result<Routine, ClientError> {
        self.call::<RoutineEnvelope>(
            Method::PATCH,
            format!("/v1/bots/{bot}/routines/{routine}"),
            Some(json!({ "dismiss_failures": true })),
        )
        .await
        .map(|v| v.routine)
    }

    /// Run a paused routine now; a success switches it back on. Repeating
    /// `request_id` returns the same run.
    pub async fn retry_routine(
        self: Arc<Self>,
        bot: Uuid,
        routine: Uuid,
        request_id: Uuid,
    ) -> Result<Routine, ClientError> {
        self.call::<RoutineEnvelope>(
            Method::POST,
            format!("/v1/bots/{bot}/routines/{routine}/retry"),
            Some(json!({ "request_id": request_id })),
        )
        .await
        .map(|v| v.routine)
    }

    pub async fn set_routine_model(
        self: Arc<Self>,
        bot: Uuid,
        routine: Uuid,
        model: Option<ProfileChoice>,
    ) -> Result<Routine, ClientError> {
        self.call::<RoutineEnvelope>(
            Method::PATCH,
            format!("/v1/bots/{bot}/routines/{routine}"),
            Some(json!({ "model": model })),
        )
        .await
        .map(|v| v.routine)
    }

    /// Delete a routine: it never runs again and a run already admitted
    /// finishes. Deleting it again succeeds.
    pub async fn delete_routine(
        self: Arc<Self>,
        bot: Uuid,
        routine: Uuid,
    ) -> Result<(), ClientError> {
        self.call_unit(
            Method::DELETE,
            format!("/v1/bots/{bot}/routines/{routine}"),
            None,
        )
        .await
    }

    /// Pause or resume a webhook. Pausing drops a wake it was waiting for.
    pub async fn set_webhook_enabled(
        self: Arc<Self>,
        bot: Uuid,
        webhook: Uuid,
        enabled: bool,
    ) -> Result<(), ClientError> {
        self.call_unit(
            Method::PATCH,
            format!("/v1/bots/{bot}/webhooks/{webhook}"),
            Some(json!({ "enabled": enabled })),
        )
        .await
    }

    /// Delete a webhook: its URL stops working at once. Deleting it again
    /// succeeds.
    pub async fn delete_webhook(
        self: Arc<Self>,
        bot: Uuid,
        webhook: Uuid,
    ) -> Result<(), ClientError> {
        self.call_unit(
            Method::DELETE,
            format!("/v1/bots/{bot}/webhooks/{webhook}"),
            None,
        )
        .await
    }

    pub async fn stop_watcher(
        self: Arc<Self>,
        bot: Uuid,
        watcher: Uuid,
    ) -> Result<(), ClientError> {
        self.call_unit(
            Method::POST,
            format!("/v1/bots/{bot}/watchers/{watcher}/stop"),
            None,
        )
        .await
    }

    /// Change the owner's read state. Every command is safe to repeat.
    pub async fn read(self: Arc<Self>, bot: Uuid, command: ReadCommand) -> Result<(), ClientError> {
        self.call_unit(
            Method::POST,
            format!("/v1/bots/{bot}/read"),
            Some(json!(command)),
        )
        .await
    }

    pub async fn delete_bot(self: Arc<Self>, bot: Uuid) -> Result<Bot, ClientError> {
        self.call::<BotEnvelope>(Method::DELETE, format!("/v1/bots/{bot}"), None)
            .await
            .map(|v| v.bot)
    }

    pub async fn bots(self: Arc<Self>) -> Result<BotList, ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this.request(Method::GET, "/v1/bots?include_deleting=true");
            this.send_json(request).await
        })
        .await
    }
}

fn root_cause(err: &reqwest::Error) -> String {
    let mut cause: &dyn std::error::Error = err;
    while let Some(source) = cause.source() {
        cause = source;
    }
    cause.to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::live::{route, subscriptions};
    use super::*;
    use futures_util::{SinkExt as _, StreamExt as _};
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub(crate) async fn serve_once(
        response: &'static str,
    ) -> (Origin, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (origin, task)
    }

    fn client(origin: Origin) -> Arc<Client> {
        let token = OwnerToken::parse(&"T".repeat(43)).unwrap();
        Arc::new(Client::new(origin, &token, tokio::runtime::Handle::current()).unwrap())
    }

    #[tokio::test]
    async fn watcher_stop_posts_without_a_body_and_reports_an_ended_watcher() {
        let bot = Uuid::from_u128(1);
        let watcher = Uuid::from_u128(2);
        let (origin, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 31\r\n\r\n{\"watcher\":{\"status\":\"waking\"}}",
        )
        .await;
        client(origin).stop_watcher(bot, watcher).await.unwrap();
        let request = server.await.unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("POST /v1/bots/00000000-0000-0000-0000-000000000001/watchers/00000000-0000-0000-0000-000000000002/stop HTTP/1.1\r\n"));
        assert_eq!(body, "");

        let (origin, _) = serve_once(
            "HTTP/1.1 409 Conflict\r\nContent-Length: 71\r\n\r\n{\"error\":{\"code\":\"not_active\",\"message\":\"This watcher already ended.\"}}",
        ).await;
        let error = client(origin).stop_watcher(bot, watcher).await.unwrap_err();
        assert_eq!(error.code(), Some("not_active"));
    }

    #[tokio::test]
    async fn vault_http_contract_keeps_values_in_owner_requests() {
        use axum::{
            Router,
            body::{Body, to_bytes},
            extract::State,
            http::Request,
            response::IntoResponse,
            routing::any,
        };
        use tokio::sync::mpsc;
        async fn handle(
            State(sent): State<mpsc::Sender<(String, String, serde_json::Value)>>,
            request: Request<Body>,
        ) -> axum::response::Response {
            let method = request.method().to_string();
            let path = request.uri().path().to_owned();
            let bytes = to_bytes(request.into_body(), 16384).await.unwrap();
            let body = if bytes.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            sent.send((method.clone(), path.clone(), body))
                .await
                .unwrap();
            let entry = json!({"name": "x_login", "description": "X, @owner", "website": "x.com", "env": ["PASSWORD"], "updated_at": "2026-11-17T12:00:00Z", "last_used": null});
            match (method.as_str(), path.as_str()) {
                ("GET", "/v1/task-secrets") => axum::Json(json!({"secrets": [entry]})).into_response(),
                ("POST", "/v1/task-secrets/x_login") => (StatusCode::CREATED, axum::Json(json!({"name": "x_login", "env": ["PASSWORD"], "not_redacted": ["PASSWORD"]}))).into_response(),
                ("PATCH", _) => axum::Json(entry).into_response(),
                ("POST", "/v1/task-secrets/x_login/reveal") => axum::Json(json!({"env": {"PASSWORD": "test-only-value"}})).into_response(),
                ("POST", _) => axum::Json(json!({})).into_response(),
                ("DELETE", _) => StatusCode::NO_CONTENT.into_response(),
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }
        let (sent, mut received) = mpsc::channel(8);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(any(handle)).with_state(sent),
            )
            .await
            .unwrap();
        });
        let client = client(origin);
        let entries = client.clone().vault().await.unwrap();
        assert_eq!(entries[0].description, "X, @owner");
        assert_eq!(entries[0].website.as_deref(), Some("x.com"));
        assert!(entries[0].last_used.is_none());
        assert_eq!(received.recv().await.unwrap().0, "GET");
        let secret = || SaveSecret {
            env: [
                ("PASSWORD".into(), "abc".into()),
                ("TOTP_SECRET".into(), "seed".into()),
            ]
            .into(),
            description: "X, @owner".into(),
            website: Some("x.com".into()),
        };
        let saved = client
            .clone()
            .create_secret("x_login".into(), secret())
            .await
            .unwrap();
        assert_eq!(saved.not_redacted, ["PASSWORD"]);
        let (method, path, body) = received.recv().await.unwrap();
        assert_eq!(
            (method.as_str(), path.as_str()),
            ("POST", "/v1/task-secrets/x_login")
        );
        assert_eq!(body["env"]["TOTP_SECRET"], "seed");
        client
            .clone()
            .patch_secret(
                "x_login".into(),
                PatchSecret {
                    description: "Renamed".into(),
                    website: None,
                    set: [("PASSWORD".into(), "new".into())].into(),
                    remove: vec!["USERNAME".into()],
                },
            )
            .await
            .unwrap();
        let (method, _, body) = received.recv().await.unwrap();
        assert_eq!(method, "PATCH");
        assert_eq!(body["website"], serde_json::Value::Null);
        assert_eq!(body["remove"], json!(["USERNAME"]));
        assert_eq!(body["set"]["PASSWORD"], "new");
        let revealed = client
            .clone()
            .reveal_secret("x_login".into())
            .await
            .unwrap();
        assert_eq!(revealed.env["PASSWORD"], "test-only-value");
        let (method, path, body) = received.recv().await.unwrap();
        assert_eq!(
            (method.as_str(), path.as_str()),
            ("POST", "/v1/task-secrets/x_login/reveal")
        );
        assert!(body.is_null());
        client
            .clone()
            .delete_secret("x_login".into())
            .await
            .unwrap();
        assert_eq!(received.recv().await.unwrap().0, "DELETE");
        server.abort();
    }

    #[tokio::test]
    async fn vault_request_fulfillment_includes_metadata_and_added_fields() {
        for store in [true, false] {
            let (origin, server) = serve_once(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            )
            .await;
            let secret = store.then(|| SaveSecret {
                env: [
                    ("PASSWORD".into(), "test-password".into()),
                    ("TOTP_SECRET".into(), "test-seed".into()),
                ]
                .into(),
                description: "X, @owner".into(),
                website: Some("x.com".into()),
            });
            client(origin)
                .answer_secret(Uuid::nil(), Uuid::nil(), secret)
                .await
                .unwrap();
            let request = server.await.unwrap();
            assert!(request.starts_with("POST /v1/bots/00000000-0000-0000-0000-000000000000/secret-requests/00000000-0000-0000-0000-000000000000/"));
            let (headers, body) = request.split_once("\r\n\r\n").unwrap();
            if store {
                assert!(headers.contains("/fulfill "));
                let body: serde_json::Value = serde_json::from_str(body).unwrap();
                assert_eq!(body["description"], "X, @owner");
                assert_eq!(body["website"], "x.com");
                assert_eq!(body["env"]["TOTP_SECRET"], "test-seed");
                assert_eq!(body["env"]["PASSWORD"], "test-password");
            } else {
                assert!(headers.contains("/cancel "));
                assert!(body.is_empty());
            }
        }
    }

    #[tokio::test]
    async fn approve_draft_sends_retained_files_including_when_approving_always() {
        for always in [false, true] {
            for attachments in [
                None,
                Some(vec![Uuid::from_u128(2), Uuid::from_u128(3)]),
                Some(vec![]),
            ] {
                let (origin, server) = serve_once(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
                ).await;
                let bot = Uuid::from_u128(10);
                let draft = Uuid::from_u128(11);
                let _ = client(origin)
                    .approve_draft(
                        bot,
                        draft,
                        crate::model::outgoing::DraftEdit {
                            version: 7,
                            subject: None,
                            body: "See flyer.".into(),
                            attachments: attachments.clone(),
                        },
                        always,
                    )
                    .await;
                let request = server.await.unwrap();
                assert!(request.starts_with(&format!(
                    "POST /v1/bots/{bot}/outgoing-drafts/{draft}/approve "
                )));
                let (_, body) = request.split_once("\r\n\r\n").unwrap();
                let body: serde_json::Value = serde_json::from_str(body).unwrap();
                assert_eq!(body["version"], 7);
                assert_eq!(body["body"], "See flyer.");
                assert_eq!(body.get("always"), always.then_some(&json!(true)));
                match attachments {
                    Some(ids) => assert_eq!(body["attachments"], json!(ids)),
                    None => assert!(body.get("attachments").is_none()),
                }
            }
        }
    }

    #[tokio::test]
    async fn create_sends_avatar_only_when_supported() {
        let capabilities: Capabilities = serde_json::from_value(json!({
            "providers": [], "limits": { "max_text_bytes": 1024 }
        }))
        .unwrap();
        assert!(!capabilities.bot_avatar_on_create);
        for avatar in [None, Some(Avatar::default())] {
            let (origin, server) = serve_once(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            )
            .await;
            let _ = client(origin)
                .create_bot(CreateBotRequest {
                    request_id: Uuid::new_v4(),
                    name: "Research".into(),
                    profile: None,
                    category_id: None,
                    avatar,
                })
                .await;
            let request = server.await.unwrap();
            let (_, body) = request.split_once("\r\n\r\n").unwrap();
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            if avatar.is_some() {
                assert_eq!(body["avatar"], json!({ "shape": "star", "color": "blue" }));
            } else {
                assert!(body.get("avatar").is_none());
            }
        }
    }

    #[tokio::test]
    async fn server_icons_are_unsupported_unset_or_set_and_travel_as_the_server_expects() {
        use crate::server_icon::ServerIcon;
        use else_wire::settings::ServerInfo;
        let icon =
            |server: serde_json::Value| serde_json::from_value::<ServerInfo>(server).unwrap().icon;
        assert_eq!(
            icon(json!({ "name": null })),
            Shared::Unsupported,
            "old server"
        );
        assert_eq!(icon(json!({ "name": null, "icon": null })), Shared::Unset);
        assert_eq!(
            icon(json!({ "name": null, "icon": { "kind": "pattern", "hue": 145 } })),
            Shared::Set(Stored::Pattern { hue: Some(145) })
        );
        assert_eq!(
            icon(json!({ "name": null, "icon": { "kind": "sticker" } })),
            Shared::Unsupported,
            "a newer kind leaves this desktop's icon alone"
        );

        let (origin, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        let emoji = ServerIcon::Emoji {
            emoji: "🐙".into(),
            hue: Some(195),
        };
        client(origin).set_server_icon(emoji, true).await.unwrap();
        let request = server.await.unwrap();
        assert!(request.starts_with("PUT /v1/settings/server/icon HTTP/1.1\r\n"));
        let (_, body) = request.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            json!({
                "icon": { "kind": "emoji", "emoji": "🐙", "hue": 195 },
                "picture": null,
                "only_if_unset": true,
            })
        );

        let png = b"png bytes".to_vec();
        let digest = crate::server_icon::digest(&png);
        let body = json!({
            "icon": { "kind": "picture", "digest": digest },
            "picture": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png),
        })
        .to_string();
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        for (asked, expected) in [(digest.clone(), Some(png.clone())), ("0".repeat(64), None)] {
            let (origin, server) = serve_once(response).await;
            let fetched = client(origin).server_picture(asked).await.unwrap();
            assert_eq!(fetched, expected, "only the picture the check saw is drawn");
            assert!(
                server
                    .await
                    .unwrap()
                    .starts_with("GET /v1/settings/server/icon HTTP/1.1\r\n")
            );
        }
    }

    #[tokio::test]
    async fn redirects_are_not_followed_and_token_stays_in_header() {
        let (origin, server) = serve_once(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://evil.example/\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        let err = client(origin).bots().await.unwrap_err();
        assert!(
            matches!(err, ClientError::Protocol(ref m) if m.contains("redirect")),
            "{err:?}"
        );
        let request = server.await.unwrap();
        assert!(request.contains(&format!("authorization: Bearer {}", "T".repeat(43))));
        assert!(
            !request.lines().next().unwrap().contains(&"T".repeat(43)),
            "token not in URL"
        );
    }

    #[tokio::test]
    async fn remove_mcp_accepts_an_empty_no_content_response() {
        let (origin, server) =
            serve_once("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n").await;
        client(origin).remove_mcp("t3-code".into()).await.unwrap();
        let request = server.await.unwrap();
        assert!(request.starts_with("DELETE /v1/settings/mcp/t3-code HTTP/1.1\r\n"));
    }

    #[tokio::test]
    async fn remove_email_deletes_the_account() {
        let (origin, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"disconnected\":true}",
        )
        .await;
        let account = Uuid::from_u128(7);
        client(origin).remove_email(account).await.unwrap();
        let request = server.await.unwrap();
        assert!(request.starts_with(&format!(
            "DELETE /v1/connectors/email/accounts/{account} HTTP/1.1\r\n"
        )));
    }

    #[tokio::test]
    async fn owner_notice_dismiss_uses_the_notice_endpoint_and_reports_a_settled_conflict() {
        let (origin, server) = serve_once(
            "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":{\"code\":\"owner_notice_settled\",\"message\":\"You already replied\"}}",
        ).await;
        let bot = Uuid::from_u128(80);
        let notice = Uuid::from_u128(400);
        let err = client(origin)
            .dismiss_owner_notice(bot, notice)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ClientError::Api {status: 409, ref code, ..} if code == "owner_notice_settled")
        );
        let request = server.await.unwrap();
        assert!(request.starts_with(&format!(
            "POST /v1/bots/{bot}/owner-notices/{notice}/dismiss HTTP/1.1\r\n"
        )));
    }

    #[tokio::test]
    async fn unauthorized_and_unreachable_are_distinct() {
        let (origin, _server) = serve_once(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        assert_eq!(
            client(origin).bots().await.unwrap_err(),
            ClientError::Unauthorized
        );
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let origin = Origin::parse(&format!("http://127.0.0.1:{port}")).unwrap();
        let err = client(origin).bots().await.unwrap_err();
        assert!(matches!(err, ClientError::Unreachable { .. }), "{err:?}");
        assert!(!err.to_string().contains(&"T".repeat(43)));
    }

    async fn first_bytes() -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            buf.truncate(n);
            buf
        });
        (address, task)
    }

    #[tokio::test]
    async fn https_and_wss_origins_send_the_token_only_over_tls() {
        let token = "T".repeat(43);
        let (address, server) = first_bytes().await;
        let origin = Origin::parse(&format!("https://{address}")).unwrap();
        let err = client(origin).bots().await.unwrap_err();
        assert!(matches!(err, ClientError::Unreachable { .. }), "{err:?}");
        let sent = server.await.unwrap();
        assert_eq!(sent.first(), Some(&0x16), "a TLS handshake record");
        assert!(!String::from_utf8_lossy(&sent).contains(&token));

        let (address, server) = first_bytes().await;
        let origin = Origin::parse(&format!("https://{address}")).unwrap();
        let live = client(origin).live();
        let sent = server.await.unwrap();
        assert_eq!(sent.first(), Some(&0x16), "the WebSocket uses TLS");
        assert!(!String::from_utf8_lossy(&sent).contains(&token));
        drop(live);
    }

    #[tokio::test]
    async fn reconnect_cuts_a_long_backoff_short() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });
        let origin = Origin::parse(&format!("http://{address}")).unwrap();
        let mut live = client(origin).live();
        async fn lost(events: &mut tokio::sync::mpsc::Receiver<LiveEvent>) {
            let next = async {
                loop {
                    match events.recv().await {
                        Some(LiveEvent::Disconnected(_)) => return,
                        Some(_) => {}
                        None => panic!("the live connection ended"),
                    }
                }
            };
            tokio::time::timeout(Duration::from_secs(60), next)
                .await
                .expect("the live connection retries");
        }
        for _ in 0..4 {
            lost(&mut live.events).await;
        }
        // The fourth failure waits about four seconds before retrying.
        let woken = std::time::Instant::now();
        live.commands.reconnect();
        lost(&mut live.events).await;
        assert!(
            woken.elapsed() < Duration::from_secs(3),
            "{:?}",
            woken.elapsed()
        );
        server.abort();
    }

    #[tokio::test]
    async fn api_errors_use_the_shared_shape() {
        let body = r#"{"error":{"code":"queue_full","message":"Too many messages.","retryable":true,"request_id":null}}"#;
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let (origin, _server) = serve_once(response).await;
        let err = client(origin).bots().await.unwrap_err();
        assert_eq!(err.code(), Some("queue_full"));
        assert!(!err.outcome_unknown());
    }

    #[test]
    fn subscription_mailbox_coalesces_and_preserves_delivered_cursors() {
        let (tx, mut rx) = tokio::sync::watch::channel(HashMap::new());
        let commands = LiveCommands {
            wanted: tx,
            wake: Default::default(),
        };
        let old = Uuid::new_v4();
        let current = Uuid::new_v4();
        commands
            .send(LiveCommand::Subscribe {
                bot: old,
                cursor: "1".into(),
            })
            .unwrap();
        commands
            .send(LiveCommand::Unsubscribe { bot: old })
            .unwrap();
        commands
            .send(LiveCommand::Subscribe {
                bot: current,
                cursor: "2".into(),
            })
            .unwrap();
        let mut wanted = HashMap::new();
        let mut applied = HashMap::new();
        let changes = subscriptions(&mut wanted, &mut applied, rx.borrow_and_update().clone());
        assert!(matches!(&changes[..], [LiveCommand::Subscribe { bot, .. }] if *bot == current));
        assert!(!wanted.contains_key(&old));
        wanted.insert(current, "3".into());
        assert!(subscriptions(&mut wanted, &mut applied, rx.borrow().clone()).is_empty());
        assert_eq!(
            wanted[&current], "3",
            "reconnect keeps the latest delivered cursor"
        );
        commands
            .send(LiveCommand::Unsubscribe { bot: current })
            .unwrap();
        let changes = subscriptions(&mut wanted, &mut applied, rx.borrow_and_update().clone());
        assert!(matches!(&changes[..], [LiveCommand::Unsubscribe { bot }] if *bot == current));
        assert!(wanted.is_empty());
    }

    #[test]
    fn routing_tracks_cursors_and_maps_channel_errors() {
        let bot = Uuid::new_v4();
        let mut wanted = HashMap::from([(bot, "i:b:1".to_owned())]);
        let frame = json!({ "channel": format!("bot:{bot}"), "id": "i:b:2", "event": "item.delta", "data": { "x": 1 } });
        match route(&frame, &mut wanted) {
            Some(LiveEvent::Bot {
                message: StreamMessage::Frame(f),
                ..
            }) => {
                assert_eq!(f.id.as_deref(), Some("i:b:2"));
                assert_eq!(f.event, Err(crate::model::Reset::Malformed));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(wanted[&bot], "i:b:2", "resume cursor advanced");
        let reset = json!({ "channel": format!("bot:{bot}"), "error": { "code": "cursor_reset_required" } });
        assert!(matches!(
            route(&reset, &mut wanted),
            Some(LiveEvent::Bot {
                message: StreamMessage::Reset,
                ..
            })
        ));
        assert!(
            !wanted.contains_key(&bot),
            "a failed channel is not resubscribed"
        );
        let deleted =
            json!({ "channel": format!("bot:{bot}"), "error": { "code": "bot_deleted" } });
        assert!(matches!(
            route(&deleted, &mut wanted),
            Some(LiveEvent::Bot {
                message: StreamMessage::Deleted,
                ..
            })
        ));
        let bots = json!({ "channel": "bots", "event": "bots.snapshot", "data": { "default_bot_id": null, "bots": [] } });
        assert!(matches!(
            route(&bots, &mut wanted),
            Some(LiveEvent::Bots(_))
        ));
    }

    #[tokio::test]
    async fn live_connection_authenticates_and_resumes_after_reconnect() {
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let bot = Uuid::new_v4();
        let (seen_tx, mut seen) = tokio::sync::mpsc::channel::<String>(16);
        tokio::spawn(async move {
            for round in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let seen_tx = seen_tx.clone();
                #[expect(
                    clippy::result_large_err,
                    reason = "Tungstenite fixes the callback error type to ErrorResponse."
                )]
                let mut socket = tokio_tungstenite::accept_hdr_async(
                    stream,
                    |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                     response| {
                        let auth = request.headers()["authorization"]
                            .to_str()
                            .unwrap()
                            .to_owned();
                        let _ = seen_tx.try_send(format!("auth {auth}"));
                        Ok(response)
                    },
                )
                .await
                .unwrap();
                while let Ok(Some(Ok(Message::Text(text)))) =
                    tokio::time::timeout(Duration::from_millis(300), socket.next()).await
                {
                    let _ = seen_tx.send(text.to_string()).await;
                }
                if round == 0 {
                    let frame = json!({ "channel": format!("bot:{bot}"), "id": "i:b:7", "event": "turn.updated", "data": {} });
                    socket
                        .send(Message::Text(frame.to_string().into()))
                        .await
                        .unwrap();
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        });
        let client = client(origin);
        let mut live = client.live();
        assert!(matches!(
            live.events.recv().await,
            Some(LiveEvent::Connected)
        ));
        live.commands
            .send(LiveCommand::Subscribe {
                bot,
                cursor: "i:b:5".into(),
            })
            .unwrap();
        assert!(matches!(
            live.events.recv().await,
            Some(LiveEvent::Bot { .. })
        ));
        assert!(matches!(
            live.events.recv().await,
            Some(LiveEvent::Disconnected(_))
        ));
        let dropped = tokio::time::Instant::now();
        assert!(matches!(
            live.events.recv().await,
            Some(LiveEvent::Connected)
        ));
        assert!(dropped.elapsed() >= Duration::from_millis(500));
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut log = Vec::new();
        while let Ok(line) = seen.try_recv() {
            log.push(line);
        }
        assert_eq!(
            log.iter()
                .filter(|line| line.contains("\"channel\":\"server\""))
                .count(),
            2,
            "the server channel is subscribed on every connection: {log:?}"
        );
        let token = format!("auth Bearer {}", "T".repeat(43));
        assert_eq!(log.iter().filter(|l| **l == token).count(), 2, "{log:?}");
        assert!(
            log.iter().any(|l| l.contains("\"cursor\":\"i:b:5\"")),
            "{log:?}"
        );
        assert!(
            log.iter().any(|l| l.contains("\"cursor\":\"i:b:7\"")),
            "resubscribed from the last delivered cursor: {log:?}"
        );
    }
}
