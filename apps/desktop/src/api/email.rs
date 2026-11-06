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

    pub async fn connect_email(
        self: Arc<Self>,
        config: EmailConfig,
        password: String,
    ) -> Result<EmailAccount, ClientError> {
        #[derive(Deserialize)]
        struct Connected {
            account: EmailAccount,
        }
        self.call::<Connected>(
            Method::POST,
            "/v1/connectors/email/accounts".into(),
            Some(json!({"config":config,"credential":{"kind":"password","password":password}})),
        )
        .await
        .map(|(_, body)| body.account)
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
        let this = self.clone();
        self.run(async move {
            let request = this
                .request(Method::POST, "/v1/connectors/email/oauth/complete")
                .timeout(std::time::Duration::from_secs(60))
                .json(&json!({"request_id":request_id,"state":state,"code":code}));
            this.send_json::<Connected>(request)
                .await
                .map(|(_, body)| body.account)
        })
        .await
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
