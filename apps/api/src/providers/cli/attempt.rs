//! The stop lifecycle of one provider attempt and how it ends.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::time::Instant;

use super::process::{Finish, Scope, Stderr};
use super::session::PreparedTurn;
use crate::app::providers::{Stop, TurnOutcome, TurnSink};

pub const TURN_DEADLINE: Duration = Duration::from_secs(30 * 60);
/// How long an interrupted provider may take to report its terminal result.
pub const INTERRUPT_GRACE: Duration = Duration::from_secs(15);
/// How long a parent may keep going after a tool parked its turn.
pub const PARK_GRACE: Duration = Duration::from_secs(20);

/// Why an attempt is being interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopCause {
    /// The owner, or a sink that no longer accepts this turn.
    Owner,
    /// An else.bot tool parked the turn and the provider kept going.
    Parked,
    Deadline,
}

/// The provider broke else.bot's policy for it: the attempt is killed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub code: &'static str,
    pub message: String,
}

impl Violation {
    pub fn policy(message: impl Into<String>) -> Self {
        Self {
            code: "policy_violation",
            message: message.into(),
        }
    }

    pub fn profile(message: impl Into<String>) -> Self {
        Self {
            code: "profile_mismatch",
            message: message.into(),
        }
    }
}

impl From<Violation> for TurnOutcome {
    fn from(violation: Violation) -> Self {
        TurnOutcome::Failed {
            code: violation.code.to_owned(),
            message: violation.message,
        }
    }
}

enum Phase {
    Running { park_at: Option<Instant> },
    Stopping { cause: StopCause, give_up: Instant },
}

/// What a timer means now.
#[derive(Debug, PartialEq, Eq)]
pub enum Tick {
    Wait,
    /// Interrupt the provider (the attempt is now stopping).
    Interrupt,
    /// The provider did not stop: end the attempt without its result.
    GiveUp,
}

/// Running, then possibly stopping for one cause, with the deadline, park,
/// and interruption timers of a turn.
pub struct Lifecycle {
    deadline: Instant,
    phase: Phase,
}

impl Lifecycle {
    pub fn start() -> Self {
        Self {
            deadline: Instant::now() + TURN_DEADLINE,
            phase: Phase::Running { park_at: None },
        }
    }

    pub fn wake_at(&self) -> Instant {
        match self.phase {
            Phase::Running { park_at } => park_at.map_or(self.deadline, |at| at.min(self.deadline)),
            Phase::Stopping { give_up, .. } => give_up,
        }
    }

    pub fn running(&self) -> bool {
        matches!(self.phase, Phase::Running { .. })
    }

    pub fn cause(&self) -> Option<StopCause> {
        match self.phase {
            Phase::Running { .. } => None,
            Phase::Stopping { cause, .. } => Some(cause),
        }
    }

    /// A tool parked the turn: the provider gets [`PARK_GRACE`] to end it.
    pub fn parked(&mut self) {
        if let Phase::Running { park_at: None } = self.phase {
            self.phase = Phase::Running {
                park_at: Some(Instant::now() + PARK_GRACE),
            };
        }
    }

    /// Start stopping; true when the provider must be interrupted now.
    pub fn stop(&mut self, cause: StopCause) -> bool {
        if !self.running() {
            return false;
        }
        self.phase = Phase::Stopping {
            cause,
            give_up: Instant::now() + INTERRUPT_GRACE,
        };
        true
    }

    pub fn on_timer(&mut self) -> Tick {
        let now = Instant::now();
        match self.phase {
            Phase::Stopping { give_up, .. } if now >= give_up => Tick::GiveUp,
            Phase::Running { park_at: Some(at) } if now >= at => {
                tracing::info!("the provider kept going after its turn parked; interrupting");
                self.stop(StopCause::Parked);
                Tick::Interrupt
            }
            Phase::Running { .. } if now >= self.deadline => {
                self.stop(StopCause::Deadline);
                Tick::Interrupt
            }
            _ => Tick::Wait,
        }
    }

    /// The outcome of an attempt ended without its terminal result. A
    /// parked turn ends as interrupted, so its pending work continues it.
    pub fn abandoned(&self, why: impl Into<String>) -> TurnOutcome {
        match self.cause() {
            Some(StopCause::Parked) => TurnOutcome::Interrupted,
            _ => TurnOutcome::Uncertain {
                message: why.into(),
            },
        }
    }

    /// The outcome of a terminal result the protocol interpreted.
    pub fn settled(&self, outcome: TurnOutcome) -> TurnOutcome {
        match (self.cause(), &outcome) {
            (_, TurnOutcome::Uncertain { .. }) => outcome,
            (Some(StopCause::Deadline), _) => TurnOutcome::Failed {
                code: "turn_deadline".into(),
                message: "The turn exceeded its 30 minute deadline.".into(),
            },
            _ => outcome,
        }
    }
}

/// What a protocol reads from one provider message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event<T> {
    /// The provider acknowledged the turn, with the model it reported.
    Accepted {
        model: String,
        native_turn: String,
    },
    Text(String),
    /// Later text starts a new assistant message.
    EndMessage,
    /// An else.bot tool parked the turn.
    Parked,
    Finished(T),
}

/// A provider's turn protocol: validation of every message it sends, and
/// the meaning of its terminal result. Anything outside its policy is a
/// [`Violation`], which kills the attempt.
pub trait Protocol {
    type Message;
    type Terminal;

    fn on(&mut self, message: &Self::Message) -> Result<Vec<Event<Self::Terminal>>, Violation>;

    /// The outcome of the terminal result; `stopping` is why the attempt
    /// interrupted the provider, if it did.
    fn outcome(
        &self,
        terminal: Self::Terminal,
        stopping: Option<StopCause>,
        accepted: bool,
    ) -> TurnOutcome;

    /// Whether a native session that ended with `outcome` may be resumed.
    fn resumable(&self, outcome: &TurnOutcome) -> bool {
        let _ = outcome;
        true
    }
}

/// Where a running turn's messages come from, and how it is interrupted.
/// `next` must be cancellation safe: the runner races it with timers.
#[async_trait]
pub trait Channel: Send {
    type Message: Send;

    /// The next message; `None` once the provider has exited.
    async fn next(&mut self) -> Result<Option<Self::Message>, String>;

    /// Ask the provider to end its turn. It may be abandoned midway when the
    /// stop escalates, so it must not hold state the attempt still needs.
    async fn interrupt(&mut self);
}

/// Ask the provider to stop; false when the stop escalated first. A
/// transport wait (a full pipe, an unanswered request) never delays a hard
/// stop.
async fn interrupt<C: Channel>(channel: &mut C, stop: &Stop) -> bool {
    tokio::select! {
        () = channel.interrupt() => true,
        () = stop.escalated() => false,
    }
}

/// Run an accepted or about-to-be-accepted turn to its end: report its
/// messages to the sink in order, interrupt it on a stop request, a parked
/// turn, or the deadline, and give up on it when it does not stop in time
/// or the stop escalates.
pub async fn drive<C, P>(
    channel: &mut C,
    protocol: &mut P,
    turn: &mut PreparedTurn<'_>,
    sink: &dyn TurnSink,
    stop: &Stop,
    label: &str,
) -> Ended
where
    C: Channel,
    P: Protocol<Message = C::Message>,
{
    let not_stopped = || format!("{label} did not stop after being interrupted.");
    let mut life = Lifecycle::start();
    loop {
        let read = tokio::select! {
            () = stop.escalated() => return Ended::Abandoned(life.abandoned(not_stopped())),
            () = stop.requested(), if life.running() => {
                if life.stop(StopCause::Owner) && !interrupt(channel, stop).await {
                    return Ended::Abandoned(life.abandoned(not_stopped()));
                }
                continue;
            }
            () = tokio::time::sleep_until(life.wake_at()) => {
                match life.on_timer() {
                    Tick::Interrupt if !interrupt(channel, stop).await => {
                        return Ended::Abandoned(life.abandoned(not_stopped()));
                    }
                    Tick::GiveUp => return Ended::Abandoned(life.abandoned(not_stopped())),
                    Tick::Interrupt | Tick::Wait => {}
                }
                continue;
            }
            read = channel.next() => read,
        };
        let message = match read {
            Ok(Some(message)) => message,
            Ok(None) => {
                let why = format!("{label} exited before its terminal result.");
                return Ended::Abandoned(life.abandoned(why));
            }
            Err(err) => return Ended::Abandoned(life.abandoned(err)),
        };
        let events = match protocol.on(&message) {
            Ok(events) => events,
            Err(violation) => return Ended::Abandoned(violation.into()),
        };
        for event in events {
            let written = match event {
                Event::Accepted { model, native_turn } => {
                    let reported =
                        json!({ "model": model, "reasoning_effort": null, "service_tier": null });
                    turn.accept(sink, &reported, &native_turn).await
                }
                Event::Text(text) => sink.text(&text).await,
                Event::EndMessage => sink.end_message().await,
                Event::Parked => {
                    life.parked();
                    Ok(())
                }
                Event::Finished(terminal) => {
                    let outcome =
                        life.settled(protocol.outcome(terminal, life.cause(), turn.accepted()));
                    let resumable = protocol.resumable(&outcome);
                    return Ended::Terminal { outcome, resumable };
                }
            };
            if written.is_err() {
                stop.request();
            }
        }
    }
}

/// How an attempt ended, which decides its cleanup and its native session.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// Nothing reached the native session.
    Undelivered(TurnOutcome),
    /// The native turn reported its terminal result.
    Terminal {
        outcome: TurnOutcome,
        resumable: bool,
    },
    /// Stopped without a terminal result: the native turn may be unfinished.
    Abandoned(TurnOutcome),
}

/// End the attempt: stop and sweep its processes, retire a native session
/// that must not be resumed, and make the outcome uncertain when cleanup
/// cannot be verified.
pub async fn conclude(
    ended: Ended,
    scope: Scope,
    stderr: Option<Stderr>,
    sink: &dyn TurnSink,
    label: &str,
) -> TurnOutcome {
    let (outcome, finish, retire) = match ended {
        Ended::Undelivered(outcome) => (outcome, Finish::Kill, false),
        Ended::Terminal { outcome, resumable } => (outcome, Finish::Exit, !resumable),
        Ended::Abandoned(outcome) => (outcome, Finish::Kill, true),
    };
    let outcome = match scope.finish(finish).await {
        Ok(()) => outcome,
        Err(err) => TurnOutcome::Uncertain {
            message: format!("{label}'s processes could not be stopped: {err}"),
        },
    };
    if retire || matches!(outcome, TurnOutcome::Uncertain { .. }) {
        sink.retire_session().await;
    }
    match (outcome, stderr) {
        (TurnOutcome::Uncertain { message }, Some(stderr)) => {
            let reason = stderr.last_line().await;
            TurnOutcome::Uncertain {
                message: format!("{message} {reason}").trim_end().to_owned(),
            }
        }
        (outcome, _) => outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;

    use serde_json::{Value, json};

    use crate::providers::cli::session::tests::{Log, input};
    use crate::providers::cli::session::{Opened, PreparedTurn};

    /// Messages to replay, then silence: a provider that never stops.
    struct Replay {
        messages: VecDeque<Value>,
        interrupts: usize,
        interrupt_hangs: bool,
    }

    #[async_trait]
    impl Channel for Replay {
        type Message = Value;

        async fn next(&mut self) -> Result<Option<Value>, String> {
            match self.messages.pop_front() {
                Some(message) => Ok(Some(message)),
                None => std::future::pending().await,
            }
        }

        async fn interrupt(&mut self) {
            self.interrupts += 1;
            if self.interrupt_hangs {
                std::future::pending::<()>().await;
            }
        }
    }

    struct Plain;

    impl Protocol for Plain {
        type Message = Value;
        type Terminal = ();

        fn on(&mut self, message: &Value) -> Result<Vec<Event<()>>, Violation> {
            Ok(match message["kind"].as_str().unwrap() {
                "accept" => vec![Event::Accepted {
                    model: "m".into(),
                    native_turn: "t".into(),
                }],
                "text" => vec![Event::Text(message["text"].as_str().unwrap().into())],
                "park" => vec![Event::Parked],
                "end" => vec![Event::Finished(())],
                _ => return Err(Violation::policy("bad")),
            })
        }

        fn outcome(&self, (): (), stopping: Option<StopCause>, _: bool) -> TurnOutcome {
            if stopping.is_some() {
                TurnOutcome::Interrupted
            } else {
                TurnOutcome::Completed
            }
        }
    }

    fn replay(messages: &[Value]) -> Replay {
        Replay {
            messages: messages.iter().cloned().collect(),
            interrupts: 0,
            interrupt_hangs: false,
        }
    }

    #[tokio::test]
    async fn messages_reach_a_slow_sink_in_order_and_a_result_ends_the_turn() {
        let input = input();
        let sink = Log::default();
        let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n".into()));
        let mut channel = replay(&[
            json!({"kind": "accept"}),
            json!({"kind": "text", "text": "a"}),
            json!({"kind": "text", "text": "b"}),
            json!({"kind": "end"}),
        ]);
        let ended = drive(
            &mut channel,
            &mut Plain,
            &mut turn,
            &sink,
            &Stop::default(),
            "P",
        )
        .await;
        assert_eq!(
            ended,
            Ended::Terminal {
                outcome: TurnOutcome::Completed,
                resumable: true
            }
        );
        assert_eq!(
            sink.calls(),
            ["running", "session", "reported:\"m\":t", "text:a", "text:b"]
        );
        assert_eq!(channel.interrupts, 0);
    }

    #[tokio::test]
    async fn a_fenced_sink_interrupts_and_an_escalation_gives_up_at_once() {
        let input = input();
        let sink = Log::default();
        sink.fenced.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n".into()));
        let mut channel = replay(&[json!({"kind": "text", "text": "late"})]);
        let stop = Stop::default();
        let escalate = {
            let stop = stop.clone();
            async move {
                stop.requested().await;
                stop.escalate();
            }
        };
        let mut plain = Plain;
        let (ended, ()) = tokio::join!(
            drive(&mut channel, &mut plain, &mut turn, &sink, &stop, "P"),
            escalate
        );
        assert!(matches!(
            ended,
            Ended::Abandoned(TurnOutcome::Uncertain { .. })
        ));
        assert_eq!(channel.interrupts, 1);
    }

    #[tokio::test]
    async fn an_escalation_preempts_an_interrupt_that_never_completes() {
        let input = input();
        let sink = Log::default();
        let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n".into()));
        let mut channel = replay(&[json!({"kind": "accept"})]);
        channel.interrupt_hangs = true;
        let stop = Stop::default();
        let owner = {
            let stop = stop.clone();
            async move {
                stop.request();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                stop.escalate();
            }
        };
        let mut plain = Plain;
        let (ended, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(
                drive(&mut channel, &mut plain, &mut turn, &sink, &stop, "P"),
                owner
            )
        })
        .await
        .expect("the escalation reached cleanup");
        assert!(matches!(
            ended,
            Ended::Abandoned(TurnOutcome::Uncertain { .. })
        ));
        assert_eq!(channel.interrupts, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_parked_turn_that_never_stops_ends_interrupted() {
        let input = input();
        let sink = Log::default();
        let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n".into()));
        let mut channel = replay(&[json!({"kind": "accept"}), json!({"kind": "park"})]);
        let ended = drive(
            &mut channel,
            &mut Plain,
            &mut turn,
            &sink,
            &Stop::default(),
            "P",
        )
        .await;
        assert_eq!(ended, Ended::Abandoned(TurnOutcome::Interrupted));
        assert_eq!(channel.interrupts, 1);
    }

    #[tokio::test]
    async fn a_violation_abandons_the_attempt_with_its_code() {
        let input = input();
        let sink = Log::default();
        let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n".into()));
        let mut channel = replay(&[json!({"kind": "other"})]);
        let ended = drive(
            &mut channel,
            &mut Plain,
            &mut turn,
            &sink,
            &Stop::default(),
            "P",
        )
        .await;
        assert!(matches!(
            ended,
            Ended::Abandoned(TurnOutcome::Failed { code, .. }) if code == "policy_violation"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_parked_turn_is_interrupted_then_given_up_as_interrupted() {
        let mut life = Lifecycle::start();
        life.parked();
        assert_eq!(life.on_timer(), Tick::Wait);
        tokio::time::advance(PARK_GRACE).await;
        assert_eq!(life.on_timer(), Tick::Interrupt);
        assert_eq!(life.cause(), Some(StopCause::Parked));
        assert!(!life.stop(StopCause::Owner), "the first cause stands");
        tokio::time::advance(INTERRUPT_GRACE).await;
        assert_eq!(life.on_timer(), Tick::GiveUp);
        assert_eq!(life.abandoned("gone"), TurnOutcome::Interrupted);
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_interrupts_and_overrides_a_terminal_result() {
        let mut life = Lifecycle::start();
        tokio::time::advance(TURN_DEADLINE).await;
        assert_eq!(life.on_timer(), Tick::Interrupt);
        assert!(matches!(
            life.settled(TurnOutcome::Interrupted),
            TurnOutcome::Failed { code, .. } if code == "turn_deadline"
        ));
        let unknown = TurnOutcome::Uncertain {
            message: "x".into(),
        };
        assert_eq!(life.settled(unknown.clone()), unknown);
        assert_eq!(
            life.abandoned("no result"),
            TurnOutcome::Uncertain {
                message: "no result".into()
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_owner_stop_gives_up_as_uncertain() {
        let mut life = Lifecycle::start();
        assert!(life.stop(StopCause::Owner));
        assert_eq!(life.wake_at(), Instant::now() + INTERRUPT_GRACE);
        tokio::time::advance(INTERRUPT_GRACE).await;
        assert_eq!(life.on_timer(), Tick::GiveUp);
        assert!(matches!(life.abandoned("x"), TurnOutcome::Uncertain { .. }));
        assert_eq!(
            life.settled(TurnOutcome::Interrupted),
            TurnOutcome::Interrupted
        );
    }
}
