//! One provider process tree, owned until its cleanup is verified.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr};
use uuid::Uuid;

use crate::providers::host::{Host, LaunchSpec, Supervised};

/// How long a process that reported its terminal result may take to exit
/// on its own before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(5);
const STDERR_TAIL: usize = 4096;

/// How a scope ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// The provider reported its terminal result: let it exit first.
    Exit,
    /// Stop everything now.
    Kill,
}

/// A provider process and everything started under its launch marker.
/// [`Scope::finish`] kills the process group, then every remaining process
/// carrying the marker (including descendants that left the group), and
/// reports whether any survived. A scope dropped without finishing is
/// killed and swept in the background.
pub struct Scope {
    host: Host,
    marker: Uuid,
    process: Supervised,
    finished: bool,
}

impl Scope {
    /// Spawn `spec` with piped stdout. Processes of one scope share
    /// `spec.turn_id` as their marker, so only one scope may run per marker.
    pub fn spawn(
        host: &Host,
        spec: &LaunchSpec,
        stdin: Stdio,
        stderr: Stdio,
    ) -> std::io::Result<Self> {
        let mut command = host.command(spec);
        command.stdin(stdin).stdout(Stdio::piped()).stderr(stderr);
        Ok(Self {
            host: host.clone(),
            marker: spec.turn_id,
            process: Supervised::spawn(command)?,
            finished: false,
        })
    }

    pub fn child(&mut self) -> &mut Child {
        &mut self.process.child
    }

    pub fn pid(&self) -> i32 {
        self.process.pid()
    }

    /// Ask the process group to stop (SIGINT), as a terminal would.
    pub fn interrupt(&self) {
        self.process.signal(libc::SIGINT);
    }

    pub async fn finish(mut self, how: Finish) -> Result<(), String> {
        self.finished = true;
        if how == Finish::Exit {
            let _ = tokio::time::timeout(EXIT_GRACE, self.process.child.wait()).await;
        }
        self.process.kill().await;
        let host = self.host.clone();
        let marker = self.marker;
        match tokio::task::spawn_blocking(move || host.sweep_turn(marker)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => Err(err.to_string()),
            Err(err) => Err(err.to_string()),
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.process.signal(libc::SIGKILL);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let host = self.host.clone();
            let marker = self.marker;
            runtime.spawn_blocking(move || {
                if let Err(err) = host.sweep_turn(marker) {
                    tracing::warn!(error = %err, "sweeping an abandoned provider process failed");
                }
            });
        }
    }
}

/// The tail of a process's stderr, collected in the background.
pub struct Stderr(tokio::task::JoinHandle<String>);

impl Stderr {
    pub fn collect(stderr: ChildStderr) -> Self {
        Self(tokio::spawn(async move {
            let mut stderr = stderr;
            let mut tail = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > STDERR_TAIL * 2 {
                    tail.drain(..tail.len() - STDERR_TAIL);
                }
            }
            String::from_utf8_lossy(&tail).into_owned()
        }))
    }

    /// The last non-empty line, waiting briefly for the pipe to close.
    pub async fn last_line(self) -> String {
        let text = tokio::time::timeout(Duration::from_secs(2), self.0)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        text.lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or_default()
            .to_owned()
    }
}
