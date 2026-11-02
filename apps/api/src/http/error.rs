//! The single JSON error shape returned by every route.
#![allow(dead_code)] // Constructors are used as product routes are added.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
    pub request_id: Option<String>,
    pub retry_after_secs: Option<u64>,
}

#[derive(Serialize)]
struct Body<'a> {
    error: Inner<'a>,
}

#[derive(Serialize)]
struct Inner<'a> {
    code: &'a str,
    message: &'a str,
    retryable: bool,
    request_id: Option<&'a str>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retryable: false,
            request_id: None,
            retry_after_secs: None,
        }
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    pub fn with_request_id(mut self, request_id: impl ToString) -> Self {
        self.request_id = Some(request_id.to_string());
        self
    }

    pub fn retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs);
        self.retryable = true;
        self
    }

    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "Not found.")
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "A valid owner bearer token is required.",
        )
    }

    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "The server could not complete the request.",
        )
        .retryable()
    }

    pub fn unavailable(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, code, message).retryable()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Body {
            error: Inner {
                code: self.code,
                message: &self.message,
                retryable: self.retryable,
                request_id: self.request_id.as_deref(),
            },
        };
        let mut response = (self.status, Json(body)).into_response();
        if let Some(secs) = self.retry_after_secs {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        response
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        let status = rejection.status();
        if status == StatusCode::PAYLOAD_TOO_LARGE {
            return ApiError::new(status, "too_large", "The request body is too large.");
        }
        if status == StatusCode::UNSUPPORTED_MEDIA_TYPE {
            return ApiError::new(
                status,
                "unsupported_media_type",
                "Send the request body as application/json.",
            );
        }
        ApiError::bad_request("invalid_body", rejection.body_text())
    }
}

/// JSON extractor that reports rejections in the shared error shape.
#[derive(axum::extract::FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct ApiJson<T>(pub T);

/// Path extractor. A malformed object ID is reported as an unknown object.
#[derive(axum::extract::FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
pub struct ApiPath<T>(pub T);

impl From<axum::extract::rejection::PathRejection> for ApiError {
    fn from(_: axum::extract::rejection::PathRejection) -> Self {
        ApiError::not_found()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        // Database messages can describe schema details; keep them in logs.
        match err {
            sqlx::Error::PoolTimedOut | sqlx::Error::Io(_) | sqlx::Error::PoolClosed => {
                tracing::warn!(error = %err, "database unavailable");
                ApiError::unavailable("database_unavailable", "The database is not reachable.")
            }
            other => {
                tracing::error!(error = %other, "database error");
                ApiError::internal()
            }
        }
    }
}
