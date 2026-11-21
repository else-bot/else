//! The stop lifecycle of one provider attempt and how it ends.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::time::Instant;

use super::process::{Finish, Scope, Stderr, last_lines};
use super::session::PreparedTurn;
use crate::app::providers::{FailureCode, Steer, Stop, TurnOutcome, TurnSink};

mod steer;

pub use steer::SendError;
use steer::Steering;

pub const TURN_DEADLINE: Duration = Duration::from_secs(30 * 60);
/// How long before [`TURN_DEADLINE`] the provider is told its time is
/// running out.
pub const DEADLINE_WARNING: Duration = Duration::from_secs(5 * 60);
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
    pub code: FailureCode,
    pub message: String,
}

impl Violation {
    pub fn policy(message: impl Into<String>) -> Self {
        Self {
            code: FailureCode::PolicyViolation,
            message: message.into(),
        }
    }

    pub fn profile(message: impl Into<String>) -> Self {
        Self {
            code: FailureCode::ProfileMismatch,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl From<Violation> for TurnOutcome {
    fn from(violation: Violation) -> Self {
        TurnOutcome::Failed {
            code: violation.code,
            message: violation.message,
        }
    }
}

enum Phase {
    /// `warn_at` is when the provider is told its time runs out, until it is.
    Running {
        park_at: Option<Instant>,
        warn_at: Option<Instant>,
    },
    Stopping {
        cause: StopCause,
        give_up: Instant,
    },
}

/// What a timer means now.
#[derive(Debug, PartialEq, Eq)]
pub enum Tick {
    Wait,
    /// Tell the provider its time runs out ([`deadline_warning`]).
    Warn,
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
        let deadline = Instant::now() + TURN_DEADLINE;
        Self {
            deadline,
            phase: Phase::Running {
                park_at: None,
                warn_at: Some(deadline - DEADLINE_WARNING),
            },
        }
    }

    pub fn wake_at(&self) -> Instant {
        match self.phase {
            Phase::Running { park_at, warn_at } => [park_at, warn_at]
                .into_iter()
                .flatten()
                .fold(self.deadline, Instant::min),
            Phase::Stopping { give_up, .. } => give_up,
        }
    }

    pub fn running(&self) -> bool {
        matches!(self.phase, Phase::Running { .. })
    }

    /// Running and not parked, so newer owner messages may join the turn.
    pub fn takes_steers(&self) -> bool {
        matches!(self.phase, Phase::Running { park_at: None, .. })
    }

    pub fn cause(&self) -> Option<StopCause> {
        match self.phase {
            Phase::Running { .. } => None,
            Phase::Stopping { cause, .. } => Some(cause),
        }
    }

    /// A tool parked the turn: the provider gets [`PARK_GRACE`] to end it.
    pub fn parked(&mut self) {
        if let Phase::Running {
            park_at: None,
            warn_at,
        } = self.phase
        {
            self.phase = Phase::Running {
                park_at: Some(Instant::now() + PARK_GRACE),
                warn_at,
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
            Phase::Running {
                park_at: Some(at), ..
            } if now >= at => {
                tracing::info!("the provider kept going after its turn parked; interrupting");
                self.stop(StopCause::Parked);
                Tick::Interrupt
            }
            Phase::Running { .. } if now >= self.deadline => {
                self.stop(StopCause::Deadline);
                Tick::Interrupt
            }
            Phase::Running {
                park_at,
                warn_at: Some(at),
            } if now >= at => {
                self.phase = Phase::Running {
                    park_at,
                    warn_at: None,
                };
                if park_at.is_some() {
                    Tick::Wait
                } else {
                    Tick::Warn
                }
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

    /// The outcome of a terminal result the protocol interpreted. Only a
    /// confirmed interruption is a cut: a turn that completed or failed on
    /// its own as the deadline fired keeps that outcome.
    pub fn settled(&self, outcome: TurnOutcome) -> TurnOutcome {
        match (self.cause(), outcome) {
            (Some(StopCause::Deadline), TurnOutcome::Interrupted) => deadline_failure(),
            (_, outcome) => outcome,
        }
    }
}

/// How an attempt cut off at [`TURN_DEADLINE`] ends. The turn continues
/// in a new attempt unless the owner stopped it or a cap is reached
/// ([`crate::db::limit_cuts`]).
pub fn deadline_failure() -> TurnOutcome {
    TurnOutcome::Failed {
        code: FailureCode::TurnDeadline,
        message: limit_message(0),
    }
}

/// Why a turn failed when the limit cut it after `continued` continuations.
pub fn limit_message(continued: i64) -> String {
    let after = match continued {
        0 => String::new(),
        1 => " after continuing once".to_owned(),
        n => format!(" after continuing {n} times"),
    };
    format!(
        "Stopped at the {}-minute attempt limit{after}. Background jobs it started may still be running.",
        TURN_DEADLINE.as_secs() / 60
    )
}

/// What else.bot tells a running provider [`DEADLINE_WARNING`] before the
/// limit cuts its attempt.
pub fn deadline_warning() -> String {
    format!(
        "[else.bot] {} minutes are left in this attempt. Move long remaining work into background \
         jobs (computer_job_start) and park on them with computer_job and wait: true, or write \
         your progress so far in a reply. When the limit cuts the attempt, the request \
         continues in a new one with that progress.",
        DEADLINE_WARNING.as_secs() / 60
    )
}

/// What a protocol reads from one provider message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event<T> {
    /// The provider acknowledged the turn, with the model it reported.
    Accepted {
        model: String,
        native_turn: String,
    },
    /// The provider switched to this model for the rest of the turn.
    Rerouted {
        model: String,
        native_turn: String,
    },
    Text(String),
    /// Later text starts a new assistant message.
    EndMessage,
    /// An else.bot tool parked the turn.
    Parked,
    /// Tokens the provider reported, added to the turn's totals.
    Usage(crate::app::providers::Usage),
    /// Subscription windows the provider reported for the owner's account.
    Limits(Vec<crate::app::providers::LimitWindow>),
    /// The provider compacted the native session's context, which may drop
    /// the memory, skills, and context blocks it was given.
    Compacted,
    /// The provider took the steered owner message of this turn ID.
    Steered(uuid::Uuid),
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

    /// The input that adds `steer` to the running turn, for
    /// [`Channel::send`]; its echo carries the steered turn's ID. `None`
    /// when this provider takes no input mid-turn: the message waits for the
    /// reply's next attempt.
    fn steer_input(&self, steer: &Steer) -> Option<Value> {
        let _ = steer;
        None
    }

    /// The input for `steer` reached the provider; its echo is now owed.
    fn steer_sent(&mut self, steer: &Steer) {
        let _ = steer;
    }

    /// The input that gives the running turn else.bot's own `text`, for
    /// [`Channel::send`]; its echo is not an owner message. `None` when this
    /// provider takes no input mid-turn.
    fn notice_input(&self, text: &str) -> Option<Value> {
        let _ = text;
        None
    }

    /// The input from [`Protocol::notice_input`] reached the provider.
    fn notice_sent(&mut self, input: &Value) {
        let _ = input;
    }

    /// Whether the provider still owes an answer to a steered message after
    /// `terminal`, so the attempt keeps reading instead of ending.
    fn continues(&self, terminal: &Self::Terminal) -> bool {
        let _ = terminal;
        false
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

    /// Give the running provider one input from [`Protocol::steer_input`].
    async fn send(&mut self, input: &Value) -> Result<(), SendError> {
        let _ = input;
        Err(SendError::Refused(
            "this provider takes no input while it runs".into(),
        ))
    }

    /// The project directory, where a steered message's files are staged.
    fn project(&self) -> Option<&Path> {
        None
    }
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

/// Tell the provider its attempt is about to be cut, when it takes input
/// mid-turn, giving up at `until`. A refused or lost warning only costs the
/// warning, since the cut attempt continues either way; a write that may be
/// partial closes steering, as a lost steered message does.
async fn warn<C, P>(
    channel: &mut C,
    protocol: &mut P,
    steering: &mut Steering,
    stop: &Stop,
    until: Instant,
) where
    C: Channel,
    P: Protocol<Message = C::Message>,
{
    if *steering == Steering::Closed {
        return;
    }
    let Some(input) = protocol.notice_input(&deadline_warning()) else {
        return;
    };
    let sent = tokio::select! {
        sent = channel.send(&input) => sent,
        () = stop.requested() => Err(SendError::Unknown("stopped while writing".into())),
        () = tokio::time::sleep_until(until) => Err(SendError::Unknown("timed out".into())),
    };
    match sent {
        Ok(()) => protocol.notice_sent(&input),
        Err(SendError::Refused(why)) => {
            tracing::info!(%why, "the provider refused the attempt limit warning");
        }
        Err(SendError::Unknown(why)) => {
            tracing::warn!(%why, "writing the attempt limit warning failed");
            *steering = Steering::Closed;
        }
    }
}

fn reported(model: &str) -> serde_json::Value {
    json!({ "model": model, "reasoning_effort": null, "service_tier": null })
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
    let mut steering = Steering::Open;
    // A message that joined before the provider took the turn had no wake.
    if turn.accepted() {
        steering.give(channel, protocol, sink, stop).await;
    }
    loop {
        let read = tokio::select! {
            () = stop.escalated() => return Ended::Abandoned(life.abandoned(not_stopped())),
            () = stop.requested(), if life.running() => {
                if life.stop(StopCause::Owner) && !interrupt(channel, stop).await {
                    return Ended::Abandoned(life.abandoned(not_stopped()));
                }
                continue;
            }
            () = stop.steered(), if life.takes_steers() => {
                steering.give(channel, protocol, sink, stop).await;
                continue;
            }
            () = tokio::time::sleep_until(life.wake_at()) => {
                match life.on_timer() {
                    Tick::Interrupt if !interrupt(channel, stop).await => {
                        return Ended::Abandoned(life.abandoned(not_stopped()));
                    }
                    Tick::GiveUp => return Ended::Abandoned(life.abandoned(not_stopped())),
                    Tick::Warn => warn(channel, protocol, &mut steering, stop, life.wake_at()).await,
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
                    let accepted = turn.accept(sink, &reported(&model), &native_turn).await;
                    if accepted.is_ok() && life.takes_steers() {
                        steering.give(channel, protocol, sink, stop).await;
                    }
                    accepted
                }
                Event::Rerouted { model, native_turn } if turn.accepted() => {
                    sink.reported(&reported(&model), &native_turn).await
                }
                Event::Rerouted { .. } => Ok(()),
                Event::Text(text) => sink.text(&text).await,
                Event::EndMessage => sink.end_message().await,
                Event::Parked => {
                    life.parked();
                    Ok(())
                }
                Event::Usage(usage) => {
                    sink.usage(usage).await;
                    Ok(())
                }
                Event::Limits(windows) => {
                    sink.limits(windows).await;
                    Ok(())
                }
                Event::Compacted => turn.compacted(sink).await,
                Event::Steered(turn_id) => sink.steered(turn_id).await,
                Event::Finished(terminal)
                    if life.takes_steers() && protocol.continues(&terminal) =>
                {
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

/// Read a tool-less one-shot attempt, such as a checkpoint or a memory
/// summary, to its terminal result: its text, with messages separated by a
/// blank line, and the terminal. A violation or an early exit is an error.
/// The caller bounds it in time and cancels it.
pub async fn collect<C, P>(
    channel: &mut C,
    protocol: &mut P,
    label: &str,
) -> Result<(String, P::Terminal), String>
where
    C: Channel,
    P: Protocol<Message = C::Message>,
{
    let mut text = String::new();
    loop {
        let Some(message) = channel.next().await? else {
            return Err(format!("{label} exited before its terminal result."));
        };
        for event in protocol.on(&message).map_err(|v| v.to_string())? {
            match event {
                Event::Text(delta) => text.push_str(&delta),
                Event::EndMessage if !text.is_empty() => text.push_str("\n\n"),
                Event::Finished(terminal) => return Ok((text, terminal)),
                _ => {}
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
            let reason = last_lines(&stderr.tail().await, 1).concat();
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
        sent: Vec<Value>,
        send_hangs: bool,
        send_refuses: bool,
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

        async fn send(&mut self, input: &Value) -> Result<(), SendError> {
            if self.send_hangs {
                std::future::pending::<()>().await;
            }
            if self.send_refuses {
                return Err(SendError::Refused("no active turn to steer".into()));
            }
            self.sent.push(input.clone());
            Ok(())
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
                "reroute" => vec![Event::Rerouted {
                    model: "f".into(),
                    native_turn: "t".into(),
                }],
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
            sent: Vec::new(),
            send_hangs: false,
            send_refuses: false,
        }
    }

    #[tokio::test]
    async fn messages_reach_a_slow_sink_in_order_and_a_result_ends_the_turn() {
        let input = input();
        let sink = Log::default();
        let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n".into()));
        let mut channel = replay(&[
            json!({"kind": "reroute"}),
            json!({"kind": "accept"}),
            json!({"kind": "text", "text": "a"}),
            json!({"kind": "reroute"}),
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
            [
                "running",
                "session",
                "reported:\"m\":t",
                "text:a",
                "reported:\"f\":t",
                "text:b"
            ]
        );
        assert_eq!(channel.interrupts, 0);
    }

    /// [`Plain`] that owes an answer to one steered message until a
    /// `steered` message echoes it.
    struct Owing {
        owed: bool,
    }

    impl Protocol for Owing {
        type Message = Value;
        type Terminal = ();

        fn on(&mut self, message: &Value) -> Result<Vec<Event<()>>, Violation> {
            if message["kind"] == "steered" {
                self.owed = false;
                return Ok(vec![Event::Steered(uuid::Uuid::nil())]);
            }
            Plain.on(message)
        }

        fn outcome(&self, (): (), stopping: Option<StopCause>, accepted: bool) -> TurnOutcome {
            Plain.outcome((), stopping, accepted)
        }

        fn continues(&self, (): &()) -> bool {
            self.owed
        }

        fn steer_input(&self, steer: &Steer) -> Option<Value> {
            Some(json!(steer.text))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_cuts_off_a_steer_write_that_never_completes() {
        let input = input();
        let sink = Log::default();
        let turn_id = uuid::Uuid::new_v4();
        *sink.steers.lock().unwrap() = vec![
            steer(turn_id, "If they agreed, send the deal."),
            steer(uuid::Uuid::new_v4(), "What is this?"),
        ];
        let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n".into()));
        let mut channel = replay(&[json!({"kind": "accept"})]);
        channel.send_hangs = true;
        let stop = Stop::default();
        stop.steer();
        let owner = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            owner.request();
        });
        let started = Instant::now();
        let ended = drive(
            &mut channel,
            &mut Owing { owed: false },
            &mut turn,
            &sink,
            &stop,
            "P",
        )
        .await;
        assert!(matches!(ended, Ended::Abandoned(_)), "{ended:?}");
        assert_eq!(channel.interrupts, 1, "the stop reached the provider");
        assert!(started.elapsed() <= Duration::from_millis(50) + INTERRUPT_GRACE);
        let marked: Vec<_> = sink
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("steer-sending"))
            .collect();
        assert_eq!(
            marked,
            [format!("steer-sending:{turn_id}")],
            "only the message being written is marked; the rest stay pending"
        );
    }

    fn steer(turn_id: uuid::Uuid, text: &str) -> Steer {
        Steer {
            turn_id,
            text: text.into(),
            parts: Vec::new(),
            files: Vec::new(),
        }
    }

    /// Codex answers `turn/steer` with an error once its turn ended: the
    /// message waits for the next attempt, and no later one overtakes it.
    #[tokio::test]
    async fn a_refused_steer_waits_and_nothing_after_it_is_given() {
        let input = input();
        let sink = Log::default();
        let (first, second) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        *sink.steers.lock().unwrap() = vec![
            steer(first, "If they agreed, send the deal."),
            steer(second, "Schedule it too."),
        ];
        let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n".into()));
        let mut channel = replay(&[json!({"kind": "accept"}), json!({"kind": "end"})]);
        channel.send_refuses = true;
        let ended = drive(
            &mut channel,
            &mut Owing { owed: false },
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
        let steering: Vec<_> = sink
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("steer"))
            .collect();
        assert_eq!(
            steering,
            [
                format!("steer-sending:{first}"),
                format!("steer-refused:{first}")
            ]
        );
        assert!(channel.sent.is_empty());
    }

    /// A message that joined before the provider took the turn is given
    /// as soon as it does, with no wake.
    #[tokio::test]
    async fn a_message_that_joined_before_acceptance_is_given_on_acceptance() {
        let input = input();
        let sink = Log::default();
        let turn_id = uuid::Uuid::new_v4();
        *sink.steers.lock().unwrap() = vec![steer(turn_id, "Schedule it too.")];
        let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n".into()));
        let mut channel = replay(&[json!({"kind": "accept"}), json!({"kind": "end"})]);
        drive(
            &mut channel,
            &mut Owing { owed: false },
            &mut turn,
            &sink,
            &Stop::default(),
            "P",
        )
        .await;
        assert_eq!(channel.sent, [json!("Schedule it too.")]);
    }

    #[tokio::test]
    async fn a_result_that_still_owes_a_steered_message_keeps_the_attempt_reading() {
        let input = input();
        let sink = Log::default();
        let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n".into()));
        let mut channel = replay(&[
            json!({"kind": "accept"}),
            json!({"kind": "text", "text": "a"}),
            json!({"kind": "end"}),
            json!({"kind": "steered"}),
            json!({"kind": "text", "text": "b"}),
            json!({"kind": "end"}),
        ]);
        let ended = drive(
            &mut channel,
            &mut Owing { owed: true },
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
        assert!(
            channel.messages.is_empty(),
            "read through the second result"
        );
        assert_eq!(
            sink.calls(),
            [
                "running".to_owned(),
                "session".into(),
                "reported:\"m\":t".into(),
                "text:a".into(),
                format!("steered:{}", uuid::Uuid::nil()),
                "text:b".into(),
            ]
        );
    }

    #[tokio::test]
    async fn a_parked_turn_ends_at_its_result_even_while_a_steer_is_owed() {
        let input = input();
        let sink = Log::default();
        let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n".into()));
        let mut channel = replay(&[
            json!({"kind": "accept"}),
            json!({"kind": "park"}),
            json!({"kind": "end"}),
            json!({"kind": "steered"}),
        ]);
        let ended = drive(
            &mut channel,
            &mut Owing { owed: true },
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
        assert_eq!(channel.messages.len(), 1, "stopped at the first result");
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
            Ended::Abandoned(TurnOutcome::Failed {
                code: FailureCode::PolicyViolation,
                ..
            })
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
    async fn the_deadline_interrupts_and_a_confirmed_interruption_is_a_cut() {
        let mut life = Lifecycle::start();
        tokio::time::advance(TURN_DEADLINE).await;
        assert_eq!(life.on_timer(), Tick::Interrupt);
        assert!(matches!(
            life.settled(TurnOutcome::Interrupted),
            TurnOutcome::Failed {
                code: FailureCode::TurnDeadline,
                ..
            }
        ));
        assert_eq!(
            life.settled(TurnOutcome::Completed),
            TurnOutcome::Completed,
            "a turn that completed as the deadline fired is not cut"
        );
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
    async fn the_provider_is_warned_once_five_minutes_before_the_deadline() {
        let mut life = Lifecycle::start();
        tokio::time::advance(Duration::from_secs(25 * 60 - 1)).await;
        assert_eq!(life.on_timer(), Tick::Wait);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(life.on_timer(), Tick::Warn);
        assert_eq!(life.on_timer(), Tick::Wait, "one warning per attempt");
        assert_eq!(life.wake_at(), Instant::now() + Duration::from_secs(5 * 60));
        tokio::time::advance(Duration::from_secs(5 * 60)).await;
        assert_eq!(life.on_timer(), Tick::Interrupt);
    }

    /// [`Plain`] that takes else.bot's notices mid-turn.
    struct Noticed;

    impl Protocol for Noticed {
        type Message = Value;
        type Terminal = ();

        fn on(&mut self, message: &Value) -> Result<Vec<Event<()>>, Violation> {
            Plain.on(message)
        }

        fn outcome(&self, (): (), stopping: Option<StopCause>, accepted: bool) -> TurnOutcome {
            Plain.outcome((), stopping, accepted)
        }

        fn notice_input(&self, text: &str) -> Option<Value> {
            Some(json!(text))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_provider_gets_the_warning_before_the_deadline_cuts_it() {
        let input = input();
        let sink = Log::default();
        let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n".into()));
        let mut channel = replay(&[json!({"kind": "accept"})]);
        let ended = drive(
            &mut channel,
            &mut Noticed,
            &mut turn,
            &sink,
            &Stop::default(),
            "P",
        )
        .await;
        assert!(matches!(
            ended,
            Ended::Abandoned(TurnOutcome::Uncertain { .. })
        ));
        assert_eq!(channel.interrupts, 1);
        let [warning] = &channel.sent[..] else {
            panic!("one warning: {:?}", channel.sent);
        };
        let warning = warning.as_str().unwrap();
        assert!(
            warning.starts_with("[else.bot] 5 minutes are left in this attempt.")
                && warning.contains("computer_job_start"),
            "{warning}"
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
