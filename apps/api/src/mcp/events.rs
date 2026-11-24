//! The MCP Events extension (SEP-3415) over Streamable HTTP.
//!
//! Events exist only on protocol revision 2026-07-28, which has no session:
//! each request is one self-contained POST. The package's shared connection
//! keeps the older handshake, because some servers accept the new one and
//! then break, so events use their own stateless requests that never touch
//! it. Only poll and push delivery are used: webhooks need a public `https`
//! address, which most installations lack. Every value parsed here comes
//! from the server and is untrusted.

use std::time::Duration;

use base64::Engine;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::{Error, Transport, USER_AGENT};
use crate::db::task_secrets::{self, Redact};

pub const EXTENSION: &str = "io.modelcontextprotocol/events";
const PROTOCOL: &str = "2026-07-28";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_LIST_PAGES: usize = 20;
const MAX_REMOTE_MESSAGE: usize = 300;
/// The shortest wait between polls, so a server cannot induce a tight loop.
pub const MIN_POLL: Duration = Duration::from_secs(1);
pub const SUGGESTED_HEARTBEAT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub enum Failure {
    /// The server does not offer events, or not over this transport.
    Unsupported(String),
    /// The server refused this subscription, and asking again cannot help.
    Rejected(String),
    /// A failure a later request may not repeat, such as a dropped
    /// connection.
    Transient(String),
    /// The server answered HTTP 401, so the sign-in needs a new token.
    Unauthorized,
    /// The event type's schema changed, so the saved cursor and descriptor
    /// no longer hold; the subscription starts over from a new cursor.
    SchemaChanged,
    Paused,
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        match error {
            Error::Paused => Self::Paused,
            Error::NotFound => Self::Rejected(error.to_string()),
            Error::Invalid(message) => Self::Unsupported(message.into()),
            error => Self::Transient(error.to_string()),
        }
    }
}

impl Failure {
    pub fn message(&self) -> &str {
        match self {
            Self::Unsupported(m) | Self::Rejected(m) | Self::Transient(m) => m,
            Self::Unauthorized => "The MCP server refused the sign-in (HTTP 401).",
            Self::SchemaChanged => "The MCP server changed the event type's schema.",
            Self::Paused => "The MCP library is paused for a backup or update.",
        }
    }
}

/// Invalid params and SEP-3415's NotFound, Forbidden and Unsupported
/// refuse a subscription for good.
fn refusal(code: i64) -> bool {
    matches!(code, -32602 | -32023 | -32024 | -32026)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Push,
    Poll,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Subscription {
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct Descriptor {
    pub delivery: Vec<Delivery>,
    /// Arguments mirrored into `Mcp-Param-*` headers: the property path
    /// and the header's name.
    headers: Vec<(Vec<String>, String)>,
}

/// One event with the fields a wait reads, and `raw`, the whole event
/// masked for the bot.
#[derive(Debug, Clone)]
pub struct Occurrence {
    pub name: String,
    pub data: Value,
    pub cursor: Option<String>,
    pub raw: Value,
}

impl Endpoint {
    /// An event as the server sent it, for matching, with a masked copy
    /// for the bot.
    fn occurrence(&self, mut raw: Value) -> Option<Occurrence> {
        let object = raw.as_object_mut()?;
        object.remove("_meta");
        let name = object.get("name")?.as_str()?.to_owned();
        object.get("eventId")?.as_str()?;
        let data = object.get("data").filter(|d| d.is_object())?.clone();
        let cursor = object
            .remove("cursor")
            .and_then(|c| c.as_str().map(str::to_owned));
        Some(Occurrence {
            name,
            data,
            cursor,
            raw: self.masked(raw),
        })
    }
}

#[derive(Debug)]
pub struct Polled {
    pub events: Vec<Occurrence>,
    pub cursor: Option<String>,
    pub truncated: bool,
    pub has_more: bool,
    pub next_poll: Duration,
}

#[derive(Debug)]
pub enum Pushed {
    /// The subscription is confirmed, at first and after a gap.
    Active {
        cursor: Option<String>,
        truncated: bool,
        heartbeat: Duration,
    },
    Event(Occurrence),
    Heartbeat {
        cursor: Option<String>,
    },
    /// One delivery failed; the subscription stays active.
    Error(String),
}

/// A server's address and the headers every request sends: the package's
/// filled-in headers and its sign-in.
#[derive(Clone)]
pub struct Endpoint {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    redact: Vec<Redact>,
    revoked: CancellationToken,
}

impl Endpoint {
    /// `config` already holds the sign-in and filled task secrets; `redact`
    /// lists the values to keep out of logs and results. `revoked` is
    /// cancelled once the package's configuration or sign-in changes.
    pub(super) fn new(
        config: &Transport,
        mut redact: Vec<Redact>,
        revoked: CancellationToken,
    ) -> Result<Self, Error> {
        let Transport::Http { url, headers, .. } = config else {
            return Err(Error::Invalid(
                "MCP events need an HTTP MCP; this one runs over stdio.",
            ));
        };
        let mut sent = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::Invalid("Invalid MCP HTTP header name."))?;
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| Error::Invalid("Invalid MCP HTTP header value."))?;
            value.set_sensitive(true);
            sent.insert(name, value);
        }
        redact.extend(super::transport::credentials(config));
        let bearer: Vec<Redact> = redact
            .iter()
            .filter_map(|r| {
                let token = r.value.strip_prefix("Bearer ")?;
                (token.len() >= task_secrets::MIN_REDACTED).then(|| Redact {
                    name: r.name.clone(),
                    value: Zeroizing::new(token.to_owned()),
                })
            })
            .collect();
        redact.extend(bearer);
        redact.sort_by_key(|r| std::cmp::Reverse(r.value.len()));
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|_| Error::Connection)?;
        Ok(Self {
            client,
            url: url.clone(),
            headers: sent,
            redact,
            revoked,
        })
    }

    /// Cancelled once the package's configuration or sign-in changes, after
    /// which nothing this endpoint receives may settle a wait.
    pub fn revoked(&self) -> &CancellationToken {
        &self.revoked
    }

    fn redacted(&self, text: &str) -> String {
        task_secrets::redact(text, &self.redact)
    }

    /// A JSON-RPC `error` as a failure. Its text comes from the server and
    /// may echo a credential, so it is masked and shortened.
    fn rpc_failure(&self, error: &Value) -> Failure {
        let code = error["code"].as_i64().unwrap_or_default();
        let text: String = self
            .redacted(error["message"].as_str().unwrap_or("MCP error"))
            .chars()
            .take(MAX_REMOTE_MESSAGE)
            .collect();
        let message = format!("{text} ({code})");
        if code == -32601 {
            Failure::Unsupported(message)
        } else if code == -32026 && error["data"]["reason"] == "schema_changed" {
            Failure::SchemaChanged
        } else if refusal(code) {
            Failure::Rejected(message)
        } else {
            Failure::Transient(message)
        }
    }

    /// Whether `other` sends the same requests to the same server, so what
    /// one subscribed to the other may still settle.
    pub fn same_target(&self, other: &Endpoint) -> bool {
        self.url == other.url && self.headers == other.headers
    }

    pub fn masked(&self, value: Value) -> Value {
        if self.redact.is_empty() {
            value
        } else {
            task_secrets::redact_json(value, &self.redact)
        }
    }

    async fn post(
        &self,
        method: &str,
        mut params: Map<String, Value>,
        headers: HeaderMap,
    ) -> Result<reqwest::Response, Failure> {
        params.insert(
            "_meta".into(),
            json!({
                "io.modelcontextprotocol/protocolVersion": PROTOCOL,
                "io.modelcontextprotocol/clientCapabilities": { "extensions": { EXTENSION: {} } },
                "io.modelcontextprotocol/clientInfo": {
                    "name": "else.bot", "version": env!("ELSE_VERSION"),
                },
            }),
        );
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let response = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .headers(headers)
            .header("MCP-Protocol-Version", PROTOCOL)
            .header("Mcp-Method", method)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&body)
            .send()
            .await
            .map_err(|error| Failure::Transient(self.redacted(&error.to_string())))?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Failure::Unauthorized);
        }
        if status.is_server_error() {
            return Err(Failure::Transient(format!(
                "The MCP server answered HTTP {status}."
            )));
        }
        Ok(response)
    }

    async fn call(
        &self,
        method: &str,
        params: Map<String, Value>,
        headers: HeaderMap,
    ) -> Result<Value, Failure> {
        let call = async {
            let response = self.post(method, params, headers).await?;
            let status = response.status();
            let message = if is_sse(&response) {
                let mut frames = Frames::new(response);
                loop {
                    match frames.next().await? {
                        Some(message) if message.get("id").is_some() => break message,
                        Some(_) => continue,
                        None => {
                            return Err(Failure::Transient(
                                "The MCP stream ended without a response.".into(),
                            ));
                        }
                    }
                }
            } else {
                let bytes = bounded_body(response).await?;
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(message) => message,
                    Err(_) if status.is_success() => {
                        return Err(Failure::Transient("The MCP response is not JSON.".into()));
                    }
                    Err(_) => {
                        return Err(Failure::Unsupported(format!(
                            "The MCP server answered HTTP {status}."
                        )));
                    }
                }
            };
            if let Some(error) = message.get("error") {
                return Err(self.rpc_failure(error));
            }
            message
                .get("result")
                .filter(|r| r.is_object())
                .cloned()
                .ok_or_else(|| Failure::Transient("The MCP response has no result.".into()))
        };
        tokio::time::timeout(REQUEST_TIMEOUT, call)
            .await
            .unwrap_or_else(|_| Err(Failure::Transient("The MCP request timed out.".into())))
    }

    /// Whether the server's `server/discover` declares events. A server
    /// that cannot answer on this revision offers none.
    pub async fn offers_events(&self) -> Result<bool, Failure> {
        match self
            .call("server/discover", Map::new(), HeaderMap::new())
            .await
        {
            Ok(result) => Ok(result["capabilities"]["extensions"]
                .get(EXTENSION)
                .is_some_and(Value::is_object)),
            Err(Failure::Unsupported(_) | Failure::Rejected(_)) => Ok(false),
            Err(failure) => Err(failure),
        }
    }

    async fn page(&self, cursor: Option<&str>) -> Result<Value, Failure> {
        let mut params = Map::new();
        if let Some(cursor) = cursor {
            params.insert("cursor".into(), cursor.into());
        }
        self.call("events/list", params, HeaderMap::new()).await
    }

    /// One `events/list` page for the bot, without the event types whose
    /// header annotations the transport forbids.
    pub async fn list(&self, cursor: Option<&str>) -> Result<Value, Failure> {
        let mut page = self.page(cursor).await?;
        if let Some(events) = page.get_mut("events").and_then(Value::as_array_mut) {
            events.retain(|event| {
                let valid = Descriptor::parse(event).is_some();
                if !valid {
                    let name: String = self
                        .redacted(event["name"].as_str().unwrap_or_default())
                        .chars()
                        .take(200)
                        .collect();
                    tracing::warn!(event = %name, "an MCP event type has invalid x-mcp-header annotations");
                }
                valid
            });
        }
        Ok(self.masked(page))
    }

    /// The descriptor of event type `name`, searched across pages. A type
    /// with invalid header annotations is absent.
    pub async fn descriptor(&self, name: &str) -> Result<Option<Descriptor>, Failure> {
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let page = self.page(cursor.as_deref()).await?;
            if let Some(found) = page["events"]
                .as_array()
                .and_then(|events| events.iter().find(|e| e["name"] == name))
            {
                return Ok(Descriptor::parse(found));
            }
            match page["nextCursor"].as_str() {
                Some(next) if !next.is_empty() => cursor = Some(next.to_owned()),
                _ => break,
            }
        }
        Ok(None)
    }

    fn subscription_params(
        subscription: &Subscription,
        cursor: Option<&str>,
    ) -> Map<String, Value> {
        let mut params = Map::new();
        params.insert("name".into(), subscription.name.clone().into());
        params.insert("arguments".into(), subscription.arguments.clone());
        params.insert("cursor".into(), cursor.map_or(Value::Null, Value::from));
        params
    }

    pub async fn poll(
        &self,
        subscription: &Subscription,
        descriptor: &Descriptor,
        cursor: Option<&str>,
    ) -> Result<Polled, Failure> {
        let mut params = Self::subscription_params(subscription, cursor);
        params.insert("maxEvents".into(), 100.into());
        let result = self
            .call("events/poll", params, descriptor.headers_for(subscription)?)
            .await?;
        let events = result["events"]
            .as_array()
            .ok_or_else(|| Failure::Transient("events/poll returned no events list.".into()))?
            .iter()
            .filter_map(|raw| self.occurrence(raw.clone()))
            .collect();
        Ok(Polled {
            events,
            cursor: result["cursor"].as_str().map(str::to_owned),
            truncated: result["truncated"] == true,
            has_more: result["hasMore"] == true,
            next_poll: Duration::from_millis(result["nextPollMs"].as_u64().unwrap_or(30_000))
                .max(MIN_POLL),
        })
    }

    /// Open an `events/stream` push stream. Dropping it aborts the request,
    /// which is how a Streamable HTTP client cancels a subscription.
    pub async fn stream(
        &self,
        subscription: &Subscription,
        descriptor: &Descriptor,
        cursor: Option<&str>,
    ) -> Result<PushStream<'_>, Failure> {
        let mut params = Self::subscription_params(subscription, cursor);
        params.insert(
            "heartbeatMs".into(),
            u64::try_from(SUGGESTED_HEARTBEAT.as_millis())
                .unwrap_or(u64::MAX)
                .into(),
        );
        let headers = descriptor.headers_for(subscription)?;
        let opening = async {
            let response = self.post("events/stream", params, headers).await?;
            if is_sse(&response) {
                return Ok(response);
            }
            let bytes = bounded_body(response).await?;
            let message: Value = serde_json::from_slice(&bytes).map_err(|_| {
                Failure::Unsupported("The MCP server did not open an event stream.".into())
            })?;
            Err(message
                .get("error")
                .map(|error| self.rpc_failure(error))
                .unwrap_or_else(|| Failure::Transient("The event stream closed at once.".into())))
        };
        let response = tokio::time::timeout(REQUEST_TIMEOUT, opening)
            .await
            .unwrap_or_else(|_| {
                Err(Failure::Transient(
                    "The event stream did not open in time.".into(),
                ))
            })?;
        Ok(PushStream {
            endpoint: self,
            frames: Frames::new(response),
        })
    }
}

impl Descriptor {
    /// `None` when an `x-mcp-header` annotation breaks the transport's
    /// rules: an empty or non-token name, one used twice, one on a value
    /// that is not a string, integer or boolean, or one outside a chain of
    /// object properties, such as under `items` or `anyOf`.
    fn parse(descriptor: &Value) -> Option<Self> {
        let offered = |mode: &str| {
            descriptor["delivery"]
                .as_array()
                .is_some_and(|modes| modes.iter().any(|m| m == mode))
        };
        let delivery = [(Delivery::Push, "push"), (Delivery::Poll, "poll")]
            .into_iter()
            .filter(|(_, mode)| offered(mode))
            .map(|(delivery, _)| delivery)
            .collect();
        let mut headers = Vec::new();
        annotated(&descriptor["inputSchema"], &mut Vec::new(), &mut headers)?;
        if annotations(&descriptor["inputSchema"]) != headers.len() {
            return None;
        }
        let mut names: Vec<String> = headers
            .iter()
            .map(|(_, h)| h.to_ascii_lowercase())
            .collect();
        names.sort();
        names.dedup();
        (names.len() == headers.len()).then_some(Self { delivery, headers })
    }

    /// `Mcp-Name` and the `Mcp-Param-*` headers the subscription's
    /// arguments call for (SEP-2243).
    fn headers_for(&self, subscription: &Subscription) -> Result<HeaderMap, Failure> {
        let mut headers = HeaderMap::new();
        let mut add = |name: String, value: &str| {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Failure::Rejected(format!("Invalid header name {name}.")))?;
            let value = HeaderValue::from_str(&header_value(value))
                .map_err(|_| Failure::Rejected("Invalid header value.".into()))?;
            headers.insert(name, value);
            Ok::<_, Failure>(())
        };
        add("Mcp-Name".into(), &subscription.name)?;
        for (path, header) in &self.headers {
            let value = path
                .iter()
                .try_fold(&subscription.arguments, |at, key| at.get(key));
            let value = match value {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Bool(flag)) => flag.to_string(),
                Some(Value::Number(number)) => number.to_string(),
                _ => continue,
            };
            add(format!("Mcp-Param-{header}"), &value)?;
        }
        Ok(headers)
    }
}

/// Collect the `x-mcp-header` annotations under `schema`'s properties,
/// nested objects included. `None` when one is invalid.
fn annotated(
    schema: &Value,
    path: &mut Vec<String>,
    found: &mut Vec<(Vec<String>, String)>,
) -> Option<()> {
    let Some(properties) = schema["properties"].as_object() else {
        return Some(());
    };
    for (property, schema) in properties {
        path.push(property.clone());
        if let Some(header) = schema.get("x-mcp-header") {
            let header = header.as_str()?;
            let primitive = schema
                .get("type")
                .is_none_or(|t| ["string", "integer", "boolean"].iter().any(|p| t == p));
            if header.is_empty() || !header.bytes().all(is_token) || !primitive {
                return None;
            }
            found.push((path.clone(), header.to_owned()));
        }
        annotated(schema, path, found)?;
        path.pop();
    }
    Some(())
}

fn annotations(schema: &Value) -> usize {
    match schema {
        Value::Object(map) => {
            usize::from(map.contains_key("x-mcp-header"))
                + map.values().map(annotations).sum::<usize>()
        }
        Value::Array(items) => items.iter().map(annotations).sum(),
        _ => 0,
    }
}

/// An RFC 9110 token character, the only kind a header name may hold.
fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// A header value as SEP-2243 sends it: plain when it is safe ASCII,
/// otherwise in the Base64 sentinel form.
fn header_value(value: &str) -> String {
    let plain = !value.is_empty()
        && value.bytes().all(|b| (0x20..0x7f).contains(&b))
        && !value.starts_with([' ', '\t'])
        && !value.ends_with([' ', '\t'])
        && !value.starts_with("=?base64?");
    if plain {
        value.to_owned()
    } else {
        format!(
            "=?base64?{}?=",
            base64::engine::general_purpose::STANDARD.encode(value)
        )
    }
}

fn is_sse(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim_start().starts_with("text/event-stream"))
}

async fn bounded_body(response: reqwest::Response) -> Result<Vec<u8>, Failure> {
    let mut body = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|_| Failure::Transient("The MCP response broke off.".into()))?;
        if bytes.len() + chunk.len() > MAX_BYTES {
            return Err(Failure::Transient(
                "The MCP response exceeds the 2 MiB limit.".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn too_large() -> Failure {
    Failure::Transient("An MCP stream message exceeds the 2 MiB limit.".into())
}

/// The JSON-RPC messages of an SSE body, each at most 2 MiB.
struct Frames {
    body: BoxStream<'static, reqwest::Result<axum::body::Bytes>>,
    buffer: Vec<u8>,
    data: String,
}

impl Frames {
    fn new(response: reqwest::Response) -> Self {
        Self {
            body: response.bytes_stream().boxed(),
            buffer: Vec::new(),
            data: String::new(),
        }
    }

    async fn next(&mut self) -> Result<Option<Value>, Failure> {
        loop {
            while let Some(end) = self.buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=end).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\n', '\r']);
                if line.is_empty() {
                    let data = std::mem::take(&mut self.data);
                    if data.is_empty() {
                        continue;
                    }
                    return serde_json::from_str(&data).map(Some).map_err(|_| {
                        Failure::Transient("The MCP stream sent a message that is not JSON.".into())
                    });
                }
                if let Some(value) = line.strip_prefix("data:") {
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    if self.data.len() + value.len() + 1 > MAX_BYTES {
                        return Err(too_large());
                    }
                    if !self.data.is_empty() {
                        self.data.push('\n');
                    }
                    self.data.push_str(value);
                }
            }
            if self.buffer.len() + self.data.len() > MAX_BYTES {
                return Err(too_large());
            }
            match self.body.next().await {
                Some(Ok(chunk)) => {
                    if self.buffer.len() + self.data.len() + chunk.len() > MAX_BYTES {
                        return Err(too_large());
                    }
                    self.buffer.extend_from_slice(&chunk);
                }
                Some(Err(_)) => {
                    return Err(Failure::Transient("The MCP stream broke off.".into()));
                }
                None => return Ok(None),
            }
        }
    }
}

pub struct PushStream<'a> {
    endpoint: &'a Endpoint,
    frames: Frames,
}

impl PushStream<'_> {
    /// The next message. `Rejected` when the server ends the subscription,
    /// `Transient` when the stream closes or breaks.
    pub async fn next(&mut self) -> Result<Pushed, Failure> {
        loop {
            let Some(message) = self.frames.next().await? else {
                return Err(Failure::Transient("The event stream closed.".into()));
            };
            if let Some(error) = message.get("error") {
                return Err(self.endpoint.rpc_failure(error));
            }
            if message.get("id").is_some() {
                return Err(Failure::Transient(
                    "The server ended the event stream.".into(),
                ));
            }
            let params = message["params"].clone();
            let cursor = params["cursor"].as_str().map(str::to_owned);
            match message["method"].as_str() {
                Some("notifications/events/active") => {
                    return Ok(Pushed::Active {
                        cursor,
                        truncated: params["truncated"] == true,
                        heartbeat: Duration::from_millis(
                            params["heartbeatMs"]
                                .as_u64()
                                .unwrap_or(SUGGESTED_HEARTBEAT.as_millis() as u64),
                        )
                        .max(MIN_POLL),
                    });
                }
                Some("notifications/events/event") => {
                    if let Some(event) = self.endpoint.occurrence(params) {
                        return Ok(Pushed::Event(event));
                    }
                }
                Some("notifications/events/heartbeat") => return Ok(Pushed::Heartbeat { cursor }),
                Some("notifications/events/error") => {
                    let failure = self.endpoint.rpc_failure(&params["error"]);
                    return Ok(Pushed::Error(failure.message().to_owned()));
                }
                Some("notifications/events/terminated") => {
                    return Err(match self.endpoint.rpc_failure(&params["error"]) {
                        Failure::Unsupported(m) | Failure::Rejected(m) => Failure::Rejected(m),
                        Failure::SchemaChanged => Failure::SchemaChanged,
                        other => Failure::Transient(other.message().to_owned()),
                    });
                }
                _ => {}
            }
        }
    }
}

pub fn matches(data: &Value, expected: &Map<String, Value>) -> bool {
    expected
        .iter()
        .all(|(pointer, value)| data.pointer(pointer) == Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_values_fall_back_to_base64_when_unsafe() {
        assert_eq!(header_value("thread.run_settled"), "thread.run_settled");
        assert_eq!(header_value("café"), "=?base64?Y2Fmw6k=?=");
        assert_eq!(header_value(" padded"), "=?base64?IHBhZGRlZA==?=");
    }

    #[test]
    fn a_descriptor_lists_usable_modes_push_first_and_annotated_headers() {
        let descriptor = Descriptor::parse(&json!({
            "name": "incident.created",
            "delivery": ["webhook", "poll", "push"],
            "inputSchema": { "type": "object", "properties": {
                "region": { "type": "string", "x-mcp-header": "Region" },
                "severity": { "type": "string" },
                "scope": { "type": "object", "properties": {
                    "tenant": { "type": "string", "x-mcp-header": "Tenant" },
                }},
            }},
        }))
        .unwrap();
        assert_eq!(descriptor.delivery, [Delivery::Push, Delivery::Poll]);
        let headers = descriptor
            .headers_for(&Subscription {
                name: "incident.created".into(),
                arguments: json!({ "region": "us-west1", "severity": "P1",
                                   "scope": { "tenant": "acme" } }),
            })
            .unwrap();
        assert_eq!(headers["mcp-name"], "incident.created");
        assert_eq!(headers["mcp-param-region"], "us-west1");
        assert_eq!(headers["mcp-param-tenant"], "acme");
        assert_eq!(headers.len(), 3);
        assert!(
            Descriptor::parse(&json!({ "delivery": ["webhook"] }))
                .unwrap()
                .delivery
                .is_empty()
        );
    }

    #[test]
    fn a_descriptor_with_an_invalid_header_annotation_is_unusable() {
        let with = |properties: Value| {
            Descriptor::parse(&json!({ "delivery": ["poll"],
                "inputSchema": { "type": "object", "properties": properties } }))
        };
        assert!(with(json!({ "a": { "type": "string", "x-mcp-header": "Bad Name" } })).is_none());
        assert!(with(json!({ "a": { "type": "array", "x-mcp-header": "List" } })).is_none());
        assert!(with(json!({ "a": { "type": "number", "x-mcp-header": "Ratio" } })).is_none());
        assert!(
            with(json!({ "a": { "type": "array",
                                "items": { "type": "string", "x-mcp-header": "Item" } } }))
            .is_none()
        );
        assert!(
            with(json!({ "a": { "type": "string", "x-mcp-header": "Region" },
                         "b": { "type": "string", "x-mcp-header": "region" } }))
            .is_none()
        );
    }

    #[test]
    fn a_remote_error_that_echoes_a_credential_is_masked() {
        let config = Transport::Http {
            secrets: Vec::new(),
            url: "http://127.0.0.1:9/mcp".into(),
            headers: std::collections::BTreeMap::from([(
                "Authorization".into(),
                "Bearer mcp-test-token".into(),
            )]),
        };
        let endpoint = Endpoint::new(&config, Vec::new(), CancellationToken::new()).unwrap();
        let failure = endpoint.rpc_failure(&json!({
            "code": -32024, "message": "Forbidden for mcp-test-token",
        }));
        let Failure::Rejected(message) = failure else {
            panic!("Forbidden refuses the subscription: {failure:?}");
        };
        assert!(!message.contains("mcp-test-token"), "{message}");
        let changed = endpoint.rpc_failure(&json!({
            "code": -32026, "message": "Unsupported", "data": { "reason": "schema_changed" },
        }));
        assert!(matches!(changed, Failure::SchemaChanged), "{changed:?}");
    }

    #[test]
    fn a_match_needs_every_pointer_to_equal_its_value() {
        let data = json!({ "threadId": "t1", "outcome": "completed" });
        let want = |v: Value| v.as_object().unwrap().clone();
        assert!(matches(&data, &want(json!({}))));
        assert!(matches(&data, &want(json!({ "/threadId": "t1" }))));
        assert!(!matches(&data, &want(json!({ "/threadId": "t2" }))));
        assert!(!matches(
            &data,
            &want(json!({ "/threadId": "t1", "/runId": "r" }))
        ));
    }
}
