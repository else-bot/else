//! Webhook rules: a bot's saved task that an event wakes it for, at a
//! human pace. An event only requests a wake; the wake runs once the pace
//! allows it, and events that arrive before then coalesce into that one
//! wake. A pace faster than [`Pace::DEFAULT`] needs the owner's approval
//! and never goes below [`FASTEST_INTERVAL_MINUTES`].

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};

/// Webhooks one bot may have at once (deleted ones do not count).
pub const MAX_PER_BOT: i64 = 10;
pub const FASTEST_INTERVAL_MINUTES: i64 = 1;
pub const SLOWEST_INTERVAL_MINUTES: i64 = 24 * 60;
pub const MAX_DAILY_WAKES: i64 = 24 * 60;
pub const WINDOW: Duration = Duration::hours(24);
const TOKEN_PREFIX: &str = "whk_";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidPace {
    #[error(
        "min_interval_minutes must be {FASTEST_INTERVAL_MINUTES} to {SLOWEST_INTERVAL_MINUTES}"
    )]
    Interval,
    #[error("daily_limit must be 1 to {MAX_DAILY_WAKES}")]
    Daily,
}

/// How often a webhook may wake its bot: at least `min_interval_minutes`
/// apart and at most `daily_limit` wakes in any 24 hours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pace {
    min_interval_minutes: i64,
    daily_limit: i64,
}

impl Pace {
    pub const DEFAULT: Pace = Pace {
        min_interval_minutes: 15,
        daily_limit: 24,
    };

    pub fn new(min_interval_minutes: i64, daily_limit: i64) -> Result<Self, InvalidPace> {
        if !(FASTEST_INTERVAL_MINUTES..=SLOWEST_INTERVAL_MINUTES).contains(&min_interval_minutes) {
            return Err(InvalidPace::Interval);
        }
        if !(1..=MAX_DAILY_WAKES).contains(&daily_limit) {
            return Err(InvalidPace::Daily);
        }
        Ok(Self {
            min_interval_minutes,
            daily_limit,
        })
    }

    /// `base` with the given fields replaced.
    pub fn with(
        base: Pace,
        min_interval_minutes: Option<i64>,
        daily_limit: Option<i64>,
    ) -> Result<Self, InvalidPace> {
        Self::new(
            min_interval_minutes.unwrap_or(base.min_interval_minutes),
            daily_limit.unwrap_or(base.daily_limit),
        )
    }

    pub fn min_interval_minutes(self) -> i64 {
        self.min_interval_minutes
    }

    pub fn daily_limit(self) -> i64 {
        self.daily_limit
    }

    /// Faster than the default in either dimension: the owner must approve.
    pub fn needs_approval(self) -> bool {
        self.min_interval_minutes < Self::DEFAULT.min_interval_minutes
            || self.daily_limit > Self::DEFAULT.daily_limit
    }

    /// Changing from `current` to this pace needs the owner's approval: it
    /// is faster than the default and than `current` in some dimension.
    pub fn needs_approval_over(self, current: Pace) -> bool {
        self.needs_approval()
            && (self.min_interval_minutes < current.min_interval_minutes
                || self.daily_limit > current.daily_limit)
    }

    /// The earliest time a wake may run, given the wakes of the last
    /// [`WINDOW`] in ascending order and when the last one finished. The
    /// interval counts from both, so a wake delayed in the queue never runs
    /// right after the one before it.
    pub fn next_allowed(
        self,
        wakes: &[DateTime<Utc>],
        last_finished: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> DateTime<Utc> {
        let interval = Duration::minutes(self.min_interval_minutes);
        let mut at = now;
        for last in wakes.last().into_iter().chain(last_finished.as_ref()) {
            at = at.max(*last + interval);
        }
        let limit = self.daily_limit as usize;
        if wakes.len() >= limit {
            at = at.max(wakes[wakes.len() - limit] + WINDOW);
        }
        at
    }
}

/// The owner-facing explanation of a faster pace's cost.
pub fn approval_context(name: &str, pace: Pace) -> String {
    format!(
        "The webhook \"{name}\" would be allowed to wake this bot every {} minute(s), up to {} \
         times a day. Each wake runs a full bot turn that uses your provider plan or API \
         credits. A script that fires constantly would use all of them: about {} turns a day. \
         The default is every {} minutes, up to {} a day. Approve only if this event needs a \
         fast reaction; you can delete the webhook to stop it.",
        pace.min_interval_minutes,
        pace.daily_limit,
        pace.daily_limit.min(24 * 60 / pace.min_interval_minutes),
        Pace::DEFAULT.min_interval_minutes,
        Pace::DEFAULT.daily_limit,
    )
}

/// A new trigger token and its digest. Only the digest is stored.
pub fn new_token() -> (String, [u8; 32]) {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("system random source");
    let token = format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes));
    let digest = token_digest(&token);
    (token, digest)
}

pub fn token_digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::UNIX_EPOCH + Duration::minutes(minutes)
    }

    #[test]
    fn a_first_wake_runs_now() {
        assert_eq!(Pace::DEFAULT.next_allowed(&[], None, at(5)), at(5));
    }

    #[test]
    fn wakes_keep_the_minimum_interval() {
        assert_eq!(Pace::DEFAULT.next_allowed(&[at(0)], None, at(1)), at(15));
        assert_eq!(Pace::DEFAULT.next_allowed(&[at(0)], None, at(20)), at(20));
    }

    #[test]
    fn a_wake_delayed_in_the_queue_rests_a_full_interval_after_it_finishes() {
        assert_eq!(
            Pace::DEFAULT.next_allowed(&[at(0)], Some(at(20)), at(21)),
            at(35)
        );
    }

    #[test]
    fn the_daily_limit_waits_for_the_oldest_wake_to_leave_the_window() {
        let pace = Pace::new(1, 3).unwrap();
        let wakes = [at(0), at(10), at(20)];
        assert_eq!(pace.next_allowed(&wakes, None, at(30)), at(24 * 60));
        assert_eq!(pace.next_allowed(&wakes[1..], None, at(30)), at(30));
    }

    #[test]
    fn faster_than_default_needs_approval_and_has_a_floor() {
        assert!(!Pace::DEFAULT.needs_approval());
        assert!(Pace::new(5, 24).unwrap().needs_approval());
        assert!(Pace::new(15, 48).unwrap().needs_approval());
        assert!(!Pace::new(60, 4).unwrap().needs_approval());
        let fast = Pace::new(5, 24).unwrap();
        assert!(fast.needs_approval_over(Pace::DEFAULT));
        assert!(!fast.needs_approval_over(fast), "keeping an approved pace");
        assert!(Pace::new(1, 24).unwrap().needs_approval_over(fast));
        assert_eq!(Pace::new(0, 24), Err(InvalidPace::Interval));
        assert_eq!(Pace::new(15, 0), Err(InvalidPace::Daily));
        assert_eq!(Pace::new(15, MAX_DAILY_WAKES + 1), Err(InvalidPace::Daily));
    }

    #[test]
    fn tokens_are_prefixed_and_digested() {
        let (token, digest) = new_token();
        assert!(token.starts_with(TOKEN_PREFIX));
        assert_eq!(token_digest(&token), digest);
    }
}
