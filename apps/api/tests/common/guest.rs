//! A fake bot computer for offline job tests.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use else_api::computers::{ComputerRuntime, ExecOutput, RuntimeError, RuntimeState, Spec};

/// A guest whose jobs launch at once and report `probe` until killed.
pub struct Guest {
    probe: Mutex<String>,
    state: Mutex<std::collections::HashMap<String, RuntimeState>>,
}

impl Guest {
    pub fn new() -> Self {
        Self {
            probe: Mutex::new("running\n".into()),
            state: Mutex::default(),
        }
    }

    pub fn report(&self, probe: &str) {
        *self.probe.lock().unwrap() = probe.into();
    }
}

#[async_trait]
impl ComputerRuntime for Guest {
    async fn list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .iter()
            .map(|(n, s)| (n.clone(), *s))
            .collect())
    }
    async fn create(&self, spec: &Spec) -> Result<(), RuntimeError> {
        self.state
            .lock()
            .unwrap()
            .insert(spec.name.clone(), RuntimeState::Running);
        Ok(())
    }
    async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        self.state
            .lock()
            .unwrap()
            .insert(name.into(), RuntimeState::Running);
        Ok(())
    }
    async fn stop(&self, name: &str) -> Result<(), RuntimeError> {
        self.state
            .lock()
            .unwrap()
            .insert(name.into(), RuntimeState::Stopped);
        Ok(())
    }
    async fn pause(&self, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn resume(&self, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn copy_out(&self, _: &str, _: &str, _: &std::path::Path) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn exec(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        self.exec_input(name, program, args, &[], timeout).await
    }
    async fn exec_input(
        &self,
        _: &str,
        _: &str,
        args: &[&str],
        _: &[u8],
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        let at = args.iter().position(|a| *a == "-c").map_or(0, |i| i + 1);
        let script = args.get(at).copied().unwrap_or_default();
        let stdout = if script.contains("echo launched") {
            "launched".to_owned()
        } else if script.contains("alive()") {
            "stopped".to_owned()
        } else if script.contains("echo running") {
            self.probe.lock().unwrap().clone()
        } else {
            String::new()
        };
        Ok(ExecOutput {
            status: Some(0),
            stdout,
            stderr: String::new(),
            truncated: false,
        })
    }
}
