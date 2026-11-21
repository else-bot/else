//! The one rule every settings pane applies to replies: each fetch takes an
//! attempt from [`Loaded::begin`], and only the reply to the latest attempt
//! lands. A local change that a reply from before it would undo calls
//! [`Loaded::supersede`].

use uuid::Uuid;

/// A fetched value that stays on screen while a refresh runs and after a
/// refresh fails.
pub(super) enum Loaded<T> {
    Loading(Uuid),
    Failed {
        error: String,
        /// The fetch retrying it, while the error stays known.
        retry: Option<Uuid>,
    },
    Ready {
        value: T,
        refresh: Option<Uuid>,
        /// Why the last refresh failed, while `value` is from before it.
        error: Option<String>,
    },
}

impl<T> Default for Loaded<T> {
    fn default() -> Self {
        Self::Loading(Uuid::nil())
    }
}

impl<T> Loaded<T> {
    pub(super) fn begin(&mut self) -> Uuid {
        let attempt = Uuid::new_v4();
        match self {
            Self::Loading(current) => *current = attempt,
            Self::Failed { retry, .. } => *retry = Some(attempt),
            Self::Ready { refresh, .. } => *refresh = Some(attempt),
        }
        attempt
    }

    /// Outdates every fetch in flight.
    pub(super) fn supersede(&mut self) {
        match self {
            Self::Loading(current) => *current = Uuid::nil(),
            Self::Failed { retry, .. } => *retry = None,
            Self::Ready { refresh, .. } => *refresh = None,
        }
    }

    pub(super) fn awaits(&self, attempt: Uuid) -> bool {
        match self {
            Self::Loading(current) => *current == attempt && !attempt.is_nil(),
            Self::Failed { retry, .. } => *retry == Some(attempt),
            Self::Ready { refresh, .. } => *refresh == Some(attempt),
        }
    }

    /// Applies the reply to `attempt`; false when a later fetch or a local
    /// change outdated it.
    pub(super) fn settle(&mut self, attempt: Uuid, result: Result<T, String>) -> bool {
        if !self.awaits(attempt) {
            return false;
        }
        *self = match (std::mem::take(self), result) {
            (_, Ok(value)) => Self::Ready {
                value,
                refresh: None,
                error: None,
            },
            (Self::Ready { value, .. }, Err(error)) => Self::Ready {
                value,
                refresh: None,
                error: Some(error),
            },
            (_, Err(error)) => Self::Failed { error, retry: None },
        };
        true
    }

    pub(super) fn value(&self) -> Option<&T> {
        match self {
            Self::Ready { value, .. } => Some(value),
            _ => None,
        }
    }

    pub(super) fn value_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Ready { value, .. } => Some(value),
            _ => None,
        }
    }

    /// Why the last fetch failed, with or without a value from before it.
    pub(super) fn error(&self) -> Option<&String> {
        match self {
            Self::Failed { error, .. }
            | Self::Ready {
                error: Some(error), ..
            } => Some(error),
            _ => None,
        }
    }

    /// Whether nothing has loaded and no failure is settled: a first fetch,
    /// or a retry after a failure.
    pub(super) fn pending(&self) -> bool {
        matches!(self, Self::Loading(_) | Self::Failed { retry: Some(_), .. })
    }
}

#[cfg(test)]
mod tests {
    use super::Loaded;

    #[test]
    fn a_refresh_keeps_the_value_and_a_failed_one_reports_beside_it() {
        let mut state = Loaded::default();
        let first = state.begin();
        assert!(state.settle(first, Ok(1)));
        let refresh = state.begin();
        assert_eq!(state.value(), Some(&1));
        assert!(state.settle(refresh, Err("offline".into())));
        assert_eq!(state.value(), Some(&1));
        assert_eq!(state.error().map(String::as_str), Some("offline"));
        let next = state.begin();
        assert!(state.settle(next, Ok(2)));
        assert_eq!(state.value(), Some(&2));
        assert_eq!(state.error(), None);
    }

    #[test]
    fn a_replaced_attempt_is_ignored_and_a_first_failure_has_no_value() {
        let mut state = Loaded::default();
        let stale = state.begin();
        let current = state.begin();
        assert!(!state.settle(stale, Ok(1)), "a later fetch replaced it");
        assert!(state.settle(current, Err("offline".into())));
        assert_eq!(state.value(), None);
        assert_eq!(state.error().map(String::as_str), Some("offline"));
        assert!(!state.pending());
        assert!(!state.settle(current, Ok(1)), "already settled");
    }

    #[test]
    fn a_retry_keeps_the_failure_known_until_it_lands() {
        let mut state = Loaded::<u8>::default();
        let first = state.begin();
        assert!(state.settle(first, Err("offline".into())));
        let retry = state.begin();
        assert!(state.pending());
        assert_eq!(state.error().map(String::as_str), Some("offline"));
        assert!(state.settle(retry, Err("still offline".into())));
        assert_eq!(state.error().map(String::as_str), Some("still offline"));
    }

    #[test]
    fn a_local_change_outdates_every_fetch_in_flight() {
        let mut state = Loaded::default();
        let first = state.begin();
        state.supersede();
        assert!(!state.settle(first, Ok(1)), "started before the change");
        let second = state.begin();
        assert!(state.settle(second, Ok(2)));
        let refresh = state.begin();
        state.supersede();
        assert!(!state.settle(refresh, Ok(3)), "started before the change");
        assert_eq!(state.value(), Some(&2));
        let failed = state.begin();
        assert!(state.settle(failed, Err("offline".into())));
        assert_eq!(state.value(), Some(&2), "a refresh failure keeps the value");
    }
}
