//! A scriptable fake bot computer for offline tests.
//!
//! Every computer it creates boots at once and records its lifecycle. Each
//! command goes first to the [`Guest::on_exec`] hook. Commands the hook
//! passes on get the guest job protocol: a launch answers `launched`, a kill
//! answers `stopped`, a probe answers what [`Guest::report`] last set, and
//! anything else succeeds with no output.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use else_api::computers::{ComputerRuntime, ExecOutput, RuntimeError, RuntimeState, Spec};
use futures_util::future::BoxFuture;

/// One command run in a guest.
#[derive(Clone, Debug)]
pub struct Exec {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    /// Whether the command was given a standard input, even an empty one.
    pub stdin: bool,
    pub input: Vec<u8>,
}

impl Exec {
    fn script_at(&self) -> usize {
        self.args
            .iter()
            .position(|a| a == "-c")
            .map_or(0, |i| i + 1)
    }

    /// The shell script after `-c`, or the first argument.
    pub fn script(&self) -> &str {
        self.arg(0).unwrap_or_default()
    }

    /// The argument `i` places after the script.
    pub fn arg(&self, i: usize) -> Option<&str> {
        self.args.get(self.script_at() + i).map(String::as_str)
    }

    pub fn has_arg(&self, arg: &str) -> bool {
        self.args.iter().any(|a| a == arg)
    }
}

pub type Reply = Result<ExecOutput, RuntimeError>;

/// A successful reply with `stdout`.
pub fn ok(stdout: impl Into<String>) -> Reply {
    Ok(ExecOutput {
        status: Some(0),
        stdout: stdout.into(),
        stderr: String::new(),
        truncated: false,
    })
}

type Hook = Box<dyn Fn(Exec) -> BoxFuture<'static, Option<Reply>> + Send + Sync>;

pub struct Guest {
    computers: Mutex<HashMap<String, RuntimeState>>,
    events: Mutex<Vec<String>>,
    execs: Mutex<Vec<Exec>>,
    configured: Mutex<Vec<u32>>,
    copied: Mutex<Vec<String>>,
    file: Vec<u8>,
    probe: Mutex<String>,
    on_exec: Option<Hook>,
}

impl Guest {
    pub fn new() -> Self {
        Self {
            computers: Mutex::default(),
            events: Mutex::default(),
            execs: Mutex::default(),
            configured: Mutex::default(),
            copied: Mutex::default(),
            file: Vec::new(),
            probe: Mutex::new("running\n".into()),
            on_exec: None,
        }
    }

    /// Answer each command `hook` returns a reply for; the job protocol
    /// answers the rest.
    pub fn on_exec<F, Fut>(mut self, hook: F) -> Self
    where
        F: Fn(Exec) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<Reply>> + Send + 'static,
    {
        self.on_exec = Some(Box::new(move |exec| Box::pin(hook(exec))));
        self
    }

    /// Write `bytes` to the host path of every file copied out.
    pub fn with_file(mut self, bytes: Vec<u8>) -> Self {
        self.file = bytes;
        self
    }

    /// What a job probe answers from now on.
    pub fn report(&self, probe: &str) {
        *self.probe.lock().unwrap() = probe.into();
    }

    /// Each lifecycle change, as `"<state> <name>"`, in order.
    pub fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    pub fn execs(&self) -> Vec<Exec> {
        self.execs.lock().unwrap().clone()
    }

    /// The boot memory each `configure` set, in order.
    pub fn configured(&self) -> Vec<u32> {
        self.configured.lock().unwrap().clone()
    }

    /// The guest paths copied out, in order.
    pub fn copied(&self) -> Vec<String> {
        self.copied.lock().unwrap().clone()
    }

    /// The arguments and input of each job launch, in order.
    pub fn launches(&self) -> Vec<(Vec<String>, Vec<u8>)> {
        self.execs()
            .into_iter()
            .filter(|e| e.script().contains("echo launched"))
            .map(|e| (e.args, e.input))
            .collect()
    }

    fn set(&self, name: &str, state: RuntimeState) -> Result<(), RuntimeError> {
        self.events
            .lock()
            .unwrap()
            .push(format!("{state:?} {name}"));
        self.computers.lock().unwrap().insert(name.into(), state);
        Ok(())
    }

    async fn run(&self, name: &str, program: &str, args: &[&str], input: Option<&[u8]>) -> Reply {
        let exec = Exec {
            name: name.into(),
            program: program.into(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            stdin: input.is_some(),
            input: input.unwrap_or_default().to_vec(),
        };
        self.execs.lock().unwrap().push(exec.clone());
        if let Some(hook) = &self.on_exec
            && let Some(reply) = hook(exec.clone()).await
        {
            return reply;
        }
        self.job_protocol(&exec)
    }

    fn job_protocol(&self, exec: &Exec) -> Reply {
        let script = exec.script();
        if script.contains("echo launched") {
            ok("launched")
        } else if script.contains("alive()") {
            ok("stopped")
        } else if script.contains("echo running") {
            ok(self.probe.lock().unwrap().clone())
        } else {
            ok("")
        }
    }
}

#[async_trait]
impl ComputerRuntime for Guest {
    async fn list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError> {
        Ok(self
            .computers
            .lock()
            .unwrap()
            .iter()
            .map(|(n, s)| (n.clone(), *s))
            .collect())
    }
    async fn create(&self, spec: &Spec) -> Result<(), RuntimeError> {
        self.set(&spec.name, RuntimeState::Running)
    }
    async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        self.set(name, RuntimeState::Running)
    }
    async fn stop(&self, name: &str) -> Result<(), RuntimeError> {
        self.set(name, RuntimeState::Stopped)
    }
    async fn pause(&self, name: &str) -> Result<(), RuntimeError> {
        self.set(name, RuntimeState::Paused)
    }
    async fn resume(&self, name: &str) -> Result<(), RuntimeError> {
        self.set(name, RuntimeState::Running)
    }
    async fn configure(
        &self,
        _: &str,
        _: u32,
        memory_mib: u32,
        _: u32,
        _: u32,
    ) -> Result<(), RuntimeError> {
        self.configured.lock().unwrap().push(memory_mib);
        Ok(())
    }
    async fn copy_out(
        &self,
        _: &str,
        guest: &str,
        host: &std::path::Path,
    ) -> Result<(), RuntimeError> {
        self.copied.lock().unwrap().push(guest.to_owned());
        if self.file.is_empty() {
            return Ok(());
        }
        std::fs::write(host, &self.file).map_err(|e| RuntimeError::Unavailable(e.to_string()))
    }
    async fn exec(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        self.run(name, program, args, None).await
    }
    async fn exec_input(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        input: &[u8],
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        self.run(name, program, args, Some(input)).await
    }
}
