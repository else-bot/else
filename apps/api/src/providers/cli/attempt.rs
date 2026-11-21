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
mod tests;
