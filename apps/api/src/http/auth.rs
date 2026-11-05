//! Owner bearer authentication and request-origin checks.
//!
//! Authentication runs before any object lookup. Exactly one
//! `Authorization: Bearer <token>` header is accepted; query parameters and
//! cookies never carry credentials. Future runtime task grants will resolve to
//! a different [`Principal`] and be refused by owner routes with `403`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::AppState;
use super::error::ApiError;

/// The authenticated caller attached to a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal {
    Owner,
}

const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const MAX_FAILURES_PER_WINDOW: u32 = 30;

#[derive(Debug)]
pub struct FailureLimiter {
    started: Instant,
    window: AtomicU64,
}

impl Default for FailureLimiter {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            window: AtomicU64::new(0),
        }
    }
}

impl FailureLimiter {
    fn epoch(&self) -> u64 {
        self.started.elapsed().as_secs() / FAILURE_WINDOW.as_secs()
    }

    fn blocked_for(&self) -> Option<u64> {
        let window = self.window.load(Ordering::Acquire);
        (window >> 32 == self.epoch() && window as u32 >= MAX_FAILURES_PER_WINDOW).then(|| {
            (FAILURE_WINDOW
                - Duration::from_secs(self.started.elapsed().as_secs() % FAILURE_WINDOW.as_secs()))
            .as_secs()
            .max(1)
        })
    }

    fn record_failure(&self) {
        let _ = self
            .window
            .try_update(Ordering::AcqRel, Ordering::Acquire, |window| {
                let epoch = self.epoch();
                let count = if window >> 32 == epoch {
                    (window as u32).saturating_add(1)
                } else {
                    1
                };
                Some((epoch << 32) | u64::from(count))
            });
    }
}

/// Reject browser-originated requests and Host names that are not loopback.
/// The native desktop sends no `Origin`; DNS-rebinding pages would. The port
/// is not checked so an SSH tunnel's local port works.
pub async fn check_origin(request: Request, next: Next) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden_origin",
            "Browser origins are not accepted by this API.",
        )
        .into_response();
    }
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| request.uri().authority().map(|a| a.as_str()));
    if !host.is_some_and(is_loopback_host) {
        return ApiError::bad_request(
            "invalid_host",
            "The Host header must name a loopback address.",
        )
        .into_response();
    }
    next.run(request).await
}

fn is_loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((addr, tail)) if tail.is_empty() || is_port(tail) => addr,
            _ => return false,
        }
    } else {
        match host.rsplit_once(':') {
            Some((name, port)) if is_port(&format!(":{port}")) => name,
            Some(_) => return false,
            None => host,
        }
    };
    if name.eq_ignore_ascii_case("localhost") {
        return true;
    }
    name.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

fn is_port(tail: &str) -> bool {
    tail.strip_prefix(':')
        .is_some_and(|port| !port.is_empty() && port.parse::<u16>().is_ok())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let text = value.to_str().ok()?;
    let (scheme, token) = text.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
        return None;
    }
    // RFC 6750 token68 characters only; rejects embedded whitespace.
    let valid = token.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
    });
    valid.then_some(token)
}

pub async fn require_owner(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    match authenticate(&state, request.headers()).await {
        Ok(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(response) => response,
    }
}

/// Check the owner bearer credential, returning the rejection response.
pub async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Principal, Response> {
    let presented = bearer(headers);
    let authenticated = presented.is_some_and(|token| state.owner_token.matches(token));
    if !authenticated {
        if let Some(token) = presented
            && state.scheduler.grants().resolve(token).await.is_some()
        {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "task_grant_forbidden",
                "Task grants cannot use owner routes.",
            )
            .into_response());
        }
        if let Some(retry_after) = state.auth_failures.blocked_for() {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_failures",
                "Too many failed authentication attempts.",
            )
            .retry_after(retry_after)
            .into_response());
        }
        state.auth_failures.record_failure();
        let mut response = ApiError::unauthorized().into_response();
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return Err(response);
    }
    Ok(Principal::Owner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        for ok in [
            "127.0.0.1:7878",
            "localhost",
            "LOCALHOST:1",
            "[::1]:7878",
            "[::1]",
            "127.0.0.2:80",
        ] {
            assert!(is_loopback_host(ok), "{ok}");
        }
        for bad in [
            "example.com",
            "evil.localhost.example:7878",
            "10.0.0.1:7878",
            "[::1]x",
            "127.0.0.1:",
            "127.0.0.1:99999",
            "",
        ] {
            assert!(!is_loopback_host(bad), "{bad}");
        }
    }

    #[test]
    fn bearer_parsing() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc_-"),
        );
        assert_eq!(bearer(&headers), Some("abc_-"));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer abc"),
        );
        assert_eq!(bearer(&headers), Some("abc"));
        for bad in [
            "Basic abc",
            "Bearer",
            "Bearer ",
            "Bearer  abc",
            "Bearer a b",
            "abc",
        ] {
            headers.insert(header::AUTHORIZATION, HeaderValue::from_static(bad));
            assert_eq!(bearer(&headers), None, "{bad}");
        }
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        assert_eq!(bearer(&headers), None, "duplicated header");
    }
}

/// Authenticate a task-scoped tool request. Identity comes only from the
/// grant record; the owner token is not accepted here.
pub async fn require_task_grant(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let presented = bearer(request.headers());
    if presented.is_some_and(|token| state.owner_token.matches(token)) {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "owner_token_forbidden",
            "Tool routes accept task grants only.",
        )
        .into_response();
    }
    let grant = match presented {
        Some(token) => state.scheduler.grants().resolve(token).await,
        None => None,
    };
    match grant {
        Some(grant) => {
            request.extensions_mut().insert(grant);
            next.run(request).await
        }
        None => {
            if let Some(retry_after) = state.auth_failures.blocked_for() {
                return ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "too_many_failures",
                    "Too many failed authentication attempts.",
                )
                .retry_after(retry_after)
                .into_response();
            }
            state.auth_failures.record_failure();
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "invalid_task_grant",
                "A valid task grant is required.",
            )
            .into_response()
        }
    }
}
