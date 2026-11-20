//! The upgrades the owner started from the desktop, and Upgrade all.
//!
//! One upgrade runs per request ([`ComputerManager::start_upgrade`]);
//! Upgrade all ([`ComputerManager::upgrade_all`]) queues every outdated
//! computer and one worker upgrades them in order, one at a time, so the
//! replacement computer of each fits in the budget. A computer that is
//! busy (a guest job, a lease such as an open desktop, a full budget, or a
//! backup or update) moves to the back as waiting and is retried after the
//! others; a failure is recorded on that computer and the queue goes on.
//! The owner can cancel the computers still queued or waiting; a running
//! upgrade finishes, or fails and keeps the old computer, as before.
//!
//! All of it is in memory, like a single upgrade's status: a restart
//! forgets the queue, and maintenance removes an interrupted replacement.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use uuid::Uuid;

use super::{ComputerError, ComputerManager};
use crate::maintenance::Gate;

/// What the owner sees of a bot's upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeStatus {
    /// In the Upgrade all queue; `position` counts from 1.
    Queued {
        position: usize,
    },
    /// In the queue, skipped for now because the computer was busy.
    Waiting(String),
    Running,
    Failed(String),
}

/// The upgrade of a bot that is not in the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Started {
    Running,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    bot_id: Uuid,
    /// Why the last attempt was put off; `None` until one was.
    waiting: Option<String>,
}

/// How an attempt ended, as the queue records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Ended {
    Upgraded,
    Failed(String),
    /// Busy: back of the queue, retried after the others, unless the
    /// owner cancelled the queue since the attempt began (`round`).
    Deferred {
        reason: String,
        round: u64,
    },
}

/// The worker's next move.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Run {
        bot_id: Uuid,
        round: u64,
    },
    /// Only waiting computers are left: retry these after a pause.
    Retry(Vec<Uuid>),
    /// The queue is empty and the worker has ended.
    Finished,
}

/// The queue's single worker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Worker {
    #[default]
    Idle,
    Draining,
}

/// A bot is either in `queue` or has a `started` upgrade, never both.
#[derive(Debug, Default)]
pub(super) struct Upgrades {
    started: HashMap<Uuid, Started>,
    queue: VecDeque<Entry>,
    worker: Worker,
    /// Counts cancellations, so an attempt that began before one is not
    /// queued again.
    round: u64,
}

impl Upgrades {
    fn position(&self, bot_id: Uuid) -> Option<usize> {
        self.queue.iter().position(|entry| entry.bot_id == bot_id)
    }

    pub(super) fn status(&self, bot_id: Uuid) -> Option<UpgradeStatus> {
        match self.started.get(&bot_id) {
            Some(Started::Running) => return Some(UpgradeStatus::Running),
            Some(Started::Failed(error)) => return Some(UpgradeStatus::Failed(error.clone())),
            None => {}
        }
        let index = self.position(bot_id)?;
        Some(match &self.queue[index].waiting {
            Some(reason) => UpgradeStatus::Waiting(reason.clone()),
            None => UpgradeStatus::Queued {
                position: index + 1,
            },
        })
    }

    pub(super) fn statuses(&self) -> HashMap<Uuid, UpgradeStatus> {
        self.started
            .keys()
            .chain(self.queue.iter().map(|entry| &entry.bot_id))
            .filter_map(|bot_id| Some((*bot_id, self.status(*bot_id)?)))
            .collect()
    }

    /// Start `bot_id`'s upgrade now, taking it out of the queue. Refused
    /// while one runs.
    pub(super) fn claim(&mut self, bot_id: Uuid) -> bool {
        if self.started.get(&bot_id) == Some(&Started::Running) {
            return false;
        }
        if let Some(index) = self.position(bot_id) {
            self.queue.remove(index);
        }
        self.started.insert(bot_id, Started::Running);
        true
    }

    pub(super) fn end(&mut self, bot_id: Uuid, ended: Ended) {
        match ended {
            Ended::Upgraded => {
                self.started.remove(&bot_id);
            }
            Ended::Failed(error) => {
                self.started.insert(bot_id, Started::Failed(error));
            }
            Ended::Deferred { reason, round } => {
                self.started.remove(&bot_id);
                if round == self.round {
                    self.queue.push_back(Entry {
                        bot_id,
                        waiting: Some(reason),
                    });
                }
            }
        }
    }

    /// Queue `bots` behind the ones already queued, skipping any that is
    /// queued or upgrading. Returns every queued bot, and whether a worker
    /// must start.
    fn enqueue(&mut self, bots: Vec<Uuid>) -> (Vec<Uuid>, bool) {
        for bot_id in bots {
            if self.started.get(&bot_id) == Some(&Started::Running)
                || self.position(bot_id).is_some()
            {
                continue;
            }
            self.started.remove(&bot_id);
            self.queue.push_back(Entry {
                bot_id,
                waiting: None,
            });
        }
        let start = self.worker == Worker::Idle && !self.queue.is_empty();
        if start {
            self.worker = Worker::Draining;
        }
        (self.queue.iter().map(|entry| entry.bot_id).collect(), start)
    }

    fn next(&mut self) -> Step {
        if let Some(index) = self.queue.iter().position(|entry| entry.waiting.is_none()) {
            let bot_id = self.queue.remove(index).expect("in range").bot_id;
            self.started.insert(bot_id, Started::Running);
            return Step::Run {
                bot_id,
                round: self.round,
            };
        }
        if self.queue.is_empty() {
            self.worker = Worker::Idle;
            return Step::Finished;
        }
        Step::Retry(self.queue.iter().map(|entry| entry.bot_id).collect())
    }

    /// Take a waiting bot out of the queue to retry it, unless the owner
    /// cancelled or started it meanwhile.
    fn retry(&mut self, bot_id: Uuid) -> Option<u64> {
        match self.position(bot_id) {
            Some(index) if self.queue[index].waiting.is_some() => {
                self.queue.remove(index);
                self.started.insert(bot_id, Started::Running);
                Some(self.round)
            }
            _ => None,
        }
    }

    fn cancel(&mut self) -> Vec<Uuid> {
        self.round += 1;
        self.queue.drain(..).map(|entry| entry.bot_id).collect()
    }
}

impl ComputerManager {
    pub async fn upgrade_status(&self, bot_id: Uuid) -> Option<UpgradeStatus> {
        self.upgrades
            .call(move |upgrades| upgrades.status(bot_id))
            .await
    }

    pub async fn upgrade_statuses(&self) -> HashMap<Uuid, UpgradeStatus> {
        self.upgrades.call(|upgrades| upgrades.statuses()).await
    }

    pub(super) async fn upgrading(&self, bot_id: Uuid) -> bool {
        self.upgrade_status(bot_id).await == Some(UpgradeStatus::Running)
    }

    /// Queue every created computer of an active bot that is not on the
    /// configured image, and start the worker unless one runs. Returns the
    /// bots now in the queue, in order.
    pub async fn upgrade_all(
        self: &Arc<Self>,
        gate: Arc<Gate>,
    ) -> Result<Vec<Uuid>, ComputerError> {
        self.can_start_upgrade()?;
        let outdated: Vec<Uuid> = sqlx::query_scalar(
            "SELECT c.bot_id FROM bot_computers c JOIN bots b ON b.id = c.bot_id
             WHERE b.status = 'active' AND c.observed_state <> 'absent' AND c.image <> ?1
             ORDER BY b.created_at, b.id",
        )
        .bind(&self.config.image)
        .fetch_all(&self.pool)
        .await?;
        // The worker starts inside the actor's operation, which finishes
        // even if this request is cancelled, so a queue never lacks one.
        let this = self.clone();
        Ok(self
            .upgrades
            .call(move |upgrades| {
                let (queued, start) = upgrades.enqueue(outdated);
                if start {
                    let worker = this.clone();
                    this.upgrade_tasks
                        .spawn(async move { worker.drain(gate).await });
                }
                queued
            })
            .await)
    }

    /// Take the computers still queued or waiting out of the queue.
    pub async fn cancel_upgrade_all(&self) -> Vec<Uuid> {
        self.upgrades.call(|upgrades| upgrades.cancel()).await
    }

    async fn drain(self: Arc<Self>, gate: Arc<Gate>) {
        loop {
            match self.upgrades.call(|upgrades| upgrades.next()).await {
                Step::Finished => return,
                Step::Run { bot_id, round } => self.run_queued(bot_id, round, &gate).await,
                Step::Retry(waiting) => {
                    tokio::select! {
                        () = tokio::time::sleep(self.config.upgrade_retry) => {}
                        () = self.upgrade_stop.cancelled() => return,
                    }
                    for bot_id in waiting {
                        let claimed = self
                            .upgrades
                            .call(move |upgrades| upgrades.retry(bot_id))
                            .await;
                        if let Some(round) = claimed {
                            self.run_queued(bot_id, round, &gate).await;
                        }
                    }
                }
            }
        }
    }

    /// One queued upgrade. Unlike a single upgrade, it leaves an open
    /// desktop alone (the owner may be using it) and does not wait for the
    /// bot's operation lock: the computer waits instead.
    async fn run_queued(&self, bot_id: Uuid, round: u64, gate: &Arc<Gate>) {
        let deferred = |reason: &str| Ended::Deferred {
            reason: reason.into(),
            round,
        };
        let ended = match (gate.enter(), self.try_op(bot_id).await) {
            (None, _) => deferred("A backup or an update is running."),
            (_, None) => deferred("The computer is busy with another operation."),
            (Some(_admission), Some(op)) => {
                let upgraded = tokio::select! {
                    upgraded = self.upgrade_image_holding(op, bot_id, &self.config.image, None) => upgraded,
                    () = self.upgrade_stop.cancelled() => Err(ComputerError::Refused("the API stopped during the upgrade".into())),
                };
                match upgraded {
                    Ok(_) | Err(ComputerError::AlreadyUses(_)) => Ended::Upgraded,
                    Err(err) => {
                        let defers = match &err {
                            ComputerError::Busy(busy) => busy.defers(),
                            _ => None,
                        };
                        match defers {
                            Some(reason) => deferred(reason),
                            None => {
                                tracing::warn!(bot = %bot_id, error = %err, "queued computer upgrade failed");
                                Ended::Failed(err.to_string())
                            }
                        }
                    }
                }
            }
        };
        self.upgrades
            .call(move |upgrades| upgrades.end(bot_id, ended))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bots(n: u128) -> Vec<Uuid> {
        (1..=n).map(Uuid::from_u128).collect()
    }

    fn run(upgrades: &mut Upgrades) -> (Uuid, u64) {
        match upgrades.next() {
            Step::Run { bot_id, round } => (bot_id, round),
            other => panic!("expected a run, got {other:?}"),
        }
    }

    fn busy(round: u64) -> Ended {
        Ended::Deferred {
            reason: "busy".into(),
            round,
        }
    }

    #[test]
    fn the_queue_runs_in_order_and_retries_busy_computers_after_the_others() {
        let [a, b, c] = bots(3)[..] else {
            unreachable!()
        };
        let mut upgrades = Upgrades::default();
        assert_eq!(upgrades.enqueue(vec![a, b, c]), (vec![a, b, c], true));
        assert_eq!(
            upgrades.status(c),
            Some(UpgradeStatus::Queued { position: 3 })
        );
        let (first, round) = run(&mut upgrades);
        assert_eq!(first, a);
        assert_eq!(upgrades.status(a), Some(UpgradeStatus::Running));
        assert_eq!(
            upgrades.status(b),
            Some(UpgradeStatus::Queued { position: 1 })
        );
        upgrades.end(a, busy(round));
        assert_eq!(
            upgrades.status(a),
            Some(UpgradeStatus::Waiting("busy".into()))
        );
        assert_eq!(run(&mut upgrades).0, b);
        upgrades.end(b, Ended::Failed("boom".into()));
        assert_eq!(run(&mut upgrades).0, c);
        upgrades.end(c, Ended::Upgraded);
        assert_eq!(upgrades.status(c), None);
        assert_eq!(
            upgrades.status(b),
            Some(UpgradeStatus::Failed("boom".into()))
        );
        assert_eq!(upgrades.next(), Step::Retry(vec![a]));
        assert_eq!(upgrades.retry(a), Some(round));
        assert_eq!(upgrades.retry(a), None, "a bot is retried once per round");
        upgrades.end(a, Ended::Upgraded);
        assert_eq!(upgrades.next(), Step::Finished);
        assert_eq!(
            upgrades.enqueue(vec![a]),
            (vec![a], true),
            "a finished worker lets the next upgrade start one"
        );
    }

    #[test]
    fn requeueing_skips_queued_and_running_bots_and_starts_one_worker() {
        let [a, b] = bots(2)[..] else { unreachable!() };
        let mut upgrades = Upgrades::default();
        assert!(upgrades.claim(a));
        assert!(!upgrades.claim(a), "one upgrade per bot");
        upgrades.end(a, Ended::Failed("boom".into()));
        assert_eq!(upgrades.enqueue(vec![b]), (vec![b], true));
        assert_eq!(
            upgrades.enqueue(vec![a, b]),
            (vec![b, a], false),
            "a failed bot is queued again, a queued one is not, and the worker runs already"
        );
        assert_eq!(
            upgrades.status(a),
            Some(UpgradeStatus::Queued { position: 2 })
        );
        assert!(
            upgrades.claim(a),
            "a single upgrade takes a bot out of the queue"
        );
        assert_eq!(upgrades.statuses().len(), 2);
        assert_eq!(run(&mut upgrades).0, b);
        assert_eq!(upgrades.enqueue(vec![a, b]), (vec![], false));
    }

    #[test]
    fn cancelling_empties_the_queue_and_keeps_a_busy_running_upgrade_out_of_it() {
        let [a, b, c] = bots(3)[..] else {
            unreachable!()
        };
        let mut upgrades = Upgrades::default();
        upgrades.enqueue(vec![a, b, c]);
        let (_, round) = run(&mut upgrades);
        upgrades.end(a, busy(round));
        assert_eq!(run(&mut upgrades).0, b);
        assert_eq!(upgrades.cancel(), vec![c, a]);
        assert_eq!(upgrades.status(a), None);
        assert_eq!(upgrades.retry(a), None, "a cancelled bot is not retried");
        assert_eq!(upgrades.status(b), Some(UpgradeStatus::Running));
        upgrades.end(b, busy(round));
        assert_eq!(
            upgrades.status(b),
            None,
            "the running attempt began before the cancel"
        );
        assert_eq!(upgrades.next(), Step::Finished);
    }

    #[test]
    fn only_contention_puts_a_computer_off() {
        use super::super::{Busy, Observed};
        assert!(Busy::InUse.defers().is_some());
        assert!(Busy::ActiveJob.defers().is_some());
        assert!(Busy::State(Observed::Stopping).defers().is_some());
        assert!(Busy::BudgetFull.defers().is_some());
        assert_eq!(Busy::NotStopped.defers(), None);
        assert_eq!(Busy::State(Observed::Missing).defers(), None);
        assert_eq!(Busy::State(Observed::Error).defers(), None);
        assert_eq!(Busy::CheckpointFailed("boom".into()).defers(), None);
    }
}
