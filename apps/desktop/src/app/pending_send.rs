use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::reply::Quoted;
use crate::api::SendRequest;
use crate::model::outgoing::DraftAttachment;
use else_wire::items::MessagePart;
use else_wire::items::Reaction;

/// The API's code for a message refused because its bot's provider cannot
/// run now, such as a CLI signed out on the server. The server checks the
/// CLI again when the same message is sent again.
pub(super) const PROVIDER_UNAVAILABLE: &str = "provider_unavailable";

/// An owner message the server has not yet confirmed, frozen under one request ID.
#[derive(Debug, Clone)]
pub(super) struct PendingSend {
    pub(super) request_id: Uuid,
    pub(super) text: String,
    pub(super) parts: Vec<MessagePart>,
    pub(super) sent_at: DateTime<Utc>,
    pub(super) reaction: Option<Reaction>,
    pub(super) open_draft: Option<DraftAttachment>,
    pub(super) quoted: Option<Quoted>,
    delivery: Delivery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Delivery {
    Sending,
    /// The request may have reached the server; only a lookup or retry under the same ID settles it.
    Unknown {
        reason: String,
    },
    Resolving {
        reason: String,
    },
    Rejected {
        reason: String,
        code: Option<String>,
    },
}

pub(super) enum Failure {
    Unknown(String),
    Rejected {
        reason: String,
        code: Option<String>,
    },
}

pub(super) enum Lookup {
    Missing,
    Failed,
}

impl PendingSend {
    pub(super) fn new(
        text: String,
        parts: Vec<MessagePart>,
        reaction: Option<Reaction>,
        open_draft: Option<DraftAttachment>,
        quoted: Option<Quoted>,
    ) -> Self {
        Self {
            request_id: Uuid::new_v4(),
            text,
            parts,
            sent_at: Utc::now(),
            reaction,
            open_draft,
            quoted,
            delivery: Delivery::Sending,
        }
    }

    pub(super) fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    pub(super) fn is_rejected(&self) -> bool {
        matches!(self.delivery, Delivery::Rejected { .. })
    }

    /// Only a send in flight cannot be dismissed. Dismissing an unconfirmed send drops the local
    /// copy; if the server did store it, the conversation history shows it.
    pub(super) fn is_dismissible(&self) -> bool {
        matches!(
            self.delivery,
            Delivery::Unknown { .. } | Delivery::Rejected { .. }
        )
    }

    pub(super) fn request(&self) -> SendRequest {
        SendRequest {
            request_id: self.request_id,
            text: self.text.clone(),
            provider_id: None,
            reaction: self.reaction,
            parts: self.parts.clone(),
            open_draft: self.open_draft.clone(),
            answers_notice: self.quoted.as_ref().and_then(Quoted::answers),
        }
    }

    pub(super) fn on_send_failure(&mut self, failure: Failure) {
        if self.delivery != Delivery::Sending {
            return;
        }
        self.delivery = match failure {
            Failure::Unknown(reason) => Delivery::Unknown { reason },
            Failure::Rejected { reason, code } => Delivery::Rejected { reason, code },
        };
    }

    pub(super) fn begin_resolution(&mut self) -> bool {
        let Delivery::Unknown { reason } = &mut self.delivery else {
            return false;
        };
        let reason = std::mem::take(reason);
        self.delivery = Delivery::Resolving { reason };
        true
    }

    pub(super) fn on_lookup(&mut self, lookup: Lookup) -> Option<SendRequest> {
        let Delivery::Resolving { reason } = &mut self.delivery else {
            return None;
        };
        match lookup {
            Lookup::Missing => {
                self.delivery = Delivery::Sending;
                Some(self.request())
            }
            Lookup::Failed => {
                let reason = std::mem::take(reason);
                self.delivery = Delivery::Unknown { reason };
                None
            }
        }
    }

    /// Whether sending again under the same request ID can settle it: its
    /// delivery is unknown, or its provider could not run and may now.
    pub(super) fn is_retryable(&self) -> bool {
        match &self.delivery {
            Delivery::Unknown { .. } => true,
            Delivery::Rejected { code, .. } => code.as_deref() == Some(PROVIDER_UNAVAILABLE),
            Delivery::Sending | Delivery::Resolving { .. } => false,
        }
    }

    pub(super) fn retry(&mut self) -> Option<SendRequest> {
        if !self.is_retryable() {
            return None;
        }
        self.delivery = Delivery::Sending;
        Some(self.request())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uncertain() -> PendingSend {
        let mut pending = PendingSend::new("hello".into(), Vec::new(), None, None, None);
        pending.on_send_failure(Failure::Unknown("connection reset".into()));
        pending
    }

    #[test]
    fn a_repeated_retry_submits_the_uncertain_message_once() {
        let mut pending = uncertain();
        let first = pending.retry().map(|request| request.request_id);
        assert_eq!(first, Some(pending.request_id));
        assert!(pending.retry().is_none());
        assert_eq!(pending.delivery(), &Delivery::Sending);
    }

    #[test]
    fn overlapping_reconnects_look_up_the_uncertain_message_once() {
        let mut pending = uncertain();
        assert!(pending.begin_resolution());
        assert!(!pending.begin_resolution());
        assert!(pending.retry().is_none());
        let resent = pending.on_lookup(Lookup::Missing);
        assert_eq!(resent.map(|r| r.request_id), Some(pending.request_id));
        assert!(pending.on_lookup(Lookup::Missing).is_none());
    }

    #[test]
    fn a_failed_lookup_leaves_the_message_retryable_with_its_reason() {
        let mut pending = uncertain();
        assert!(pending.begin_resolution());
        assert!(pending.on_lookup(Lookup::Failed).is_none());
        assert_eq!(
            pending.delivery(),
            &Delivery::Unknown {
                reason: "connection reset".into()
            }
        );
        assert!(pending.retry().is_some());
    }

    #[test]
    fn only_a_settled_failure_is_dismissible() {
        let mut pending = PendingSend::new("hello".into(), Vec::new(), None, None, None);
        assert!(!pending.is_dismissible());
        pending.on_send_failure(Failure::Unknown("paused".into()));
        assert!(pending.is_dismissible());
        assert!(pending.begin_resolution());
        assert!(!pending.is_dismissible());
        pending.on_lookup(Lookup::Failed);
        assert!(pending.is_dismissible());
        pending.retry();
        assert!(!pending.is_dismissible());
    }

    #[test]
    fn a_rejected_message_is_never_resent() {
        let mut pending = PendingSend::new("hello".into(), Vec::new(), None, None, None);
        pending.on_send_failure(Failure::Rejected {
            reason: "too long".into(),
            code: None,
        });
        assert!(!pending.begin_resolution());
        assert!(pending.retry().is_none());
        pending.on_send_failure(Failure::Unknown("late".into()));
        assert!(pending.is_rejected());
    }

    #[test]
    fn a_message_refused_for_its_provider_retries_once_under_its_request_id() {
        let mut pending = PendingSend::new("hello".into(), Vec::new(), None, None, None);
        pending.on_send_failure(Failure::Rejected {
            reason: "Claude Code is not logged in on the host.".into(),
            code: Some("provider_unavailable".into()),
        });
        let resent = pending.retry().map(|request| request.request_id);
        assert_eq!(resent, Some(pending.request_id));
        assert!(pending.retry().is_none(), "a second click while sending");
        pending.on_send_failure(Failure::Rejected {
            reason: "Claude Code is still not logged in.".into(),
            code: Some("provider_unavailable".into()),
        });
        assert_eq!(
            pending.delivery(),
            &Delivery::Rejected {
                reason: "Claude Code is still not logged in.".into(),
                code: Some("provider_unavailable".into()),
            }
        );
        assert!(pending.retry().is_some());
    }

    #[test]
    fn a_retry_answers_the_same_ask() {
        let notice = Uuid::new_v4();
        let quoted = Quoted {
            quotes: vec![super::super::reply::Quote::ask(
                notice,
                "Hotfix?".into(),
                true,
            )],
            prefix: 0,
        };
        let pending = PendingSend::new("hotfix it".into(), Vec::new(), None, None, Some(quoted));
        assert_eq!(pending.request().answers_notice, Some(notice));
        assert_eq!(pending.request().request_id, pending.request().request_id);
    }
}
