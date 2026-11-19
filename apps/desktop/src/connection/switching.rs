//! A server switch shows its target at once and checks the connection
//! behind it. The latest switch alone can land or fail.

use uuid::Uuid;

/// Where the latest server switch stands. `T` is the check running in the
/// background; dropping it discards the check's outcome.
pub(super) enum Switch<T> {
    Settled,
    /// The target shows while its connection is checked.
    Connecting {
        server: Uuid,
        from: Option<Uuid>,
        attempt: Uuid,
        _check: T,
    },
    /// The check failed; the target stays shown with what it last knew.
    Failed {
        server: Uuid,
        from: Option<Uuid>,
        error: String,
        /// The session was connected when the check failed, so its
        /// connection coming back proves nothing new.
        live: bool,
    },
}

/// A failed switch, as its banner shows it.
pub(super) struct Failure<'a> {
    pub server: Uuid,
    pub from: Option<Uuid>,
    pub error: &'a str,
}

impl<T> Switch<T> {
    /// Starts checking `server`, superseding any earlier switch.
    pub(super) fn begin(
        &mut self,
        server: Uuid,
        from: Option<Uuid>,
        check: impl FnOnce(Uuid) -> T,
    ) {
        let attempt = Uuid::new_v4();
        *self = Self::Connecting {
            server,
            from,
            attempt,
            _check: check(attempt),
        };
    }

    /// Records a check's outcome; an outdated attempt changes nothing.
    /// `live` says whether the target's session was connected then.
    pub(super) fn finish(
        &mut self,
        attempt: Uuid,
        outcome: Result<(), String>,
        live: bool,
    ) -> bool {
        let Self::Connecting {
            server,
            from,
            attempt: current,
            ..
        } = self
        else {
            return false;
        };
        if *current != attempt {
            return false;
        }
        *self = match outcome {
            Ok(()) => Self::Settled,
            Err(error) => Self::Failed {
                server: *server,
                from: *from,
                error,
                live,
            },
        };
        true
    }

    pub(super) fn settle(&mut self) {
        *self = Self::Settled;
    }

    /// Forgets a removed server as the place to switch back to.
    pub(super) fn forget(&mut self, removed: Uuid) {
        if let Self::Connecting { from, .. } | Self::Failed { from, .. } = self
            && *from == Some(removed)
        {
            *from = None;
        }
    }

    /// Whether a check that failed while `id` was offline should run again
    /// now that its session connected.
    pub(super) fn recheck(&self, id: Uuid) -> bool {
        matches!(self, Self::Failed { server, live: false, .. } if *server == id)
    }

    pub(super) fn connecting(&self, id: Uuid) -> bool {
        matches!(self, Self::Connecting { server, .. } if *server == id)
    }

    pub(super) fn failure(&self) -> Option<Failure<'_>> {
        match self {
            Self::Failed {
                server,
                from,
                error,
                ..
            } => Some(Failure {
                server: *server,
                from: *from,
                error,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> [Uuid; 3] {
        [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()]
    }

    fn begin(switch: &mut Switch<()>, server: Uuid, from: Option<Uuid>) -> Uuid {
        let mut started = None;
        switch.begin(server, from, |attempt| started = Some(attempt));
        started.unwrap()
    }

    #[test]
    fn a_landed_check_settles_the_switch() {
        let [a, b, _] = ids();
        let mut switch = Switch::Settled;
        let attempt = begin(&mut switch, b, Some(a));
        assert!(switch.connecting(b));
        assert!(switch.finish(attempt, Ok(()), false));
        assert!(matches!(switch, Switch::Settled));
        assert!(!switch.connecting(b));
    }

    #[test]
    fn a_failed_check_keeps_the_target_and_offers_the_way_back() {
        let [a, b, _] = ids();
        let mut switch = Switch::Settled;
        let attempt = begin(&mut switch, b, Some(a));
        assert!(switch.finish(attempt, Err("Connection refused.".into()), false));
        let failure = switch.failure().unwrap();
        assert_eq!(failure.server, b);
        assert_eq!(failure.from, Some(a));
        assert_eq!(failure.error, "Connection refused.");
    }

    #[test]
    fn rapid_switches_end_on_the_last_target() {
        let [a, b, c] = ids();
        let mut switch = Switch::Settled;
        let to_b = begin(&mut switch, b, Some(a));
        let to_c = begin(&mut switch, c, Some(b));
        assert!(!switch.finish(to_b, Err("Timed out.".into()), false));
        assert!(switch.connecting(c));
        assert!(!switch.connecting(b));
        assert!(switch.finish(to_c, Err("Timed out.".into()), false));
        let failure = switch.failure().unwrap();
        assert_eq!(failure.server, c);
        assert_eq!(failure.from, Some(b));
    }

    #[test]
    fn a_settled_switch_ignores_late_checks() {
        let [a, b, _] = ids();
        let mut switch = Switch::Settled;
        let attempt = begin(&mut switch, b, Some(a));
        switch.settle();
        assert!(!switch.finish(attempt, Err("Timed out.".into()), false));
        assert!(switch.failure().is_none());
    }

    #[test]
    fn only_a_failure_while_offline_is_checked_again_on_connect() {
        let [a, b, _] = ids();
        let mut switch = Switch::Settled;
        let attempt = begin(&mut switch, b, Some(a));
        switch.finish(attempt, Err("Connection refused.".into()), false);
        assert!(switch.recheck(b));
        assert!(!switch.recheck(a));
        let attempt = begin(&mut switch, b, Some(a));
        assert!(!switch.recheck(b), "a running check is not repeated");
        switch.finish(attempt, Err("Unsupported API version.".into()), true);
        assert!(!switch.recheck(b));
    }

    #[test]
    fn removing_the_origin_drops_switch_back() {
        let [a, b, _] = ids();
        let mut switch = Switch::Settled;
        let attempt = begin(&mut switch, b, Some(a));
        switch.finish(attempt, Err("Timed out.".into()), false);
        switch.forget(a);
        assert_eq!(switch.failure().unwrap().from, None);
    }
}
