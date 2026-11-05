use super::{Client, ClientError};
use crate::attachments::{Attachment, MAX_FILE_BYTES};
use base64::Engine as _;
use futures_util::StreamExt as _;
use reqwest::{Method, StatusCode};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

impl Client {
    pub async fn upload_attachment(
        self: Arc<Self>,
        bot: Uuid,
        file: Attachment,
        data: Arc<Vec<u8>>,
    ) -> Result<Attachment, ClientError> {
        let this = self.clone();
        self.run(async move {
            let expected = file.clone();
            let body = serde_json::json!({
                "id": file.id, "name": file.name, "media_type": file.media_type,
                "data": base64::engine::general_purpose::STANDARD.encode(data.as_ref()),
            });
            let path = format!("/v1/bots/{bot}/attachments");
            let request = this
                .request(Method::POST, &path)
                .timeout(Duration::from_secs(120))
                .json(&body);
            drop(body);
            drop(data);
            let (_, file): (_, Attachment) = this.send_json(request).await?;
            if file.id != expected.id || file.name != expected.name || file.size != expected.size {
                return Err(ClientError::Protocol(
                    "The server returned different attachment metadata.".into(),
                ));
            }
            Ok(file)
        })
        .await
    }

    pub async fn attachment_bytes(
        self: Arc<Self>,
        bot: Uuid,
        id: Uuid,
    ) -> Result<Vec<u8>, ClientError> {
        let this = self.clone();
        self.run(async move {
            let response = this
                .request(Method::GET, &format!("/v1/bots/{bot}/attachments/{id}"))
                .timeout(Duration::from_secs(30))
                .send()
                .await
                .map_err(|e| ClientError::Protocol(e.without_url().to_string()))?;
            if response.status() == StatusCode::UNAUTHORIZED {
                return Err(ClientError::Unauthorized);
            }
            if !response.status().is_success() {
                return Err(ClientError::Protocol(format!(
                    "Could not fetch attachment ({})",
                    response.status()
                )));
            }
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk =
                    chunk.map_err(|e| ClientError::Protocol(e.without_url().to_string()))?;
                if bytes.len() + chunk.len() > MAX_FILE_BYTES {
                    return Err(ClientError::Protocol(
                        "Attachment exceeds the 10 MB limit.".into(),
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        })
        .await
    }
}
