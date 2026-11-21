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
