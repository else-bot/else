//! One attempt of a CLI that streams JSON events on stdout for one turn
//! and exits (Claude Code's and Grok's print modes).

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};

use super::attempt::{self, Channel, Ended, Protocol};
use super::lines::JsonLines;
use super::process::{Finish, Scope, Stderr};
use super::session::PreparedTurn;
use crate::app::providers::{Stop, TurnOutcome, TurnSink};
use crate::providers::host::{Host, LaunchSpec};

const PIPE_TIMEOUT: Duration = Duration::from_secs(30);

/// How the CLI is asked to stop its turn.
pub enum Interrupt {
    /// A control message on its input.
    Line(String),
    /// SIGINT to its process group.
    Signal,
}

pub struct StreamAttempt {
    scope: Scope,
    stdin: Option<ChildStdin>,
    events: JsonLines<BufReader<ChildStdout>>,
    stderr: Stderr,
    interrupt: Interrupt,
    label: &'static str,
}

impl StreamAttempt {
    /// Start the CLI. With [`Interrupt::Line`] its stdin stays open for
    /// the turn's input and control messages.
    pub fn spawn(
        host: &Host,
        spec: &LaunchSpec,
        interrupt: Interrupt,
        label: &'static str,
    ) -> std::io::Result<Self> {
        let stdin = match interrupt {
            Interrupt::Line(_) => Stdio::piped(),
            Interrupt::Signal => Stdio::null(),
        };
        let mut scope = Scope::spawn(host, spec, stdin, Stdio::piped())?;
        let child = scope.child();
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("no stderr"))?;
        Ok(Self {
            scope,
            stdin,
            events: JsonLines::new(BufReader::new(stdout), label),
            stderr: Stderr::collect(stderr),
            interrupt,
            label,
        })
    }

    /// Write one line to the CLI's input.
    pub async fn write(&mut self, line: &str) -> std::io::Result<()> {
        let pipe = self
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::other("no stdin"))?;
        tokio::time::timeout(PIPE_TIMEOUT, async {
            pipe.write_all(line.as_bytes()).await?;
            pipe.flush().await
        })
        .await
        .unwrap_or_else(|_| Err(std::io::Error::other("the CLI stopped reading its input")))
    }

    /// Run the turn to its end with `protocol`.
    pub async fn drive<P: Protocol<Message = Value>>(
        &mut self,
        protocol: &mut P,
        turn: &mut PreparedTurn<'_>,
        sink: &dyn TurnSink,
        stop: &Stop,
    ) -> Ended {
        let label = self.label;
        attempt::drive(self, protocol, turn, sink, stop, label).await
    }

    pub async fn conclude(self, ended: Ended, sink: &dyn TurnSink) -> TurnOutcome {
        drop(self.stdin);
        attempt::conclude(ended, self.scope, Some(self.stderr), sink, self.label).await
    }

    /// End an attempt that reported nothing to a sink: kill and sweep it.
    pub async fn close(self) -> Result<(), String> {
        drop(self.stdin);
        self.scope.finish(Finish::Kill).await
    }
}

#[async_trait]
impl Channel for StreamAttempt {
    type Message = Value;

    async fn next(&mut self) -> Result<Option<Value>, String> {
        self.events.next().await.map_err(|e| e.to_string())
    }

    async fn interrupt(&mut self) {
        match &self.interrupt {
            Interrupt::Line(line) => {
                let line = line.clone();
                let _ = self.write(&line).await;
            }
            Interrupt::Signal => self.scope.interrupt(),
        }
    }
}
