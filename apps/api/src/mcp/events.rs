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

use super::{Error, Transport, USER_AGENT};
use crate::db::task_secrets::{self, Redact};

pub const EXTENSION: &str = "io.modelcontextprotocol/events";
const PROTOCOL: &str = "2026-07-28";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_LIST_PAGES: usize = 20;
/// The shortest wait between polls, so a server cannot induce a tight loop.
pub const MIN_POLL: Duration = Duration::from_secs(1);
pub const SUGGESTED_HEARTBEAT: Duration = Duration::from_secs(30);

/// Why an events request did not succeed.
#[derive(Debug)]
pub enum Failure {
    /// The server does not offer events, or not over this transport.
    Unsupported(String),
    /// The server refused this subscription, and asking again cannot help.
    Rejected(String),
    /// A failure a later request may not repeat, such as a dropped
    /// connection or an expired token.
    Transient(String),
    /// The MCP library is paused for maintenance.
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
            Self::Paused => "The MCP library is paused for a backup or update.",
        }
    }
}

/// The JSON-RPC errors a subscription cannot recover from: invalid params,
/// and SEP-3415's NotFound, Forbidden and Unsupported.
fn rpc_failure(error: &Value) -> Failure {
    let code = error["code"].as_i64().unwrap_or_default();
    let message = format!(
        "{} ({code})",
        error["message"].as_str().unwrap_or("MCP error")
    );
    match code {
        -32601 => Failure::Unsupported(message),
        -32602 | -32023 | -32024 | -32026 => Failure::Rejected(message),
        _ => Failure::Transient(message),
    }
}

/// A delivery mode else.bot can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Push,
    Poll,
}

/// What a subscription listens for: an event type and its arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Subscription {
    pub name: String,
    pub arguments: Value,
}

/// An event type as `events/list` describes it, reduced to what a
/// subscription needs.
#[derive(Debug, Clone)]
pub struct Descriptor {
    /// Push first, as else.bot prefers it.
    pub delivery: Vec<Delivery>,
    /// Arguments mirrored into `Mcp-Param-*` headers, by property.
    headers: Vec<(String, String)>,
}

/// One event, kept whole for the bot, with the fields a wait reads.
#[derive(Debug, Clone)]
pub struct Occurrence {
    pub name: String,
    pub data: Value,
    pub cursor: Option<String>,
    pub raw: Value,
}

impl Occurrence {
    fn parse(mut raw: Value) -> Option<Self> {
        let object = raw.as_object_mut()?;
        object.remove("_meta");
        let name = object.get("name")?.as_str()?.to_owned();
        object.get("eventId")?.as_str()?;
        let data = object.get("data").filter(|d| d.is_object())?.clone();
        let cursor = object
            .remove("cursor")
            .and_then(|c| c.as_str().map(str::to_owned));
        Some(Self {
            name,
            data,
            cursor,
            raw,
        })
    }
}

/// One `events/poll` result.
#[derive(Debug)]
pub struct Polled {
    pub events: Vec<Occurrence>,
    pub cursor: Option<String>,
    pub truncated: bool,
    pub has_more: bool,
    pub next_poll: Duration,
}

/// A message on an `events/stream` push stream.
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
pub struct Endpoint {
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
    redact: Vec<Redact>,
}

impl Endpoint {
    /// `config` already holds the sign-in and filled task secrets; `redact`
    /// lists the values to keep out of logs and results.
    pub(super) fn new(config: &Transport, mut redact: Vec<Redact>) -> Result<Self, Error> {
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
        })
    }

    fn redacted(&self, text: &str) -> String {
        task_secrets::redact(text, &self.redact)
    }

    /// Remote data with any credential this endpoint sends masked out.
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
        if status == reqwest::StatusCode::UNAUTHORIZED || status.is_server_error() {
            return Err(Failure::Transient(format!(
                "The MCP server answered HTTP {status}."
            )));
        }
        Ok(response)
    }

    /// One request and its result. A response may arrive as JSON or as an
    /// SSE stream holding the response.
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
                return Err(rpc_failure(error));
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
            Err(Failure::Transient(message)) => Err(Failure::Transient(message)),
            Err(Failure::Paused) => Err(Failure::Paused),
            Err(Failure::Unsupported(_) | Failure::Rejected(_)) => Ok(false),
        }
    }

    /// One `events/list` page, for the bot.
    pub async fn list(&self, cursor: Option<&str>) -> Result<Value, Failure> {
        let mut params = Map::new();
        if let Some(cursor) = cursor {
            params.insert("cursor".into(), cursor.into());
        }
        let page = self.call("events/list", params, HeaderMap::new()).await?;
        Ok(self.masked(page))
    }

    /// The descriptor of event type `name`, searched across pages.
    pub async fn descriptor(&self, name: &str) -> Result<Option<Descriptor>, Failure> {
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let page = self.list(cursor.as_deref()).await?;
            if let Some(found) = page["events"]
                .as_array()
                .and_then(|events| events.iter().find(|e| e["name"] == name))
            {
                return Ok(Some(Descriptor::parse(found)));
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
            .filter_map(|raw| Occurrence::parse(self.masked(raw.clone())))
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
        let response = self
            .post(
                "events/stream",
                params,
                descriptor.headers_for(subscription)?,
            )
            .await?;
        if !is_sse(&response) {
            let bytes = bounded_body(response).await?;
            let message: Value = serde_json::from_slice(&bytes).map_err(|_| {
                Failure::Unsupported("The MCP server did not open an event stream.".into())
            })?;
            return Err(message
                .get("error")
                .map(rpc_failure)
                .unwrap_or_else(|| Failure::Transient("The event stream closed at once.".into())));
        }
        Ok(PushStream {
            endpoint: self,
            frames: Frames::new(response),
        })
    }
}

impl Descriptor {
    fn parse(descriptor: &Value) -> Self {
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
        let headers = descriptor["inputSchema"]["properties"]
            .as_object()
            .map(|properties| {
                properties
                    .iter()
                    .filter_map(|(property, schema)| {
                        Some((
                            property.clone(),
                            schema["x-mcp-header"].as_str()?.to_owned(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { delivery, headers }
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
        for (property, header) in &self.headers {
            let value = match &subscription.arguments[property] {
                Value::String(text) => text.clone(),
                Value::Bool(flag) => flag.to_string(),
                Value::Number(number) => number.to_string(),
                _ => continue,
            };
            add(format!("Mcp-Param-{header}"), &value)?;
        }
        Ok(headers)
    }
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

/// The JSON-RPC messages of an SSE body, each frame at most 2 MiB.
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

    /// The next message, or `None` when the stream ends.
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
                    if !self.data.is_empty() {
                        self.data.push('\n');
                    }
                    self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
                }
            }
            if self.buffer.len() + self.data.len() > MAX_BYTES {
                return Err(Failure::Transient(
                    "An MCP stream message exceeds the 2 MiB limit.".into(),
                ));
            }
            match self.body.next().await {
                Some(Ok(chunk)) => self.buffer.extend_from_slice(&chunk),
                Some(Err(_)) => {
                    return Err(Failure::Transient("The MCP stream broke off.".into()));
                }
                None => return Ok(None),
            }
        }
    }
}

/// An open push stream.
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
                return Err(rpc_failure(error));
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
                    if let Some(event) = Occurrence::parse(self.endpoint.masked(params)) {
                        return Ok(Pushed::Event(event));
                    }
                }
                Some("notifications/events/heartbeat") => return Ok(Pushed::Heartbeat { cursor }),
                Some("notifications/events/error") => {
                    let error = &params["error"];
                    return Ok(Pushed::Error(format!(
                        "{} ({})",
                        error["message"].as_str().unwrap_or("error"),
                        error["code"]
                    )));
                }
                Some("notifications/events/terminated") => {
                    let error = &params["error"];
                    return Err(Failure::Rejected(format!(
                        "The server ended the subscription: {} ({})",
                        error["message"].as_str().unwrap_or("terminated"),
                        error["code"]
                    )));
                }
                _ => {}
            }
        }
    }
}

/// Whether `data` holds each value its JSON pointer names.
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
            }},
        }));
        assert_eq!(descriptor.delivery, [Delivery::Push, Delivery::Poll]);
        let headers = descriptor
            .headers_for(&Subscription {
                name: "incident.created".into(),
                arguments: json!({ "region": "us-west1", "severity": "P1" }),
            })
            .unwrap();
        assert_eq!(headers["mcp-name"], "incident.created");
        assert_eq!(headers["mcp-param-region"], "us-west1");
        assert_eq!(headers.len(), 2);
        assert!(
            Descriptor::parse(&json!({ "delivery": ["webhook"] }))
                .delivery
                .is_empty()
        );
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
