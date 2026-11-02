//! A JSON-RPC client for `codex app-server` over stdio, running inside the
//! host sandbox. Responses are matched to requests by ID; notifications and
//! server-initiated requests (approvals, tool calls) are delivered in order
//! through [`AppServer::next`].

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use super::host::{HostSandbox, LaunchSpec, Supervised};

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

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, (i64, String)>>>>>;

/// A message from the server that is not a response to our request.
#[derive(Debug, Clone)]
pub enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// The server asks us something (an approval, a tool call). It must be
    /// answered with [`AppServer::respond`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

pub struct AppServer {
    process: Supervised,
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    pending: Pending,
    next_id: AtomicI64,
    incoming: mpsc::UnboundedReceiver<Incoming>,
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
        let stdin = process.child.stdin.take().ok_or(RpcError::Closed)?;
        let stdout = process.child.stdout.take().ok_or(RpcError::Closed)?;
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, incoming) = mpsc::unbounded_channel();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let method = message.get("method").and_then(Value::as_str);
                match (message.get("id"), method) {
                    (Some(id), None) => {
                        let Some(id) = id.as_i64() else { continue };
                        let waiter = reader_pending
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
                    (Some(id), Some(method)) => {
                        let _ = tx.send(Incoming::Request {
                            id: id.clone(),
                            method: method.to_owned(),
                            params: message.get("params").cloned().unwrap_or(Value::Null),
                        });
                    }
                    (None, Some(method)) => {
                        let _ = tx.send(Incoming::Notification {
                            method: method.to_owned(),
                            params: message.get("params").cloned().unwrap_or(Value::Null),
                        });
                    }
                    (None, None) => {}
                }
            }
            // EOF: fail every outstanding request.
            reader_pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear();
        });
        let server = Self {
            process,
            stdin: tokio::sync::Mutex::new(stdin),
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

    async fn write(&self, message: Value) -> Result<(), RpcError> {
        let mut line = message.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|_| RpcError::Closed)?;
        stdin.flush().await.map_err(|_| RpcError::Closed)
    }

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
        self.write(json!({ "id": id, "method": method, "params": params }))
            .await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err((code, message)))) => Err(RpcError::Remote {
                method: method.to_owned(),
                code,
                message,
            }),
            Ok(Err(_)) => Err(RpcError::Closed),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&id);
                Err(RpcError::Timeout(method.to_owned()))
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), RpcError> {
        self.write(json!({ "method": method, "params": params }))
            .await
    }

    /// Answer a server request with an error (used to refuse approvals and
    /// tool calls this POC does not offer).
    pub async fn refuse(&self, id: Value, message: &str) -> Result<(), RpcError> {
        self.write(json!({ "id": id, "error": { "code": -32000, "message": message } }))
            .await
    }

    /// The next notification or server request; `None` once the process
    /// has closed its output.
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
