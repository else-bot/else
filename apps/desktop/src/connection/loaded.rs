//! A server value a settings pane shows, and the rule for which reply may
//! land: each fetch takes an attempt from [`Loaded::begin`], and only the
//! reply to the latest attempt lands. A local change that may alter the value
//! holds a [`Guard`] from [`Loaded::operate`], so no reply from before it
//! lands and no fetch starts until it ends. A local change whose reply is the
//! whole new value shows it with [`Loaded::replace`]. [`Loaded::poll`] keeps
//! a pane fresh under that rule.

use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{Context, Task, Window};
use uuid::Uuid;

/// A fetched value that stays on screen while a refresh runs and after a
/// refresh fails.
pub(super) struct Loaded<T> {
    state: State<T>,
    /// Shared with every live [`Guard`]; more than one owner means a local
    /// change is running.
    operations: Rc<()>,
}

enum State<T> {
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

impl<T> Default for State<T> {
    fn default() -> Self {
        Self::Loading(Uuid::nil())
    }
}

impl<T> Default for Loaded<T> {
    fn default() -> Self {
        Self {
            state: State::default(),
            operations: Rc::new(()),
        }
    }
}

/// A running local change. While it lives, [`Loaded::begin`] starts no
/// fetch.
#[must_use = "the operation ends when the guard drops"]
pub(super) struct Guard {
    _operations: Rc<()>,
}

impl<T: 'static> Loaded<T> {
    /// Fetches the value every `interval` while the view lives. No fetch
    /// starts during an operation, and `apply` only sees the reply to the
    /// latest attempt; it settles that attempt itself.
    pub(super) fn poll<V, R, F>(
        window: &mut Window,
        cx: &mut Context<V>,
        loaded: fn(&mut V) -> &mut Self,
        interval: fn(&V) -> Duration,
        fetch: impl Fn() -> F + 'static,
        apply: fn(&mut V, Uuid, R, &mut Window, &mut Context<V>),
    ) -> Task<()>
    where
        V: 'static,
        R: 'static,
        F: Future<Output = R> + 'static,
    {
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(attempt) = this.update(cx, |view, _| loaded(view).begin()) {
                if let Some(attempt) = attempt {
                    let reply = fetch().await;
                    let open = this.update_in(cx, |view, window, cx| {
                        if loaded(view).awaits(attempt) {
                            apply(view, attempt, reply, window, cx);
                        }
                    });
                    if open.is_err() {
                        break;
                    }
                }
                let Ok(delay) = this.update(cx, |view, _| interval(view)) else {
                    break;
                };
                cx.background_executor().timer(delay).await;
            }
        })
    }
}

impl<T> Loaded<T> {
    /// Starts a fetch; `None` while an operation runs.
    pub(super) fn begin(&mut self) -> Option<Uuid> {
        if self.operating() {
            return None;
        }
        let attempt = Uuid::new_v4();
        match &mut self.state {
            State::Loading(current) => *current = attempt,
            State::Failed { retry, .. } => *retry = Some(attempt),
            State::Ready { refresh, .. } => *refresh = Some(attempt),
        }
        Some(attempt)
    }

    /// Starts a local change: outdates every fetch in flight and starts none
    /// until the guard drops.
    pub(super) fn operate(&mut self) -> Guard {
        match &mut self.state {
            State::Loading(current) => *current = Uuid::nil(),
            State::Failed { retry, .. } => *retry = None,
            State::Ready { refresh, .. } => *refresh = None,
        }
        Guard {
            _operations: self.operations.clone(),
        }
    }

    fn operating(&self) -> bool {
        Rc::strong_count(&self.operations) > 1
    }

    /// Shows the value a local change returned, outdating every fetch in
    /// flight.
    pub(super) fn replace(&mut self, value: T) {
        self.state = State::Ready {
            value,
            refresh: None,
            error: None,
        };
    }

    pub(super) fn awaits(&self, attempt: Uuid) -> bool {
        match &self.state {
            State::Loading(current) => *current == attempt && !attempt.is_nil(),
            State::Failed { retry, .. } => *retry == Some(attempt),
            State::Ready { refresh, .. } => *refresh == Some(attempt),
        }
    }

    /// Applies the reply to `attempt`; false when a later fetch or a local
    /// change outdated it.
    pub(super) fn settle(&mut self, attempt: Uuid, result: Result<T, String>) -> bool {
        if !self.awaits(attempt) {
            return false;
        }
        self.state = match (std::mem::take(&mut self.state), result) {
            (_, Ok(value)) => State::Ready {
                value,
                refresh: None,
                error: None,
            },
            (State::Ready { value, .. }, Err(error)) => State::Ready {
                value,
                refresh: None,
                error: Some(error),
            },
            (_, Err(error)) => State::Failed { error, retry: None },
        };
        true
    }

    pub(super) fn value(&self) -> Option<&T> {
        match &self.state {
            State::Ready { value, .. } => Some(value),
            _ => None,
        }
    }

    pub(super) fn value_mut(&mut self) -> Option<&mut T> {
        match &mut self.state {
            State::Ready { value, .. } => Some(value),
            _ => None,
        }
    }

    /// Why the last fetch failed, with or without a value from before it.
    pub(super) fn error(&self) -> Option<&String> {
        match &self.state {
            State::Failed { error, .. }
            | State::Ready {
                error: Some(error), ..
            } => Some(error),
            _ => None,
        }
    }

    /// Whether nothing has loaded and no failure is settled: a first fetch,
    /// or a retry after a failure.
    pub(super) fn pending(&self) -> bool {
        matches!(
            self.state,
            State::Loading(_) | State::Failed { retry: Some(_), .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::Loaded;
    use uuid::Uuid;

    fn begin<T>(state: &mut Loaded<T>) -> Uuid {
        state.begin().expect("no operation runs")
    }

    #[test]
    fn a_refresh_keeps_the_value_and_a_failed_one_reports_beside_it() {
        let mut state = Loaded::default();
        let first = begin(&mut state);
        assert!(state.settle(first, Ok(1)));
        let refresh = begin(&mut state);
        assert_eq!(state.value(), Some(&1));
        assert!(state.settle(refresh, Err("offline".into())));
        assert_eq!(state.value(), Some(&1));
        assert_eq!(state.error().map(String::as_str), Some("offline"));
        let next = begin(&mut state);
        assert!(state.settle(next, Ok(2)));
        assert_eq!(state.value(), Some(&2));
        assert_eq!(state.error(), None);
    }

    #[test]
    fn a_replaced_attempt_is_ignored_and_a_first_failure_has_no_value() {
        let mut state = Loaded::default();
        let stale = begin(&mut state);
        let current = begin(&mut state);
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
        let first = begin(&mut state);
        assert!(state.settle(first, Err("offline".into())));
        let retry = begin(&mut state);
        assert!(state.pending());
        assert_eq!(state.error().map(String::as_str), Some("offline"));
        assert!(state.settle(retry, Err("still offline".into())));
        assert_eq!(state.error().map(String::as_str), Some("still offline"));
    }

    #[test]
    fn an_operation_outdates_fetches_in_flight_and_holds_new_ones_until_it_ends() {
        let mut state = Loaded::default();
        let first = begin(&mut state);
        let operation = state.operate();
        assert!(!state.settle(first, Ok(1)), "started before the change");
        assert_eq!(state.begin(), None, "no fetch during the change");
        drop(operation);
        let second = begin(&mut state);
        assert!(state.settle(second, Ok(2)));
        let refresh = begin(&mut state);
        let operation = state.operate();
        assert!(!state.settle(refresh, Ok(3)), "started before the change");
        assert_eq!(state.value(), Some(&2));
        let overlapping = state.operate();
        drop(operation);
        assert_eq!(state.begin(), None, "another change still runs");
        drop(overlapping);
        let failed = begin(&mut state);
        assert!(state.settle(failed, Err("offline".into())));
        assert_eq!(state.value(), Some(&2), "a refresh failure keeps the value");
    }
}
