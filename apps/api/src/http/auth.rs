//! Owner bearer authentication and request-origin checks.
//!
//! Authentication runs before any object lookup. Exactly one
//! `Authorization: Bearer <token>` header is accepted; query parameters and
//! cookies never carry credentials. Future runtime task grants will resolve to
//! a different [`Principal`] and be refused by owner routes with `403`.

use std::sync::Arc;
use std::sync::Mutex;
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

/// Failed-authentication budget per window, shared by all callers. The
/// listener is loopback-only, so per-address accounting adds nothing.
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const MAX_FAILURES_PER_WINDOW: u32 = 30;

#[derive(Debug)]
pub struct FailureLimiter {
    window: Mutex<(Instant, u32)>,
}

impl Default for FailureLimiter {
    fn default() -> Self {
        Self {
            window: Mutex::new((Instant::now(), 0)),
        }
    }
}

impl FailureLimiter {
    fn window(&self) -> std::sync::MutexGuard<'_, (Instant, u32)> {
        let mut guard = self.window.lock().unwrap_or_else(|p| p.into_inner());
        if guard.0.elapsed() >= FAILURE_WINDOW {
            *guard = (Instant::now(), 0);
        }
        guard
    }

    /// Seconds until attempts are accepted again, if the budget is spent.
    fn blocked_for(&self) -> Option<u64> {
        let guard = self.window();
        (guard.1 >= MAX_FAILURES_PER_WINDOW).then(|| {
            FAILURE_WINDOW
                .saturating_sub(guard.0.elapsed())
                .as_secs()
                .max(1)
        })
    }

    fn record_failure(&self) {
        self.window().1 += 1;
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
        // Bracketed IPv6 literal with an optional port.
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

/// Extract the single bearer credential, or `None` for any missing,
/// duplicated, or malformed header.
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
    match authenticate(&state, request.headers()) {
        Ok(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(response) => response,
    }
}

/// Check the owner bearer credential, returning the rejection response.
pub fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Principal, Response> {
    let authenticated = bearer(headers).is_some_and(|token| state.owner_token.matches(token));
    if !authenticated {
        // The valid owner is never locked out; failures beyond the budget
        // get a cheap 429 instead of a 401.
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
