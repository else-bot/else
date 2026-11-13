use std::io::{Read as _, Seek as _};
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::Config;
use crate::http::error::ApiError;

const OUTPUT_CAP: u64 = 64 * 1024;
const START_GRACE: u64 = 30;
const RUN_LIMIT: u64 = 2 * 60 * 60;
const WORKER: &str = include_str!("server_updates/worker.sh");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Version([u64; 3]);

impl Version {
    fn parse(value: &str) -> Option<Self> {
        let mut numbers = value.split('.');
        let mut version = [0; 3];
        for part in &mut version {
            let text = numbers.next()?;
            if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            *part = text.parse().ok()?;
            if text != part.to_string() {
                return None;
            }
        }
        numbers.next().is_none().then_some(Self(version))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub request_id: Uuid,
    pub version: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Snapshot {
    version: &'static str,
    unavailable: Option<String>,
    operation: Option<Operation>,
}

#[derive(Debug, Clone, Serialize)]
struct Operation {
    request_id: Uuid,
    version: String,
    state: State,
    output: String,
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Running,
    Succeeded,
    Failed,
}

#[derive(Serialize, Deserialize)]
struct Record {
    request_id: Uuid,
    version: String,
    started: u64,
}

pub(crate) struct Installation {
    home: Result<PathBuf, String>,
}

impl Default for Installation {
    fn default() -> Self {
        Self {
            home: Err("This server is managed outside the else.bot installer. Use its deployment's update procedure.".into()),
        }
    }
}

impl Installation {
    pub(crate) fn discover(config: &Config) -> Self {
        let identify = || -> Option<PathBuf> {
            let cwd = std::env::current_dir().ok()?.canonicalize().ok()?;
            if cwd.file_name()? != "config" {
                return None;
            }
            let home = cwd.parent()?.to_owned();
            let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
            let pointer = if home.join("server-current").symlink_metadata().is_ok() {
                "server-current"
            } else {
                "current"
            };
            if home
                .join(pointer)
                .join("bin/else-api")
                .canonicalize()
                .ok()?
                != exe
                || config.data_dir.canonicalize().ok()? != home.join("data").canonicalize().ok()?
                || config.owner_token_file.canonicalize().ok()?
                    != home.join("data/owner-token").canonicalize().ok()?
                || !home.join("bin/elsebot").is_file()
                || !home.join("src/install.sh").is_file()
            {
                return None;
            }
            Some(home)
        };
        identify().map_or_else(Self::default, |home| Self { home: Ok(home) })
    }

    pub(crate) fn snapshot(&self) -> Result<Snapshot, ApiError> {
        let home = match &self.home {
            Ok(home) => home,
            Err(why) => {
                return Ok(Snapshot {
                    version: crate::VERSION,
                    unavailable: Some(why.clone()),
                    operation: None,
                });
            }
        };
        let latest = home.join("run/server-updates/latest");
        let operation = match std::fs::read_to_string(&latest) {
            Ok(id) => {
                let id = Uuid::parse_str(id.trim()).map_err(|_| storage_error())?;
                Some(read_operation(home, id)?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(storage_error()),
        };
        Ok(Snapshot {
            version: crate::VERSION,
            unavailable: None,
            operation,
        })
    }

    pub(crate) async fn start(
        &mut self,
        request: Request,
        state: &crate::http::AppState,
    ) -> Result<Snapshot, ApiError> {
        let home = self.home.as_ref().map_err(|why| {
            ApiError::new(StatusCode::CONFLICT, "update_unavailable", why.clone())
        })?;
        let version = Version::parse(&request.version).ok_or_else(|| {
            ApiError::bad_request("invalid_version", "Use a stable X.Y.Z version.")
        })?;
        let root = home.join("run/server-updates");
        let job = root.join(request.request_id.to_string());
        if job.join("request.json").exists() {
            let operation = read_operation(home, request.request_id)?;
            if operation.version != request.version {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "request_conflict",
                    "This request already names a different version.",
                ));
            }
            return Ok(Snapshot {
                version: crate::VERSION,
                unavailable: None,
                operation: Some(operation),
            });
        }
        if self
            .snapshot()?
            .operation
            .is_some_and(|operation| operation.state == State::Running)
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "update_running",
                "A server update is already running.",
            ));
        }
        if version <= Version::parse(crate::VERSION).expect("workspace version is stable") {
            return Err(ApiError::bad_request(
                "invalid_version",
                "Choose a version newer than the running server.",
            ));
        }
        if state.scheduler.gate().is_closed() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "busy",
                "A backup or update is running. Try again once it has finished.",
            ));
        }
        crate::http::maintenance::ensure_idle(state).await?;
        private_directory(&root)?;
        private_directory(&job)?;
        let record = Record {
            request_id: request.request_id,
            version: request.version.clone(),
            started: now(),
        };
        write_private(
            &job.join("request.json"),
            &serde_json::to_vec(&record).map_err(|_| storage_error())?,
        )?;
        write_private(&job.join("worker.sh"), WORKER.as_bytes())?;
        write_private(
            &root.join("latest"),
            request.request_id.to_string().as_bytes(),
        )?;
        let mut command = tokio::process::Command::new(home.join("bin/elsebot"));
        command
            .args([
                "update-background",
                &request.request_id.to_string(),
                &request.version,
            ])
            .current_dir("/")
            .env_clear()
            .envs(std::env::vars_os().filter(|(name, _)| {
                let name = name.to_string_lossy();
                !(name.starts_with("ELSE_")
                    || name.starts_with("SECRET_KEY")
                    || name == "DATABASE_URL"
                    || name == "OWNER_TOKEN_FILE")
            }))
            .env("ELSE_HOME", home)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let launched = tokio::time::timeout(Duration::from_secs(15), command.status()).await;
        if matches!(launched, Ok(result) if !matches!(result, Ok(status) if status.success())) {
            write_private(&job.join("result"), b"127")?;
            write_private(&job.join("output.log"), b"The server could not start its background updater. Check the service manager and try again.\n")?;
        }
        self.snapshot()
    }
}

fn read_operation(home: &Path, id: Uuid) -> Result<Operation, ApiError> {
    let job = home.join("run/server-updates").join(id.to_string());
    let record: Record = serde_json::from_slice(
        &std::fs::read(job.join("request.json")).map_err(|_| storage_error())?,
    )
    .map_err(|_| storage_error())?;
    let output = tail(&job.join("output.log"))?;
    let elapsed = now().saturating_sub(record.started);
    let result = std::fs::read_to_string(job.join("result")).ok();
    let alive = std::fs::read_to_string(job.join("pid"))
        .ok()
        .and_then(|pid| pid.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 1)
        .and_then(rustix::process::Pid::from_raw)
        .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok());
    let (state, error) = match result.as_deref().map(str::trim) {
        Some("0") => (State::Succeeded, None),
        Some(_) => (State::Failed, Some("The installer failed. Review its output and try again.".into())),
        None if elapsed > RUN_LIMIT => (
            State::Failed,
            Some("The updater has not reported a result for two hours. Check the server before trying again.".into()),
        ),
        None if !alive && elapsed > START_GRACE => (
            State::Failed,
            Some("The background updater stopped before reporting its result. Check the server before trying again.".into()),
        ),
        None => (State::Running, None),
    };
    Ok(Operation {
        request_id: record.request_id,
        version: record.version,
        state,
        output,
        error,
    })
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn private_directory(path: &Path) -> Result<(), ApiError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|_| storage_error())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), ApiError> {
    crate::private_file::replace(path, bytes).map_err(|_| storage_error())
}

fn tail(path: &Path) -> Result<String, ApiError> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(_) => return Err(storage_error()),
    };
    let length = file.metadata().map_err(|_| storage_error())?.len();
    file.seek(std::io::SeekFrom::Start(length.saturating_sub(OUTPUT_CAP)))
        .map_err(|_| storage_error())?;
    let mut bytes = Vec::new();
    file.take(OUTPUT_CAP)
        .read_to_end(&mut bytes)
        .map_err(|_| storage_error())?;
    let mut output = String::from_utf8_lossy(&bytes).into_owned();
    if output.len() > OUTPUT_CAP as usize {
        let mut start = output.len() - OUTPUT_CAP as usize;
        while !output.is_char_boundary(start) {
            start += 1;
        }
        output = output.split_off(start);
    }
    Ok(output)
}

fn storage_error() -> ApiError {
    ApiError::unavailable(
        "update_storage",
        "The server could not read or save its update status.",
    )
}

#[cfg(test)]
mod tests;
