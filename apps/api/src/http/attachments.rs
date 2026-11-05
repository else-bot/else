//! Uploading files for a bot's messages and reading them back.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use serde::Deserialize;
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use crate::app::messages::{self, InvalidAttachment, MAX_ATTACHMENT_BYTES};
use crate::db::attachments::{self, StoreError};

/// Largest upload body: a 10 MiB file in base64 plus its JSON envelope.
pub const MAX_UPLOAD_BODY_BYTES: usize = 14 * 1024 * 1024;

/// Whether a request is an upload, which may exceed the usual body limit.
pub fn is_upload(method: &axum::http::Method, path: &str) -> bool {
    method == axum::http::Method::POST
        && path
            .strip_prefix("/v1/bots/")
            .and_then(|rest| rest.strip_suffix("/attachments"))
            .is_some_and(|bot| !bot.contains('/'))
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/bots/{bot_id}/attachments",
            post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD_BODY_BYTES)),
        )
        .route("/bots/{bot_id}/attachments/{attachment_id}", get(download))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadBody {
    id: Uuid,
    name: String,
    media_type: String,
    /// Standard base64.
    data: String,
}

impl From<InvalidAttachment> for ApiError {
    fn from(err: InvalidAttachment) -> Self {
        match err {
            InvalidAttachment::TooLarge => {
                ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", err.to_string())
            }
            _ => ApiError::bad_request("invalid_attachment", err.to_string()),
        }
    }
}

/// `201` with the file's metadata, or `200` when this exact upload was
/// already stored.
async fn upload(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UploadBody>,
) -> Result<Response, ApiError> {
    messages::validate_name(&body.name)?;
    let media_type = messages::media_type(&body.media_type)?;
    // Base64 holds at least 3 bytes per 4 characters.
    if body.data.len() as u64 > MAX_ATTACHMENT_BYTES.div_ceil(3) * 4 {
        return Err(InvalidAttachment::TooLarge.into());
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(body.data.as_bytes())
        .map_err(|_| InvalidAttachment::Encoding)?;
    if data.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(InvalidAttachment::TooLarge.into());
    }
    let (file, created) =
        attachments::store(&state.db, bot_id, body.id, &body.name, &media_type, &data)
            .await
            .map_err(|err| match err {
                StoreError::NotFound => ApiError::not_found(),
                StoreError::Deleted => super::turns::bot_deleted(),
                StoreError::Conflict => ApiError::new(
                    StatusCode::CONFLICT,
                    "attachment_conflict",
                    "This attachment ID was already used with different content.",
                ),
                StoreError::Db(err) => err.into(),
            })?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(file)).into_response())
}

/// The file's bytes. Only raster images keep their media type; anything
/// else is served as an opaque download.
async fn download(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, attachment_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    super::turns::active_bot(&state, bot_id).await?;
    let (file, data) = attachments::read(&state.db, bot_id, attachment_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    let content_type = match file.media_type.as_str() {
        "image/png" => "image/png",
        "image/jpeg" => "image/jpeg",
        "image/gif" => "image/gif",
        "image/webp" => "image/webp",
        _ => "application/octet-stream",
    };
    let disposition = format!(
        "attachment; filename*=UTF-8''{}",
        percent_encode(&file.name)
    );
    let mut response = data.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok(response)
}

/// RFC 8187 encoding of a file name for `filename*`.
fn percent_encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_paths_are_recognized_exactly() {
        let post = axum::http::Method::POST;
        let id = Uuid::nil();
        assert!(is_upload(&post, &format!("/v1/bots/{id}/attachments")));
        assert!(!is_upload(
            &axum::http::Method::GET,
            &format!("/v1/bots/{id}/attachments")
        ));
        assert!(!is_upload(&post, &format!("/v1/bots/{id}/turns")));
        assert!(!is_upload(&post, &format!("/v1/bots/{id}/x/attachments")));
    }

    #[test]
    fn file_names_are_encoded_for_disposition() {
        assert_eq!(percent_encode("a b\"é.png"), "a%20b%22%C3%A9.png");
    }
}
