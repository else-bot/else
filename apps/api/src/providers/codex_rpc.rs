//! A JSON-RPC client for `codex app-server` over stdio, running inside the
//! host sandbox. Responses are matched to requests by ID; notifications are
//! delivered in order through [`AppServer::next`].
//!
//! Server-initiated requests (approvals, tool calls, input requests) are
//! refused immediately by the reader itself, so a request waiting for a
//! response can never deadlock behind one; the refusal is reported to the
//! caller as [`Incoming::Refused`]. Provider output is bounded: a line over
//! [`MAX_LINE`] bytes or a backlog over [`MAX_QUEUED`] messages closes the
//! connection rather than growing API memory.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot};

use super::host::{HostSandbox, LaunchSpec, Supervised};

const MAX_LINE: u64 = 8 * 1024 * 1024;
const MAX_QUEUED: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("could not start the provider: {0}")]
    Spawn(String),
    #[error("the provider process exited")]
    Closed,
    #[error("the provider did not answer {0} in time")]
    Timeout(String),
    #[error("{method} failed: {message} ({code})")]
    Remote {
        method: String,
        code: i64,
        message: String,
    },
}

type Waiter = oneshot::Sender<Result<Value, (i64, String)>>;
type Pending = Arc<Mutex<HashMap<i64, Waiter>>>;
type Stdin = Arc<tokio::sync::Mutex<Option<ChildStdin>>>;

/// A message from the server that is not a response to our request.
#[derive(Debug, Clone)]
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// The server asked for something (an approval, a tool call) and was
    /// refused; nothing it asked for happened.
    Refused {
        method: String,
    },
}

pub struct AppServer {
    process: Supervised,
    stdin: Stdin,
    pending: Pending,
    next_id: AtomicI64,
    incoming: mpsc::Receiver<Incoming>,
}

/// Removes a pending entry however the request ends, including when the
/// calling future is dropped.
struct PendingGuard<'a> {
    pending: &'a Pending,
    id: i64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}

async fn write_line(stdin: &Stdin, message: &Value) -> Result<(), RpcError> {
    let mut line = message.to_string();
    line.push('\n');
    let mut guard = stdin.lock().await;
    let Some(pipe) = guard.as_mut() else {
        return Err(RpcError::Closed);
    };
    let written = async {
        pipe.write_all(line.as_bytes()).await?;
        pipe.flush().await
    }
    .await;
    if written.is_err() {
        // A partial write leaves the stream unusable; close it.
        *guard = None;
        return Err(RpcError::Closed);
    }
    Ok(())
}

impl AppServer {
    /// Start the server in the sandbox and complete the initialize handshake.
    pub async fn start(sandbox: &HostSandbox, spec: &LaunchSpec) -> Result<Self, RpcError> {
        let mut command = sandbox.command(spec);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Provider logs can contain request details; they are not kept.
            .stderr(Stdio::null());
        let mut process =
            Supervised::spawn(command).map_err(|err| RpcError::Spawn(err.to_string()))?;
        let stdin: Stdin = Arc::new(tokio::sync::Mutex::new(process.child.stdin.take()));
        let stdout = process.child.stdout.take().ok_or(RpcError::Closed)?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, incoming) = mpsc::channel(MAX_QUEUED);
        tokio::spawn(read_loop(stdout, pending.clone(), stdin.clone(), tx));
        let server = Self {
            process,
            stdin,
            pending,
            next_id: AtomicI64::new(1),
            incoming,
        };
        server
            .request(
                "initialize",
                json!({ "clientInfo": { "name": "else_bot", "version": env!("CARGO_PKG_VERSION") } }),
                Duration::from_secs(30),
            )
            .await?;
        server.notify("initialized", json!({})).await?;
        Ok(server)
    }

    /// Send a request and wait for its response. The deadline covers
    /// writing as well as waiting.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        let _guard = PendingGuard {
            pending: &self.pending,
            id,
        };
        let exchange = async {
            write_line(
                &self.stdin,
                &json!({ "id": id, "method": method, "params": params }),
            )
            .await?;
            match rx.await {
                Ok(Ok(result)) => Ok(result),
                Ok(Err((code, message))) => Err(RpcError::Remote {
                    method: method.to_owned(),
                    code,
                    message,
                }),
                Err(_) => Err(RpcError::Closed),
            }
        };
        tokio::time::timeout(timeout, exchange)
            .await
            .map_err(|_| RpcError::Timeout(method.to_owned()))?
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        tokio::time::timeout(
            Duration::from_secs(10),
            write_line(&self.stdin, &json!({ "method": method, "params": params })),
        )
        .await
        .map_err(|_| RpcError::Timeout(method.to_owned()))?
    }

    /// The next notification or refused request; `None` once the process
    /// has closed its output or exceeded the output bounds.
    pub async fn next(&mut self) -> Option<Incoming> {
        self.incoming.recv().await
    }

    pub fn pid(&self) -> i32 {
        self.process.pid()
    }

    /// Terminate the sandbox and wait for it to exit.
    pub async fn shutdown(mut self, grace: Duration) {
        self.process.terminate(grace).await;
    }
}

async fn read_loop(
    stdout: tokio::process::ChildStdout,
    pending: Pending,
    stdin: Stdin,
    tx: mpsc::Sender<Incoming>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut reader)
            .take(MAX_LINE + 1)
            .read_until(b'\n', &mut line)
            .await;
        match read {
            Ok(0) | Err(_) => break,
            Ok(_) if line.len() as u64 > MAX_LINE => {
                tracing::warn!("provider sent an oversized message; closing it");
                break;
            }
            Ok(_) => {}
        }
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let method = message.get("method").and_then(Value::as_str);
        let delivered = match (message.get("id"), method) {
            (Some(id), None) => {
                if let Some(id) = id.as_i64() {
                    let waiter = pending
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&id);
                    if let Some(waiter) = waiter {
                        let result = match message.get("error") {
                            Some(error) => Err((
                                error["code"].as_i64().unwrap_or_default(),
                                error["message"].as_str().unwrap_or_default().to_owned(),
                            )),
                            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = waiter.send(result);
                    }
                }
                true
            }
            (Some(id), Some(method)) => {
                // Nothing the server can ask for is offered: refuse at once.
                let refusal = json!({
                    "id": id,
                    "error": { "code": -32000, "message": "not available in else.bot" },
                });
                let _ = write_line(&stdin, &refusal).await;
                tx.try_send(Incoming::Refused {
                    method: method.to_owned(),
                })
                .is_ok()
            }
            (None, Some(method)) => tx
                .try_send(Incoming::Notification {
                    method: method.to_owned(),
                    params: message.get("params").cloned().unwrap_or(Value::Null),
                })
                .is_ok(),
            (None, None) => true,
        };
        if !delivered {
            tracing::warn!("provider output backlog exceeded; closing it");
            break;
        }
    }
    // Closing fails every outstanding request with `Closed`.
    pending.lock().unwrap_or_else(|p| p.into_inner()).clear();
    *stdin.lock().await = None;
}
