use super::{Client, ClientError};
use else_wire::email::{
    Accounts, Connected, Definitions, EmailAccount, EmailAuthorization, EmailConfig, EmailMessage,
    EmailPreset, EmailProvider, MessageRef,
};
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

impl Client {
    pub fn cancel_email_authorization(self: Arc<Self>, request_id: Uuid) {
        let runtime = self.runtime.clone();
        runtime.spawn(async move {
            let _ = self
                .call_unit(
                    Method::DELETE,
                    format!("/v1/connectors/email/oauth/{request_id}"),
                    None,
                )
                .await;
        });
    }

    /// One message for the owner's reader, fetched with the inbox's saved
    /// sign-in. It stays unread on the server.
    pub async fn view_email(
        self: Arc<Self>,
        account: Uuid,
        message: MessageRef,
    ) -> Result<EmailMessage, ClientError> {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("folder", &message.folder)
            .append_pair("uid_validity", &message.uid_validity.to_string())
            .append_pair("uid", &message.uid.to_string())
            .finish();
        self.call::<EmailMessage>(
            Method::GET,
            format!("/v1/connectors/email/accounts/{account}/messages?{query}"),
            None,
        )
        .await
    }

    pub async fn email_accounts(self: Arc<Self>) -> Result<Vec<EmailAccount>, ClientError> {
        self.call::<Accounts>(Method::GET, "/v1/connectors/email/accounts".into(), None)
            .await
            .map(|body| body.accounts)
    }

    pub async fn email_presets(self: Arc<Self>) -> Result<Vec<EmailPreset>, ClientError> {
        self.call::<Definitions>(Method::GET, "/v1/connectors".into(), None)
            .await
            .map(|body| {
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
            this.send_json::<T>(request).await
        })
        .await
    }

    pub async fn connect_email(
        self: Arc<Self>,
        config: EmailConfig,
        password: String,
    ) -> Result<EmailAccount, ClientError> {
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
        self.sign_in::<serde::de::IgnoredAny>(
            Method::PUT,
            format!("/v1/connectors/email/accounts/{account}/credentials"),
            json!({"kind":"password","password":password}),
        )
        .await
        .map(drop)
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
            .await
    }

    pub async fn complete_email(
        self: Arc<Self>,
        request_id: Uuid,
        state: String,
        code: String,
    ) -> Result<EmailAccount, ClientError> {
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
        self.call_unit(
            Method::PUT,
            format!("/v1/connectors/email/accounts/{account}/bots/{bot}/wake"),
            Some(json!({"wake":wake})),
        )
        .await
    }

    /// Remove an inbox for every bot. Its messages stay in the mailbox.
    pub async fn remove_email(self: Arc<Self>, account: Uuid) -> Result<(), ClientError> {
        self.call_unit(
            Method::DELETE,
            format!("/v1/connectors/email/accounts/{account}"),
            None,
        )
        .await
    }

    /// Delete a bot's trigger on an inbox. The bot still reads the inbox.
    pub async fn delete_email_trigger(
        self: Arc<Self>,
        account: Uuid,
        bot: Uuid,
    ) -> Result<(), ClientError> {
        self.call_unit(
            Method::DELETE,
            format!("/v1/connectors/email/accounts/{account}/bots/{bot}/trigger"),
            None,
        )
        .await
    }

    pub async fn introduce_email(
        self: Arc<Self>,
        account: Uuid,
        bot: Uuid,
        request: Uuid,
    ) -> Result<(), ClientError> {
        self.call_unit(
            Method::POST,
            format!("/v1/connectors/email/accounts/{account}/introduction"),
            Some(json!({"bot_id":bot,"request_id":request})),
        )
        .await
    }
}
