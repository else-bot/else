//! A JSON-RPC client for `codex app-server` over stdio, running on the
//! host. Responses are matched to requests by ID; notifications are
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
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use super::host::{Host, LaunchSpec, Supervised};

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
const MAX_REQUESTS: usize = 64;
struct Pending {
    reply: Waiter,
    _slot: OwnedSemaphorePermit,
}
enum Command {
    Request {
        id: i64,
        message: Value,
        reply: Waiter,
        slot: OwnedSemaphorePermit,
    },
    Notify {
        message: Value,
        reply: oneshot::Sender<Result<(), RpcError>>,
    },
}

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
    /// The bot's project directory, for `cwd`.
    pub cwd: String,
    process: Supervised,
    commands: mpsc::Sender<Command>,
    slots: Arc<Semaphore>,
    next_id: AtomicI64,
    incoming: mpsc::Receiver<Incoming>,
}

async fn write_line(
    stdin: &mut (impl AsyncWrite + Unpin),
    message: &Value,
) -> Result<(), RpcError> {
    let mut line = message.to_string();
    line.push('\n');
    tokio::time::timeout(Duration::from_secs(10), async {
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await
    })
    .await
    .map_err(|_| RpcError::Closed)?
    .map_err(|_| RpcError::Closed)
}

impl AppServer {
    /// Start the server on the host and complete the initialize handshake.
    pub async fn start(host: &Host, spec: &LaunchSpec) -> Result<Self, RpcError> {
        let mut command = host.command(spec);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut process =
            Supervised::spawn(command).map_err(|err| RpcError::Spawn(err.to_string()))?;
        let stdin = process.child.stdin.take().ok_or(RpcError::Closed)?;
        let stdout = process.child.stdout.take().ok_or(RpcError::Closed)?;
        let (tx, incoming) = mpsc::channel(MAX_QUEUED);
        let (commands, command_rx) = mpsc::channel(MAX_REQUESTS);
        tokio::spawn(io_loop(stdout, stdin, command_rx, tx));
        let server = Self {
            cwd: spec.dirs.project.to_string_lossy().into_owned(),
            process,
            commands,
            slots: Arc::new(Semaphore::new(MAX_REQUESTS)),
            next_id: AtomicI64::new(1),
            incoming,
        };
        server
            .request(
                "initialize",
                json!({ "clientInfo": { "name": "else_bot", "version": env!("ELSE_VERSION") } }),
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
        let exchange = async {
            let slot = self
                .slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| RpcError::Closed)?;
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            self.commands
                .send(Command::Request {
                    id,
                    message: json!({ "id": id, "method": method, "params": params }),
                    reply: tx,
                    slot,
                })
                .await
                .map_err(|_| RpcError::Closed)?;
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
        tokio::time::timeout(Duration::from_secs(10), async {
            let (tx, rx) = oneshot::channel();
            self.commands
                .send(Command::Notify {
                    message: json!({ "method": method, "params": params }),
                    reply: tx,
                })
                .await
                .map_err(|_| RpcError::Closed)?;
            rx.await.map_err(|_| RpcError::Closed)?
        })
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

    /// Terminate the server's process group and wait for it to exit.
    pub async fn shutdown(mut self, grace: Duration) {
        self.process.terminate(grace).await;
    }
}

async fn io_loop(
    stdout: impl AsyncRead + Unpin + Send + 'static,
    mut stdin: impl AsyncWrite + Unpin,
    mut commands: mpsc::Receiver<Command>,
    tx: mpsc::Sender<Incoming>,
) {
    let (wire_tx, mut wire) = mpsc::channel(MAX_QUEUED);
    let reader = tokio::spawn(read_loop(stdout, wire_tx));
    let mut pending = HashMap::<i64, Pending>::new();
    let mut cleanup = tokio::time::interval(Duration::from_millis(100));
    loop {
        pending.retain(|_, waiter| !waiter.reply.is_closed());
        let message = tokio::select! {
            _ = tx.closed() => break,
            _ = cleanup.tick() => continue,
            command = commands.recv() => {
                match command {
                    None => break,
                    Some(Command::Request { id, message, reply, slot }) => {
                        if reply.is_closed() { continue; }
                        pending.insert(id, Pending { reply, _slot: slot });
                        if write_line(&mut stdin, &message).await.is_err() { break; }
                    }
                    Some(Command::Notify { message, reply }) => {
                        if reply.is_closed() { continue; }
                        let result = write_line(&mut stdin, &message).await;
                        let failed = result.is_err();
                        let _ = reply.send(result);
                        if failed { break; }
                    }
                }
                continue;
            }
            message = wire.recv() => match message { Some(message) => message, None => break },
        };
        let method = message.get("method").and_then(Value::as_str);
        let delivered = match (message.get("id"), method) {
            (Some(id), None) => {
                if let Some(waiter) = id.as_i64().and_then(|id| pending.remove(&id)) {
                    let result = match message.get("error") {
                        Some(error) => Err((
                            error["code"].as_i64().unwrap_or_default(),
                            error["message"].as_str().unwrap_or_default().to_owned(),
                        )),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = waiter.reply.send(result);
                }
                true
            }
            (Some(id), Some(method)) => {
                let refusal = json!({ "id": id, "error": { "code": -32000, "message": "not available in else.bot" } });
                if write_line(&mut stdin, &refusal).await.is_err() {
                    break;
                }
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
    reader.abort();
}

async fn read_loop(stdout: impl AsyncRead + Unpin, tx: mpsc::Sender<Value>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut reader)
            .take(MAX_LINE + 1)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(0) | Err(_) => break,
            Ok(_) if line.len() as u64 > MAX_LINE => {
                tracing::warn!("provider sent an oversized message; closing it");
                break;
            }
            Ok(_) => {}
        }
        if let Ok(message) = serde_json::from_slice::<Value>(&line) {
            if tx.try_send(message).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn actor_refuses_server_requests_while_waiting_for_a_response() {
        let (client, server) = tokio::io::duplex(4096);
        let (input, output) = tokio::io::split(client);
        let (commands, rx) = mpsc::channel(MAX_REQUESTS);
        let (events, mut incoming) = mpsc::channel(MAX_QUEUED);
        let actor = tokio::spawn(io_loop(input, output, rx, events));
        let slots = Arc::new(Semaphore::new(1));
        let (reply, response) = oneshot::channel();
        commands
            .send(Command::Request {
                id: 1,
                message: json!({"id": 1, "method": "test"}),
                reply,
                slot: slots.clone().acquire_owned().await.unwrap(),
            })
            .await
            .unwrap_or_else(|_| panic!("actor running"));
        let (read, mut write) = tokio::io::split(server);
        let mut read = BufReader::new(read).lines();
        let sent: Value = serde_json::from_str(&read.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(sent["id"], 1);
        write
            .write_all(b"{\"id\":99,\"method\":\"approval\"}\n")
            .await
            .unwrap();
        let refusal = tokio::time::timeout(Duration::from_secs(1), read.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&refusal).unwrap()["error"]["code"],
            -32000
        );
        write
            .write_all(b"{\"id\":1,\"result\":\"ok\"}\n")
            .await
            .unwrap();
        assert_eq!(response.await.unwrap().unwrap(), json!("ok"));
        assert!(
            matches!(incoming.recv().await, Some(Incoming::Refused { method }) if method == "approval")
        );
        assert_eq!(slots.available_permits(), 1);
        drop(commands);
        tokio::time::timeout(Duration::from_secs(1), actor)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn cancelled_requests_release_in_flight_capacity_without_a_response() {
        let (client, server) = tokio::io::duplex(4096);
        let (input, output) = tokio::io::split(client);
        let (commands, rx) = mpsc::channel(MAX_REQUESTS);
        let (events, _incoming) = mpsc::channel(MAX_QUEUED);
        let actor = tokio::spawn(io_loop(input, output, rx, events));
        let slots = Arc::new(Semaphore::new(1));
        let (reply, response) = oneshot::channel();
        commands
            .send(Command::Request {
                id: 1,
                message: json!({"id": 1, "method": "never_answers"}),
                reply,
                slot: slots.clone().acquire_owned().await.unwrap(),
            })
            .await
            .unwrap_or_else(|_| panic!("actor running"));
        let mut read = BufReader::new(server).lines();
        read.next_line().await.unwrap().unwrap();
        assert!(
            slots.try_acquire().is_err(),
            "outstanding request holds capacity"
        );
        drop(response);
        let permit = tokio::time::timeout(Duration::from_secs(1), slots.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        drop(commands);
        actor.await.unwrap();
    }
}
