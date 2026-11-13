//! One provider process tree, owned until its cleanup is verified.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStderr};
use tokio::sync::watch;
use uuid::Uuid;

use super::files::GRANT_ENV;
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

    /// Ask the process group to stop (SIGINT), as a terminal would.
    pub fn interrupt(&self) {
        self.process.signal(libc::SIGINT);
    }

    /// Stop the process tree and verify that nothing under the marker
    /// survived. Until this completes, dropping the scope still kills and
    /// sweeps it.
    pub async fn finish(mut self, how: Finish) -> Result<(), String> {
        if how == Finish::Exit {
            let _ = tokio::time::timeout(EXIT_GRACE, self.process.child.wait()).await;
        }
        self.process.kill().await;
        let swept = sweep(&self.host, self.marker).await;
        self.finished = true;
        swept
    }
}

/// Kill every remaining process carrying `marker` and wait until they exit.
pub async fn sweep(host: &Host, marker: Uuid) -> Result<(), String> {
    let host = host.clone();
    match tokio::task::spawn_blocking(move || host.sweep_turn(marker)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => Err(err.to_string()),
        Err(err) => Err(err.to_string()),
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

/// The tail of a process's stderr, collected in the background, with the
/// launch's tool grant hidden. Set once the pipe closes.
pub struct Stderr(watch::Receiver<Option<String>>);

impl Stderr {
    pub fn collect(stderr: ChildStderr, spec: &LaunchSpec) -> Self {
        let grant = spec
            .env
            .iter()
            .find(|(key, value)| key == GRANT_ENV && !value.is_empty())
            .map(|(_, value)| value.clone());
        let (done, tail) = watch::channel(None);
        tokio::spawn(async move {
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
            let mut text = String::from_utf8_lossy(&tail).into_owned();
            if let Some(grant) = grant {
                text = text.replace(&grant, "[tool grant]");
            }
            let _ = done.send(Some(text));
        });
        Self(tail)
    }

    /// The tail, waiting briefly for the pipe to close; empty if it stays
    /// open.
    pub async fn tail(&self) -> String {
        let mut tail = self.0.clone();
        match tokio::time::timeout(Duration::from_secs(2), tail.wait_for(Option::is_some)).await {
            Ok(Ok(text)) => text.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }
}

/// The last `count` non-empty lines of `text`, oldest first.
pub fn last_lines(text: &str, count: usize) -> Vec<&str> {
    let mut lines: Vec<&str> = text
        .lines()
        .rev()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(count)
        .collect();
    lines.reverse();
    lines
}
