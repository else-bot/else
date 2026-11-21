//! Each saved server's health, checked in the background every 30 seconds
//! or on request.

use super::*;
use else_wire::settings::{Capabilities, ServerInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::connection) enum Health {
    Checking,
    Online,
    Offline,
    Unauthorized,
    Incompatible,
}

#[derive(Clone)]
pub(in crate::connection) struct Observation {
    pub health: Health,
    pub version: Option<String>,
    pub seen: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub working: Option<u64>,
    /// `None` when the server does not keep a name or has not answered;
    /// a name of `None` when it keeps one but nobody set it.
    pub server: Option<ServerInfo>,
    /// When the last answered check started.
    pub asked: Option<Instant>,
    /// From the last answered check; `None` when the bot list failed.
    pub attention: Option<crate::attention::Attention>,
    /// The last bot list that loaded and when its check started, kept
    /// while the server is offline or the list fails.
    pub bots: Option<(Instant, Vec<crate::quick_switcher::Bot>)>,
}

impl Default for Observation {
    fn default() -> Self {
        Self {
            health: Health::Checking,
            version: None,
            seen: None,
            error: None,
            working: None,
            server: None,
            asked: None,
            attention: None,
            bots: None,
        }
    }
}

impl Observation {
    /// Applies a check that started at `asked`. A failed bot list keeps the
    /// previous list with the time of the check that loaded it.
    fn record(&mut self, result: Result<Observed, (Health, String)>, asked: Instant) {
        match result {
            Ok(observed) => {
                self.health = Health::Online;
                self.version = observed.version;
                self.working = observed.working;
                self.server = observed.server;
                self.attention = observed.attention;
                if let Some(bots) = observed.bots {
                    self.bots = Some((asked, bots));
                }
                self.asked = Some(asked);
                self.seen = Some(Utc::now());
                self.error = None;
            }
            Err((health, error)) => {
                self.health = health;
                self.error = Some(error);
                self.working = None;
            }
        }
    }
}

struct Monitor {
    profile: Profile,
    commands: mpsc::Sender<()>,
    _watch: Task<()>,
}

pub(in crate::connection) struct Connections {
    pub entries: HashMap<Uuid, Observation>,
    monitors: HashMap<Uuid, Monitor>,
}

#[derive(Deserialize)]
struct Report {
    version: String,
    active_turns: u64,
    running_commands: u64,
}

struct Observed {
    version: Option<String>,
    working: Option<u64>,
    server: Option<ServerInfo>,
    attention: Option<crate::attention::Attention>,
    bots: Option<Vec<crate::quick_switcher::Bot>>,
}

async fn observe(client: Arc<Client>) -> Result<Observed, (Health, String)> {
    let this = client.clone();
    client
        .run(async move {
            let (caps, readiness, report, bots) = tokio::join!(
                this.send_json::<Capabilities>(this.request(Method::GET, "/v1/capabilities")),
                this.send_json::<serde_json::Value>(this.request(Method::GET, "/readyz")),
                this.send_json::<Report>(this.request(Method::GET, "/v1/maintenance/update")),
                this.send_json::<else_wire::bots::BotList>(this.request(Method::GET, "/v1/bots")),
            );
            let (_, caps) = caps.map_err(|err| match err {
                ClientError::Unauthorized => (
                    Health::Unauthorized,
                    "The server rejected the saved owner token.".into(),
                ),
                other => (Health::Offline, other.to_string()),
            })?;
            if caps
                .api_version
                .as_deref()
                .is_some_and(|version| version != "v1")
            {
                return Err((
                    Health::Incompatible,
                    "Unsupported API version. Update the desktop app.".into(),
                ));
            }
            if let Err(err) = readiness
                && !matches!(&err, ClientError::Protocol(why) if why.starts_with("HTTP 404"))
            {
                return Err((
                    Health::Offline,
                    format!("Server readiness check failed. {err}"),
                ));
            }
            let (version, working) = match report {
                Ok((_, report)) => (
                    Some(report.version),
                    Some(report.active_turns.saturating_add(report.running_commands)),
                ),
                Err(_) => (None, None),
            };
            let list = bots.ok().map(|(_, list)| list.bots);
            let attention = list
                .as_ref()
                .map(|bots| crate::attention::Attention::of(bots, crate::inbox::saved_unread));
            Ok(Observed {
                version,
                working,
                server: caps.server,
                attention,
                bots: list.as_deref().map(crate::quick_switcher::Bot::listed),
            })
        })
        .await
}

fn monitor(
    profile: Profile,
    runtime: &tokio::runtime::Handle,
) -> (mpsc::Sender<()>, watch::Receiver<Observation>) {
    let (commands, mut requests) = mpsc::channel(1);
    let (states, snapshots) = watch::channel(Observation::default());
    runtime.spawn(async move {
        let client = Client::new(profile.origin, &profile.token, tokio::runtime::Handle::current()).map(Arc::new);
        let mut timer = tokio::time::interval(Duration::from_secs(30));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = states.closed() => break,
                request = requests.recv() => if request.is_none() { break },
                _ = timer.tick() => {},
            }
            let asked = Instant::now();
            let result = match &client {
                Ok(client) => tokio::select! { _ = states.closed() => break, result = observe(client.clone()) => result },
                Err(error) => Err((Health::Offline, error.clone())),
            };
            states.send_modify(|state| state.record(result, asked));
        }
    });
    (commands, snapshots)
}

impl Connections {
    /// Check a saved server again now instead of at its next interval.
    pub(in crate::connection) fn recheck(&self, id: Uuid) {
        if let Some(monitor) = self.monitors.get(&id) {
            let _ = monitor.commands.try_send(());
        }
    }

    pub(in crate::connection) fn new(
        registry: &ServerRegistry,
        runtime: &tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Shell>,
    ) -> Self {
        let mut this = Self {
            entries: HashMap::new(),
            monitors: HashMap::new(),
        };
        this.refresh(registry, runtime, window, cx);
        this
    }

    pub(in crate::connection) fn refresh(
        &mut self,
        registry: &ServerRegistry,
        runtime: &tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Shell>,
    ) {
        self.entries.retain(|id, _| registry.find(*id).is_some());
        self.monitors.retain(|id, monitor| {
            registry
                .find(*id)
                .is_some_and(|saved| saved.profile == monitor.profile)
        });
        for saved in &registry.servers {
            if let Some(monitor) = self.monitors.get(&saved.id) {
                let _ = monitor.commands.try_send(());
                continue;
            }
            self.entries.insert(saved.id, Observation::default());
            let id = saved.id;
            let profile = saved.profile.clone();
            let (commands, mut states) = monitor(profile.clone(), runtime);
            let expected = profile.clone();
            let task = cx.spawn_in(window, async move |this, cx| {
                while states.changed().await.is_ok() {
                    let snapshot = states.borrow_and_update().clone();
                    let accepted = this.update(cx, |shell, cx| {
                        if !shell
                            .registry
                            .find(id)
                            .is_some_and(|s| s.profile == expected)
                        {
                            return false;
                        }
                        if let Some((fetched, bots)) = &snapshot.bots {
                            shell.cache_bots(id, bots, *fetched, cx);
                        }
                        shell.servers.entries.insert(id, snapshot);
                        shell.adopt_server_name(id, cx);
                        shell.adopt_server_icon(id, cx);
                        cx.notify();
                        true
                    });
                    if !matches!(accepted, Ok(true)) {
                        break;
                    }
                }
            });
            self.monitors.insert(
                id,
                Monitor {
                    profile,
                    commands,
                    _watch: task,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        routing::get,
    };
    use serde_json::json;

    const TOKEN: &str = "TTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTTT";

    #[::core::prelude::v1::test]
    fn a_failed_bot_list_keeps_the_last_list_and_its_time() {
        let observed = |bots: Option<Vec<crate::quick_switcher::Bot>>| Observed {
            version: None,
            working: None,
            server: None,
            attention: None,
            bots,
        };
        let bot = crate::quick_switcher::Bot {
            id: Uuid::new_v4(),
            name: "Inbox".into(),
            avatar: Default::default(),
        };
        let first = Instant::now();
        let mut state = Observation::default();
        state.record(Ok(observed(Some(vec![bot.clone()]))), first);
        let later = first + Duration::from_secs(30);
        state.record(Ok(observed(None)), later);
        assert_eq!(state.asked, Some(later));
        assert_eq!(state.bots, Some((first, vec![bot.clone()])));
        state.record(
            Err((Health::Offline, "down".into())),
            later + Duration::from_secs(30),
        );
        assert_eq!(state.health, Health::Offline);
        assert_eq!(state.bots, Some((first, vec![bot])));
    }

    async fn fixture(
        version: &str,
        api_version: &str,
        ready: StatusCode,
        report: bool,
        expected_token: &str,
    ) -> (Profile, tokio::task::JoinHandle<()>) {
        let version = version.to_owned();
        let api_version = api_version.to_owned();
        let expected = format!("Bearer {expected_token}");
        let app = Router::new()
            .route("/v1/capabilities", get(move |headers: HeaderMap| {
                let api_version = api_version.clone();
                let expected = expected.clone();
                async move {
                    let allowed = headers.get("authorization").and_then(|value| value.to_str().ok()) == Some(expected.as_str());
                    let status = if allowed { StatusCode::OK } else { StatusCode::UNAUTHORIZED };
                    let mut body = json!({"api_version":api_version,"providers":[],"limits":{"max_text_bytes":32768}});
                    if report {
                        body["server"] = json!({"name": "Studio"});
                    }
                    (status, Json(body))
                }
            }))
            .route("/readyz", get(move || async move {
                (ready, Json(json!({"ready":ready.is_success()})))
            }))
            .route("/v1/maintenance/update", get(move || {
                let version = version.clone();
                async move {
                    let status = if report { StatusCode::OK } else { StatusCode::NOT_FOUND };
                    (status, Json(json!({"version":version,"active_turns":2,"running_commands":1})))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (Profile::parse(&origin, TOKEN).unwrap(), server)
    }

    type Probed = (Option<String>, Option<u64>, Option<Option<String>>);

    async fn probe(profile: Profile) -> Result<Probed, (Health, String)> {
        let client = Arc::new(
            Client::new(
                profile.origin,
                &profile.token,
                tokio::runtime::Handle::current(),
            )
            .unwrap(),
        );
        observe(client)
            .await
            .map(|o| (o.version, o.working, o.server.map(|s| s.name)))
    }

    #[tokio::test]
    async fn saved_servers_report_their_own_authenticated_version_and_work() {
        let (a, a_server) = fixture("0.0.20", "v1", StatusCode::OK, true, TOKEN).await;
        let (b, b_server) = fixture(
            "0.0.24",
            "v1",
            StatusCode::OK,
            true,
            "UUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUU",
        )
        .await;
        let b = Profile::parse(
            b.origin.as_str(),
            "UUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUUU",
        )
        .unwrap();
        let (first, second) = tokio::join!(probe(a), probe(b));
        let studio = Some(Some("Studio".into()));
        assert_eq!(
            first.unwrap(),
            (Some("0.0.20".into()), Some(3), studio.clone())
        );
        assert_eq!(second.unwrap(), (Some("0.0.24".into()), Some(3), studio));
        a_server.abort();
        b_server.abort();
    }

    #[tokio::test]
    async fn connection_health_distinguishes_authentication_readiness_and_protocol() {
        for (api_version, ready, token, expected) in [
            ("v1", StatusCode::OK, "wrong", Health::Unauthorized),
            (
                "v1",
                StatusCode::SERVICE_UNAVAILABLE,
                TOKEN,
                Health::Offline,
            ),
            ("v2", StatusCode::OK, TOKEN, Health::Incompatible),
        ] {
            let (profile, server) = fixture("0.0.24", api_version, ready, true, token).await;
            let (health, _) = probe(profile).await.unwrap_err();
            assert!(health == expected);
            server.abort();
        }
    }

    #[tokio::test]
    async fn older_server_without_version_reporting_remains_online() {
        let (profile, server) = fixture("0.0.24", "v1", StatusCode::OK, false, TOKEN).await;
        assert_eq!(probe(profile).await.unwrap(), (None, None, None));
        server.abort();
    }
}
