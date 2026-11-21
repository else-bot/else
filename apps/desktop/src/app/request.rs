//! The one request a card or dialog sends, and its outcome.

use crate::api::ClientError;
use uuid::Uuid;

/// A card's or dialog's request. `Op` is what was sent, kept while the
/// outcome matters: to show it optimistically, or to resend it unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(in crate::app) enum Request<Op> {
    #[default]
    Idle,
    Sending {
        attempt: Uuid,
        op: Op,
    },
    /// Sending `op` again after its outcome was unknown.
    Resending {
        attempt: Uuid,
        op: Op,
    },
    /// The server accepted `op`; the stream settles the card next.
    Accepted(Op),
    Failed(String),
    /// The request may or may not have applied, so resending must repeat
    /// `op` unchanged.
    Unconfirmed {
        op: Op,
        error: String,
    },
}

/// What settling an attempt did, and so whether its caller goes on to
/// handle the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(in crate::app) enum Settle {
    /// The attempt is not the request in flight; nothing changed.
    Stale,
    /// The owner token was rejected. The request is idle again, or still
    /// unconfirmed after a resend the server never handled, and the window
    /// handles the rejection.
    Rejected,
    Settled,
}

impl Settle {
    /// Whether the caller goes on to handle the result.
    pub(in crate::app) fn settled(self) -> bool {
        self == Self::Settled
    }
}

impl<Op> Request<Op> {
    /// Whether the controls stay locked: sent and not yet failed.
    pub(in crate::app) fn busy(&self) -> bool {
        matches!(
            self,
            Self::Sending { .. } | Self::Resending { .. } | Self::Accepted(_)
        )
    }

    /// The op sent and not failed, for showing its effect before the
    /// stream confirms it.
    pub(in crate::app) fn pending(&self) -> Option<&Op> {
        match self {
            Self::Sending { op, .. } | Self::Resending { op, .. } | Self::Accepted(op) => Some(op),
            _ => None,
        }
    }

    pub(in crate::app) fn error(&self) -> Option<&str> {
        match self {
            Self::Failed(error) | Self::Unconfirmed { error, .. } => Some(error),
            _ => None,
        }
    }

    pub(in crate::app) fn unconfirmed(&self) -> bool {
        matches!(self, Self::Unconfirmed { .. })
    }

    /// The op whose outcome is unknown, which a resend must repeat.
    pub(in crate::app) fn unconfirmed_op(&self) -> Option<&Op> {
        match self {
            Self::Unconfirmed { op, .. } => Some(op),
            _ => None,
        }
    }

    /// Starts sending `op`, unless a request is already busy. Returns the
    /// attempt that its result must settle.
    pub(in crate::app) fn begin(&mut self, op: Op) -> Option<Uuid> {
        if self.busy() {
            return None;
        }
        let attempt = Uuid::new_v4();
        *self = Self::Sending { attempt, op };
        Some(attempt)
    }

    /// Resends the op whose outcome is unknown. Returns the attempt and the
    /// op to send.
    pub(in crate::app) fn retry(&mut self) -> Option<(Uuid, Op)>
    where
        Op: Clone,
    {
        let Self::Unconfirmed { op, .. } = self else {
            return None;
        };
        let op = op.clone();
        let attempt = Uuid::new_v4();
        *self = Self::Resending {
            attempt,
            op: op.clone(),
        };
        Some((attempt, op))
    }

    /// Records a rejection found before anything was sent, such as an
    /// empty field. Ignored while busy.
    pub(in crate::app) fn reject(&mut self, message: impl Into<String>) {
        if !self.busy() {
            *self = Self::Failed(message.into());
        }
    }

    /// Settles `attempt` with `result`, changing nothing when `attempt` is
    /// not the request in flight.
    pub(in crate::app) fn settle<T>(
        &mut self,
        attempt: Uuid,
        result: &Result<T, ClientError>,
    ) -> Settle {
        self.settle_as(attempt, result, ToString::to_string)
    }

    /// Like [`Self::settle`], with `message` describing a failure. A
    /// resend the server never handled says nothing about the earlier
    /// attempt, so it stays unconfirmed.
    pub(in crate::app) fn settle_as<T>(
        &mut self,
        attempt: Uuid,
        result: &Result<T, ClientError>,
        message: impl FnOnce(&ClientError) -> String,
    ) -> Settle {
        let (op, resent) = match std::mem::take(self) {
            Self::Sending {
                attempt: current,
                op,
            } if current == attempt => (op, false),
            Self::Resending {
                attempt: current,
                op,
            } if current == attempt => (op, true),
            other => {
                *self = other;
                return Settle::Stale;
            }
        };
        *self = match result {
            Ok(_) => Self::Accepted(op),
            Err(err)
                if err.outcome_unknown()
                    || err.is_request_conflict()
                    || (resent && err.never_handled()) =>
            {
                Self::Unconfirmed {
                    op,
                    error: message(err),
                }
            }
            Err(ClientError::Unauthorized) => Self::Idle,
            Err(err) => Self::Failed(message(err)),
        };
        if matches!(result, Err(ClientError::Unauthorized)) {
            Settle::Rejected
        } else {
            Settle::Settled
        }
    }

    /// Fails `attempt` with `message`, for results that are not a
    /// [`ClientError`]. Returns false when `attempt` is stale.
    pub(in crate::app) fn fail(&mut self, attempt: Uuid, message: String) -> bool {
        match self {
            Self::Sending {
                attempt: current, ..
            }
            | Self::Resending {
                attempt: current, ..
            } if *current == attempt => {
                *self = Self::Failed(message);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Request, Settle};
    use crate::api::ClientError;
    use uuid::Uuid;

    fn api(status: u16, code: &str) -> ClientError {
        ClientError::Api {
            status,
            code: code.into(),
            message: "Nope.".into(),
            retryable: false,
        }
    }

    fn unreachable() -> ClientError {
        ClientError::Unreachable {
            origin: "http://127.0.0.1:1".into(),
            cause: "connection refused".into(),
        }
    }

    #[test]
    fn a_sent_request_locks_until_it_fails() {
        let mut request = Request::Idle;
        let attempt = request.begin("approve").unwrap();
        assert!(request.busy());
        assert_eq!(request.pending(), Some(&"approve"));
        assert_eq!(
            request.settle(attempt, &Err::<(), _>(api(422, "invalid"))),
            Settle::Settled
        );
        assert_eq!(request, Request::Failed("Nope.".into()));
        assert!(!request.busy());
        assert_eq!(request.error(), Some("Nope."));
    }

    #[test]
    fn an_accepted_request_stays_locked_and_keeps_its_op() {
        let mut request = Request::Idle;
        let attempt = request.begin(true).unwrap();
        assert_eq!(
            request.settle(attempt, &Ok::<_, ClientError>(())),
            Settle::Settled
        );
        assert_eq!(request, Request::Accepted(true));
        assert!(request.busy());
        assert_eq!(request.pending(), Some(&true));
        assert_eq!(request.begin(false), None);
    }

    #[test]
    fn a_second_begin_while_sending_is_refused() {
        let mut request = Request::Idle;
        let attempt = request.begin(1).unwrap();
        assert_eq!(request.begin(2), None);
        assert_eq!(request, Request::Sending { attempt, op: 1 });
    }

    #[test]
    fn a_stale_attempt_changes_nothing() {
        let mut request = Request::Idle;
        let attempt = request.begin(1).unwrap();
        let stale = Uuid::new_v4();
        assert_eq!(
            request.settle(stale, &Err::<(), _>(api(422, "invalid"))),
            Settle::Stale
        );
        assert!(!request.fail(stale, "late".into()));
        assert_eq!(request, Request::Sending { attempt, op: 1 });

        let mut failed = Request::<i32>::Failed("first".into());
        assert_eq!(
            failed.settle(attempt, &Ok::<_, ClientError>(())),
            Settle::Stale
        );
        assert_eq!(failed, Request::Failed("first".into()));
    }

    #[test]
    fn an_unknown_outcome_is_unconfirmed_and_retries_the_same_op() {
        for error in [
            unreachable(),
            api(503, "unavailable"),
            api(409, "request_conflict"),
        ] {
            let mut request = Request::Idle;
            let attempt = request.begin("discard").unwrap();
            assert_eq!(
                request.settle(attempt, &Err::<(), _>(error)),
                Settle::Settled
            );
            assert!(request.unconfirmed());
            assert!(!request.busy());
            let (retry, op) = request.retry().unwrap();
            assert_ne!(retry, attempt);
            assert_eq!(op, "discard");
            assert_eq!(
                request,
                Request::Resending {
                    attempt: retry,
                    op: "discard"
                }
            );
        }
    }

    #[test]
    fn a_rejected_token_unlocks_without_an_error() {
        let mut request = Request::Idle;
        let attempt = request.begin(()).unwrap();
        assert_eq!(
            request.settle(attempt, &Err::<(), _>(ClientError::Unauthorized)),
            Settle::Rejected
        );
        assert_eq!(request, Request::Idle);
        assert_eq!(request.error(), None);
        assert!(
            request.begin(()).is_some(),
            "a rejected token leaves the card free to send again"
        );
    }

    #[test]
    fn a_resend_the_server_never_handled_keeps_the_op_unconfirmed() {
        for error in [
            ClientError::Unauthorized,
            ClientError::Transport("Not on your Tailscale network.".into()),
        ] {
            let mut request = Request::Idle;
            let first = request.begin("create").unwrap();
            assert_eq!(
                request.settle(first, &Err::<(), _>(unreachable())),
                Settle::Settled
            );
            let (resend, _) = request.retry().unwrap();
            let rejected = matches!(error, ClientError::Unauthorized);
            let settled = request.settle(resend, &Err::<(), _>(error));
            assert_eq!(settled == Settle::Rejected, rejected);
            assert!(request.unconfirmed());
            assert_eq!(request.retry().unwrap().1, "create");
        }
    }

    #[test]
    fn a_resend_the_server_rejects_fails() {
        let mut request = Request::Idle;
        let first = request.begin("create").unwrap();
        assert_eq!(
            request.settle(first, &Err::<(), _>(unreachable())),
            Settle::Settled
        );
        let (resend, _) = request.retry().unwrap();
        assert_eq!(
            request.settle(resend, &Err::<(), _>(api(422, "invalid_bot"))),
            Settle::Settled
        );
        assert_eq!(request, Request::Failed("Nope.".into()));
    }

    #[test]
    fn only_an_unconfirmed_request_retries() {
        let mut idle = Request::<i32>::Idle;
        assert_eq!(idle.retry(), None);
        let mut failed = Request::<i32>::Failed("Nope.".into());
        assert_eq!(failed.retry(), None);
        assert_eq!(failed, Request::Failed("Nope.".into()));
    }

    #[test]
    fn settle_as_describes_the_failure() {
        let mut request = Request::Idle;
        let attempt = request.begin(()).unwrap();
        let _ = request.settle_as(attempt, &Err::<(), _>(api(422, "invalid")), |err| {
            format!("Couldn't save. {err}")
        });
        assert_eq!(request.error(), Some("Couldn't save. Nope."));
    }

    #[test]
    fn a_rejection_before_sending_waits_for_a_busy_request() {
        let mut request = Request::Idle;
        request.reject("Give your assistant a name.");
        assert_eq!(
            request,
            Request::Failed("Give your assistant a name.".into())
        );
        let attempt = request.begin(()).unwrap();
        request.reject("ignored");
        assert_eq!(request, Request::Sending { attempt, op: () });
    }

    #[test]
    fn fail_records_a_message_for_the_current_attempt() {
        let mut request = Request::Idle;
        let attempt = request.begin(()).unwrap();
        assert!(request.fail(attempt, "A bot could not be added.".into()));
        assert_eq!(request, Request::Failed("A bot could not be added.".into()));
    }
}
