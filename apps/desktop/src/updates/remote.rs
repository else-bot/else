use super::*;
use serde::Deserialize;
use uuid::Uuid;

const ENDPOINT: &str = "/v1/maintenance/update/installation";
const RECONNECT_LIMIT: Duration = Duration::from_secs(180);

#[derive(Deserialize)]
struct Status {
    version: String,
    unavailable: Option<String>,
    operation: Option<RemoteOperation>,
}

#[derive(Deserialize)]
struct RemoteOperation {
    request_id: Uuid,
    version: String,
    state: RemoteState,
    output: String,
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RemoteState {
    Running,
    Succeeded,
    Failed,
}

struct Remote {
    client: Arc<Client>,
    settings: Settings,
    state: watch::Sender<Snapshot>,
    pending: Arc<AtomicBool>,
    latest: Option<Version>,
    last_response: tokio::time::Instant,
}

pub fn start(
    client: Arc<Client>,
    runtime: &tokio::runtime::Handle,
) -> (Commands, watch::Receiver<Snapshot>) {
    start_with(client, Settings::default(), runtime)
}

fn start_with(
    client: Arc<Client>,
    settings: Settings,
    runtime: &tokio::runtime::Handle,
) -> (Commands, watch::Receiver<Snapshot>) {
    let (tx, commands) = mpsc::channel(4);
    let (state_tx, state) = watch::channel(Snapshot::initial());
    let pending = Arc::new(AtomicBool::new(false));
    runtime.spawn(
        Remote {
            client,
            settings,
            state: state_tx,
            pending: pending.clone(),
            latest: None,
            last_response: tokio::time::Instant::now(),
        }
        .run(commands),
    );
    (
        Commands {
            tx,
            state: state.clone(),
            pending,
            component: Component::Server,
        },
        state,
    )
}

impl Remote {
    async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        self.check().await;
        let mut check = tokio::time::interval(self.settings.check_every);
        let mut work = tokio::time::interval(self.settings.work_every);
        let mut progress = tokio::time::interval(Duration::from_millis(500));
        for timer in [&mut check, &mut work, &mut progress] {
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            timer.tick().await;
        }
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::Check) => self.check().await,
                    Some(Command::Install) => {
                        self.install().await;
                        self.pending.store(false, Ordering::Release);
                    }
                    Some(Command::Restart) => {},
                    None => return,
                },
                _ = check.tick(), if !self.state.borrow().running() => self.check().await,
                _ = work.tick(), if !self.state.borrow().running() => {
                    self.poll().await;
                    self.work().await;
                },
                _ = progress.tick(), if self.state.borrow().running() => self.poll().await,
                () = self.state.closed() => return,
            }
        }
    }

    async fn status(&self) -> Result<Status, ClientError> {
        self.client
            .send_json(
                self.client
                    .request(Method::GET, ENDPOINT)
                    .timeout(self.settings.work_limit),
            )
            .await
            .map(|(_, status)| status)
    }

    async fn check(&mut self) {
        if self.state.borrow().running() {
            return;
        }
        self.state
            .send_modify(|state| state.check = Check::Checking);
        let env = service_env(std::env::vars_os(), Path::new("/"));
        let (latest, status) = tokio::join!(
            release::latest(
                &self.settings.git,
                &self.settings.repo,
                env,
                self.settings.check_limit
            ),
            self.status(),
        );
        self.state.send_modify(|state| {
            state.latest = latest.as_ref().ok().map(ToString::to_string);
        });
        match latest {
            Ok(latest) => self.latest = Some(latest),
            Err(error) => {
                self.latest = None;
                self.state
                    .send_modify(|state| state.check = Check::Failed(error));
            }
        }
        match status {
            Ok(status) => self.apply(status),
            Err(error) => self.failed_status(error),
        }
        self.work().await;
    }

    async fn work(&mut self) {
        let result = self
            .client
            .send_json::<Report>(
                self.client
                    .request(Method::GET, "/v1/maintenance/update")
                    .timeout(self.settings.work_limit),
            )
            .await;
        let work = match result {
            Ok((_, report)) if report.active_turns == 0 && report.running_commands == 0 => {
                Work::Idle
            }
            Ok((_, report)) => Work::Busy {
                turns: report.active_turns,
                commands: report.running_commands,
            },
            Err(error) => {
                Work::Unavailable(format!("Cannot check this server's ongoing work. {error}"))
            }
        };
        self.state.send_modify(|state| state.work = work);
    }

    async fn poll(&mut self) {
        match self.status().await {
            Ok(status) => self.apply(status),
            Err(error) => self.failed_status(error),
        }
    }

    fn apply(&mut self, status: Status) {
        self.last_response = tokio::time::Instant::now();
        let installed = status.version.parse::<Version>();
        let check = self.latest.map(|latest| match installed {
            Ok(installed) if installed < latest => Check::Available {
                latest: latest.to_string(),
            },
            Ok(_) => Check::Current {
                latest: latest.to_string(),
            },
            Err(_) => Check::Failed("The server reported an invalid version.".into()),
        });
        let operation = status.operation.map(|operation| {
            let mut log = output::Log::new(OUTPUT_CAP);
            log.push(operation.output.as_bytes());
            log.finish();
            let state = match operation.state {
                RemoteState::Running => Operation::Installing,
                RemoteState::Succeeded if matches!((installed, operation.version.parse::<Version>()), (Ok(installed), Ok(target)) if installed >= target) => {
                    if matches!(check, Some(Check::Available { .. })) {
                        Operation::Idle
                    } else {
                        Operation::Ready { version: format!("v{}", status.version) }
                    }
                },
                RemoteState::Succeeded => Operation::Failed(format!(
                    "The installer finished, but the server reports {} instead of {}.",
                    status.version, operation.version
                )),
                RemoteState::Failed => Operation::Failed(operation.error.unwrap_or_else(|| {
                    "The server update failed. Review the output and try again.".into()
                })),
            };
            (state, log.text().to_owned())
        });
        let lost = operation.is_none() && self.state.borrow().running();
        self.state.send_modify(|snapshot| {
            snapshot.unavailable = status.unavailable;
            if let Some(check) = check {
                if !matches!(check, Check::Failed(_)) {
                    snapshot.checked_at = Some(SystemTime::now());
                }
                snapshot.check = check;
            }
            if let Some((operation, output)) = operation {
                snapshot.operation = operation;
                snapshot.output = output;
            } else if lost {
                snapshot.operation = Operation::Failed(
                    "The server has no record of the update. Check its status before retrying."
                        .into(),
                );
            }
        });
    }

    fn failed_status(&mut self, error: ClientError) {
        let unsupported = matches!(
            &error,
            ClientError::Api {
                status: 404 | 405,
                ..
            }
        ) || matches!(&error, ClientError::Protocol(why) if why.starts_with("HTTP 404") || why.starts_with("HTTP 405"));
        let running = self.state.borrow().running();
        self.state.send_modify(|state| {
            if unsupported {
                let why = "This server needs one update in a terminal before it can update from the app. Run on the server: elsebot update --server-only".to_owned();
                if running {
                    state.operation = Operation::Failed(why.clone());
                }
                state.unavailable = Some(why.clone());
                state.check = Check::Failed(why);
            } else if running && error.outcome_unknown() && self.last_response.elapsed() < RECONNECT_LIMIT {
                state.operation = Operation::Restarting;
            } else if running {
                state.operation = Operation::Failed(format!("Could not follow the server update. Check again to reconnect. {error}"));
            } else {
                state.check = Check::Failed(error.to_string());
            }
        });
    }

    async fn install(&mut self) {
        if self.state.borrow().install_refusal().is_some() {
            return;
        }
        let Some(version) = self.latest else { return };
        self.state.send_modify(|state| {
            state.operation = Operation::Preparing;
            state.output.clear();
        });
        self.work().await;
        if self.state.borrow().work != Work::Idle {
            self.state
                .send_modify(|state| state.operation = Operation::Idle);
            return;
        }
        let request_id = Uuid::new_v4();
        self.last_response = tokio::time::Instant::now();
        let request = self
            .client
            .request(Method::POST, ENDPOINT)
            .timeout(self.settings.work_limit)
            .json(&serde_json::json!({"request_id":request_id,"version":version.plain()}));
        match self.client.send_json::<Status>(request).await {
            Ok((_, status)) => self.apply(status),
            Err(error) => {
                match self.status().await {
                    Ok(status) if status.operation.as_ref().is_some_and(|operation| operation.request_id == request_id) => self.apply(status),
                    _ => self.state.send_modify(|state| state.operation = Operation::Failed(format!("Could not confirm that the server accepted the update. Check again before retrying. {error}"))),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::get,
    };
    use serde_json::{Value, json};
    use std::sync::atomic::AtomicU64;

    const TOKEN: &str = "TTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTT";

    #[derive(Clone)]
    struct Server {
        status: watch::Sender<Value>,
        turns: Arc<AtomicU64>,
        posts: Arc<AtomicU64>,
        offline: Arc<AtomicBool>,
        lose_response: Arc<AtomicBool>,
        gateway: Arc<AtomicBool>,
        supported: Arc<AtomicBool>,
    }

    struct Fixture {
        server: Server,
        client: Arc<Client>,
        settings: Settings,
        _dir: tempfile::TempDir,
        task: tokio::task::JoinHandle<()>,
    }

    async fn status(State(server): State<Server>, headers: HeaderMap) -> Response {
        assert_eq!(headers["authorization"], format!("Bearer {TOKEN}"));
        if !server.supported.load(Ordering::Relaxed) {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error":{"code":"not_found","message":"Not found."}})),
            )
                .into_response();
        }
        if server.gateway.load(Ordering::Relaxed) {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":{"code":"restarting","message":"Backend unavailable"}})),
            )
                .into_response();
        }
        if server.offline.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Json(server.status.borrow().clone()).into_response()
    }

    async fn install(
        State(server): State<Server>,
        headers: HeaderMap,
        Json(request): Json<Value>,
    ) -> Response {
        assert_eq!(headers["authorization"], format!("Bearer {TOKEN}"));
        assert_eq!(request["version"], "0.0.20");
        server.posts.fetch_add(1, Ordering::Relaxed);
        server.status.send_modify(|status| status["operation"] = json!({
            "request_id":request["request_id"], "version":request["version"], "state":"running", "output":"\u{1b}[32mDownloading\u{1b}[0m\n", "error":null
        }));
        if server.lose_response.load(Ordering::Relaxed) {
            StatusCode::BAD_GATEWAY.into_response()
        } else {
            Json(server.status.borrow().clone()).into_response()
        }
    }

    impl Fixture {
        async fn new(supported: bool) -> Self {
            let (status_tx, _) =
                watch::channel(json!({"version":"0.0.9","unavailable":null,"operation":null}));
            let server = Server {
                status: status_tx,
                turns: Arc::default(),
                posts: Arc::default(),
                offline: Arc::default(),
                lose_response: Arc::default(),
                gateway: Arc::default(),
                supported: Arc::new(AtomicBool::new(supported)),
            };
            let router = Router::new().route(ENDPOINT, get(status).post(install))
                .route("/v1/maintenance/update", get(|State(server): State<Server>| async move {
                    Json(json!({"active_turns":server.turns.load(Ordering::Relaxed),"running_commands":0}))
                }))
                .with_state(server.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let profile =
                Profile::parse(&format!("http://{}", listener.local_addr().unwrap()), TOKEN)
                    .unwrap();
            let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let client = Arc::new(
                Client::new(
                    profile.origin,
                    &profile.token,
                    tokio::runtime::Handle::current(),
                )
                .unwrap(),
            );
            let dir = tempfile::tempdir().unwrap();
            let git = dir.path().join("git");
            std::fs::write(&git, "#!/bin/sh\nprintf 'abc\\trefs/tags/v0.0.20\\n'\n").unwrap();
            std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o700)).unwrap();
            let settings = Settings {
                git,
                work_every: Duration::from_millis(50),
                work_limit: Duration::from_millis(100),
                ..Settings::default()
            };
            Self {
                server,
                client,
                settings,
                _dir: dir,
                task,
            }
        }

        fn start(&self) -> (Commands, watch::Receiver<Snapshot>) {
            start_with(
                self.client.clone(),
                self.settings.clone(),
                &tokio::runtime::Handle::current(),
            )
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn until(
        states: &mut watch::Receiver<Snapshot>,
        condition: impl FnMut(&Snapshot) -> bool,
    ) -> Snapshot {
        tokio::time::timeout(Duration::from_secs(5), states.wait_for(condition))
            .await
            .unwrap()
            .unwrap()
            .clone()
    }

    async fn available(states: &mut watch::Receiver<Snapshot>) {
        until(states, |state| {
            matches!(state.check, Check::Available { .. }) && state.work == Work::Idle
        })
        .await;
    }

    #[tokio::test]
    async fn remote_checks_refresh_in_the_background_and_on_request() {
        let mut fixture = Fixture::new(true).await;
        fixture.settings.check_every = Duration::from_millis(100);
        let (commands, mut states) = fixture.start();
        available(&mut states).await;
        std::fs::write(
            &fixture.settings.git,
            "#!/bin/sh\nprintf 'abc\trefs/tags/v0.0.21\n'\n",
        )
        .unwrap();
        until(&mut states, |state| {
            state.check
                == Check::Available {
                    latest: "v0.0.21".into(),
                }
        })
        .await;
        assert_eq!(fixture.server.posts.load(Ordering::Relaxed), 0);
        drop(commands);
        drop(states);

        fixture.settings.check_every = Duration::from_secs(60 * 60);
        let (commands, mut states) = fixture.start();
        until(&mut states, |state| {
            state.check
                == Check::Available {
                    latest: "v0.0.21".into(),
                }
        })
        .await;
        std::fs::write(
            &fixture.settings.git,
            "#!/bin/sh\nprintf 'abc\trefs/tags/v0.0.22\n'\n",
        )
        .unwrap();
        commands.check().unwrap();
        until(&mut states, |state| {
            state.check
                == Check::Available {
                    latest: "v0.0.22".into(),
                }
        })
        .await;
        assert_eq!(fixture.server.posts.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_remote_update_streams_output_reconnects_and_survives_reopening_the_screen() {
        let fixture = Fixture::new(true).await;
        let (commands, mut states) = fixture.start();
        available(&mut states).await;
        commands.install().unwrap();
        assert!(commands.install().is_err());
        let installing = until(&mut states, |state| {
            state.operation == Operation::Installing
        })
        .await;
        assert_eq!(installing.output, "Downloading\n");
        assert!(commands.restart().is_err());
        fixture.server.offline.store(true, Ordering::Relaxed);
        until(&mut states, |state| {
            state.operation == Operation::Restarting
        })
        .await;
        fixture.server.offline.store(false, Ordering::Relaxed);
        fixture.server.status.send_modify(|status| {
            status["version"] = json!("0.0.20");
            status["operation"]["state"] = json!("succeeded");
            status["operation"]["output"] = json!("Installed\n");
        });
        let installed = until(&mut states, |state| {
            matches!(state.operation, Operation::Ready { .. })
        })
        .await;
        assert_eq!(
            installed.check,
            Check::Current {
                latest: "v0.0.20".into()
            }
        );
        assert_eq!(installed.output, "Installed\n");
        drop(commands);
        drop(states);
        let (_, mut reopened) = fixture.start();
        until(&mut reopened, |state| {
            matches!(state.operation, Operation::Ready { .. })
        })
        .await;
        assert_eq!(fixture.server.posts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn busy_remote_work_is_rechecked_before_starting_and_failed_updates_can_retry() {
        let fixture = Fixture::new(true).await;
        let (commands, mut states) = fixture.start();
        available(&mut states).await;
        fixture.server.turns.store(1, Ordering::Relaxed);
        commands.install().unwrap();
        until(&mut states, |state| {
            matches!(state.work, Work::Busy { .. }) && state.operation == Operation::Idle
        })
        .await;
        assert_eq!(fixture.server.posts.load(Ordering::Relaxed), 0);
        fixture.server.turns.store(0, Ordering::Relaxed);
        available(&mut states).await;
        commands.install().unwrap();
        until(&mut states, |state| {
            state.operation == Operation::Installing
        })
        .await;
        fixture.server.status.send_modify(|status| {
            status["operation"]["state"] = json!("failed");
            status["operation"]["error"] = json!("Download failed");
        });
        until(
            &mut states,
            |state| matches!(&state.operation, Operation::Failed(why) if why == "Download failed"),
        )
        .await;
        commands.install().unwrap();
        until(&mut states, |state| {
            state.operation == Operation::Installing
        })
        .await;
        assert_eq!(fixture.server.posts.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn a_lost_install_response_follows_the_accepted_request_without_posting_twice() {
        let fixture = Fixture::new(true).await;
        fixture.server.lose_response.store(true, Ordering::Relaxed);
        let (commands, mut states) = fixture.start();
        available(&mut states).await;
        commands.install().unwrap();
        until(&mut states, |state| {
            state.operation == Operation::Installing
        })
        .await;
        assert_eq!(fixture.server.posts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn a_proxy_failure_during_restart_reconnects_to_the_updated_server() {
        let fixture = Fixture::new(true).await;
        let (commands, mut states) = fixture.start();
        available(&mut states).await;
        commands.install().unwrap();
        until(&mut states, |state| {
            state.operation == Operation::Installing
        })
        .await;
        fixture.server.gateway.store(true, Ordering::Relaxed);
        until(&mut states, |state| {
            state.operation == Operation::Restarting
        })
        .await;
        fixture.server.status.send_modify(|status| {
            status["version"] = json!("0.0.20");
            status["operation"]["state"] = json!("succeeded");
        });
        fixture.server.gateway.store(false, Ordering::Relaxed);
        until(&mut states, |state| {
            matches!(state.operation, Operation::Ready { .. })
        })
        .await;
        assert_eq!(fixture.server.posts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn older_servers_explain_the_one_time_terminal_update() {
        let fixture = Fixture::new(false).await;
        let (commands, mut states) = fixture.start();
        let unavailable = until(&mut states, |state| state.unavailable.is_some()).await;
        assert!(
            unavailable
                .unavailable
                .unwrap()
                .contains("elsebot update --server-only")
        );
        assert!(commands.install().is_err());
        assert_eq!(states.borrow().latest.as_deref(), Some("v0.0.20"));
        fixture.server.supported.store(true, Ordering::Relaxed);
        commands.check().unwrap();
        available(&mut states).await;
        assert!(states.borrow().unavailable.is_none());
        assert_eq!(fixture.server.posts.load(Ordering::Relaxed), 0);
    }
}
