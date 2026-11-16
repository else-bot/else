use super::{Client, ClientError};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailProvider {
    Gmail,
    Microsoft,
    Yahoo,
    Icloud,
    Imap,
}

impl EmailProvider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Gmail => "Gmail",
            Self::Microsoft => "Microsoft 365",
            Self::Yahoo => "Yahoo",
            Self::Icloud => "iCloud",
            Self::Imap => "Other email",
        }
    }

    /// Where the owner makes an app password for this provider, shown on a
    /// card that asks for one.
    pub fn app_password_help(self) -> Option<AppPasswordHelp> {
        match self {
            Self::Icloud => Some(AppPasswordHelp {
                note: "Use an app-specific password, not your Apple Account password. \
                       Two-factor authentication must be on.",
                link: "Open account.apple.com",
                url: "https://account.apple.com/",
            }),
            _ => None,
        }
    }
}

pub struct AppPasswordHelp {
    pub note: &'static str,
    pub link: &'static str,
    pub url: &'static str,
}

/// A bot's trigger on an inbox: whether new mail wakes it, and what to do
/// then. A trigger the inbox sign-in card turned on has no instructions.
#[derive(Debug, Clone, Deserialize)]
pub struct MailTrigger {
    pub account_id: Uuid,
    pub address: String,
    pub state: String,
    pub wake: bool,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub last_woke_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct EmailEndpoint {
    pub host: String,
    pub port: u16,
    pub start_tls: bool,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct EmailConfig {
    pub provider: EmailProvider,
    pub address: String,
    pub username: String,
    pub imap: EmailEndpoint,
    pub smtp: EmailEndpoint,
    pub archive_folder: String,
    pub trash_folder: String,
    pub drafts_folder: String,
    pub sent_folder: String,
}

#[derive(Clone, Deserialize)]
pub struct EmailAccount {
    pub id: Uuid,
    pub config: EmailConfig,
    pub state: String,
    pub health: String,
}

#[derive(Deserialize)]
pub struct EmailPreset {
    pub provider: EmailProvider,
    pub preset: Option<EmailConfig>,
}

/// The server's settings for `provider`, signing in as `address`.
pub fn preset_config(
    presets: &[EmailPreset],
    provider: EmailProvider,
    address: &str,
) -> Option<EmailConfig> {
    let mut config = presets
        .iter()
        .find(|p| p.provider == provider)?
        .preset
        .clone()?;
    config.address = address.to_owned();
    config.username = address.to_owned();
    Some(config)
}

/// The inbox a bot asked the owner to sign in to from a chat card.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct EmailConsent {
    pub provider: EmailProvider,
    pub address: String,
}

#[derive(Deserialize)]
pub struct EmailAuthorization {
    pub request_id: Uuid,
    pub authorization_url: String,
}

impl Client {
    pub fn cancel_email_authorization(self: Arc<Self>, request_id: Uuid) {
        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            let _ = self
                .call::<Value>(
                    Method::DELETE,
                    format!("/v1/connectors/email/oauth/{request_id}"),
                    None,
                )
                .await;
        });
    }

    pub async fn email_accounts(self: Arc<Self>) -> Result<Vec<EmailAccount>, ClientError> {
        #[derive(Deserialize)]
        struct Accounts {
            accounts: Vec<EmailAccount>,
        }
        self.call::<Accounts>(Method::GET, "/v1/connectors/email/accounts".into(), None)
            .await
            .map(|(_, body)| body.accounts)
    }

    pub async fn email_presets(self: Arc<Self>) -> Result<Vec<EmailPreset>, ClientError> {
        #[derive(Deserialize)]
        struct Definition {
            providers: Vec<EmailPreset>,
        }
        #[derive(Deserialize)]
        struct Definitions {
            connectors: Vec<Definition>,
        }
        self.call::<Definitions>(Method::GET, "/v1/connectors".into(), None)
            .await
            .map(|(_, body)| {
                body.connectors
                    .into_iter()
                    .flat_map(|d| d.providers)
                    .collect()
            })
    }

    /// A request that signs in to a mail server, which may take longer than
    /// the usual request timeout.
    async fn sign_in<T: serde::de::DeserializeOwned + Send + 'static>(
        self: Arc<Self>,
        method: Method,
        path: String,
        body: Value,
    ) -> Result<T, ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this
                .request(method, &path)
                .timeout(std::time::Duration::from_secs(60))
                .json(&body);
            this.send_json::<T>(request).await.map(|(_, body)| body)
        })
        .await
    }

    pub async fn connect_email(
        self: Arc<Self>,
        config: EmailConfig,
        password: String,
    ) -> Result<EmailAccount, ClientError> {
        #[derive(Deserialize)]
        struct Connected {
            account: EmailAccount,
        }
        self.sign_in::<Connected>(
            Method::POST,
            "/v1/connectors/email/accounts".into(),
            json!({"config":config,"credential":{"kind":"password","password":password}}),
        )
        .await
        .map(|body| body.account)
    }

    /// Replaces an inbox's password. The server signs in with it first.
    pub async fn replace_email_password(
        self: Arc<Self>,
        account: Uuid,
        password: String,
    ) -> Result<(), ClientError> {
        self.sign_in::<Value>(
            Method::PUT,
            format!("/v1/connectors/email/accounts/{account}/credentials"),
            json!({"kind":"password","password":password}),
        )
        .await
        .map(|_| ())
    }

    pub async fn authorize_email(
        self: Arc<Self>,
        provider: EmailProvider,
        address: String,
        client_id: String,
        client_secret: Option<String>,
        redirect_uri: String,
    ) -> Result<EmailAuthorization, ClientError> {
        self.call(Method::POST, "/v1/connectors/email/oauth/start".into(),
            Some(json!({"provider":provider,"address":address,"client_id":client_id,"client_secret":client_secret,"redirect_uri":redirect_uri})))
            .await.map(|(_, body)| body)
    }

    pub async fn complete_email(
        self: Arc<Self>,
        request_id: Uuid,
        state: String,
        code: String,
    ) -> Result<EmailAccount, ClientError> {
        #[derive(Deserialize)]
        struct Connected {
            account: EmailAccount,
        }
        self.sign_in::<Connected>(
            Method::POST,
            "/v1/connectors/email/oauth/complete".into(),
            json!({"request_id":request_id,"state":state,"code":code}),
        )
        .await
        .map(|body| body.account)
    }

    pub async fn set_email_wake(
        self: Arc<Self>,
        account: Uuid,
        bot: Uuid,
        wake: bool,
    ) -> Result<(), ClientError> {
        self.call::<Value>(
            Method::PUT,
            format!("/v1/connectors/email/accounts/{account}/bots/{bot}/wake"),
            Some(json!({"wake":wake})),
        )
        .await
        .map(|_| ())
    }

    /// Remove an inbox for every bot. Its messages stay in the mailbox.
    pub async fn remove_email(self: Arc<Self>, account: Uuid) -> Result<(), ClientError> {
        self.call::<Value>(
            Method::DELETE,
            format!("/v1/connectors/email/accounts/{account}"),
            None,
        )
        .await
        .map(|_| ())
    }

    /// Delete a bot's trigger on an inbox. The bot still reads the inbox.
    pub async fn delete_email_trigger(
        self: Arc<Self>,
        account: Uuid,
        bot: Uuid,
    ) -> Result<(), ClientError> {
        self.call::<Value>(
            Method::DELETE,
            format!("/v1/connectors/email/accounts/{account}/bots/{bot}/trigger"),
            None,
        )
        .await
        .map(|_| ())
    }

    pub async fn introduce_email(
        self: Arc<Self>,
        account: Uuid,
        bot: Uuid,
        request: Uuid,
    ) -> Result<(), ClientError> {
        self.call::<Value>(
            Method::POST,
            format!("/v1/connectors/email/accounts/{account}/introduction"),
            Some(json!({"bot_id":bot,"request_id":request})),
        )
        .await
        .map(|_| ())
    }
}
