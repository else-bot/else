use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::api::SendRequest;
use crate::attachments::MessagePart;
use crate::model::{DraftAttachment, Reaction};

/// An owner message the server has not yet confirmed, frozen under one request ID.
#[derive(Debug, Clone)]
pub(super) struct PendingSend {
    pub(super) request_id: Uuid,
    pub(super) text: String,
    pub(super) parts: Vec<MessagePart>,
    pub(super) sent_at: DateTime<Utc>,
    pub(super) reaction: Option<Reaction>,
    pub(super) open_draft: Option<DraftAttachment>,
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
    },
}

pub(super) enum Failure {
    Unknown(String),
    Rejected(String),
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
    ) -> Self {
        Self {
            request_id: Uuid::new_v4(),
            text,
            parts,
            sent_at: Utc::now(),
            reaction,
            open_draft,
            delivery: Delivery::Sending,
        }
    }

    pub(super) fn delivery(&self) -> &Delivery {
        &self.delivery
    }

    pub(super) fn is_rejected(&self) -> bool {
        matches!(self.delivery, Delivery::Rejected { .. })
    }

    pub(super) fn request(&self) -> SendRequest {
        SendRequest {
            request_id: self.request_id,
            text: self.text.clone(),
            provider_id: None,
            reaction: self.reaction,
            parts: self.parts.clone(),
            open_draft: self.open_draft.clone(),
        }
    }

    pub(super) fn on_send_failure(&mut self, failure: Failure) {
        self.delivery = match failure {
            Failure::Unknown(reason) => Delivery::Unknown { reason },
            Failure::Rejected(reason) => Delivery::Rejected { reason },
        };
    }

    pub(super) fn begin_resolution(&mut self) -> bool {
        matches!(self.delivery, Delivery::Unknown { .. })
    }

    pub(super) fn on_lookup(&mut self, lookup: Lookup) -> Option<SendRequest> {
        match lookup {
            Lookup::Missing => self.retry(),
            Lookup::Failed => None,
        }
    }

    pub(super) fn retry(&mut self) -> Option<SendRequest> {
        self.delivery = Delivery::Sending;
        Some(self.request())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uncertain() -> PendingSend {
        let mut pending = PendingSend::new("hello".into(), Vec::new(), None, None);
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
    fn a_rejected_message_is_never_resent() {
        let mut pending = PendingSend::new("hello".into(), Vec::new(), None, None);
        pending.on_send_failure(Failure::Rejected("too long".into()));
        assert!(!pending.begin_resolution());
        assert!(pending.retry().is_none());
        pending.on_send_failure(Failure::Unknown("late".into()));
        assert!(pending.is_rejected());
    }
}
