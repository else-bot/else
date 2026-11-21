use super::draft_card::{DraftForm, DraftImages};
use super::question_card::choice::WriteIn;
use super::question_card::connection::Form as ConnectionForm;
use super::question_card::{Extent, FormattedContext, QuestionForm};
use super::request::Request;
use super::secret_card::SecretForm;
use super::skill_card::SkillCard;
use super::social_card::SocialCard;
use crate::app::conversation::ConversationView;
use gpui_kit::*;
use std::collections::HashMap;
use uuid::Uuid;

/// What the desktop keeps for each card on screen, by card ID. Dropping a
/// card's entry drops all of its state.
#[derive(Default)]
pub(super) struct Cards {
    /// One entry per loaded question.
    pub(super) questions: HashMap<Uuid, QuestionCard>,
    /// One entry per pending credential request.
    pub(super) secrets: HashMap<Uuid, SecretCard>,
    /// One entry per loaded outgoing draft.
    pub(super) drafts: HashMap<Uuid, DraftCard>,
    /// One entry per loaded skill install, while connected.
    pub(super) skills: HashMap<Uuid, SkillCard>,
    /// One entry per pending or connected social setup, while connected and shown.
    pub(super) socials: HashMap<Uuid, SocialCard>,
}

#[derive(Default)]
pub(super) struct QuestionCard {
    /// The answer sent from this card, until the question settles.
    pub(super) submit: Option<QuestionForm>,
    /// The approval scope picked, until the question settles.
    pub(super) scope: Option<String>,
    /// The typed answer field of a pending single-choice question.
    pub(super) write_in: Option<WriteIn>,
    /// The parsed markdown of its context; it stays after the question settles.
    pub(super) contexts: HashMap<Extent, FormattedContext>,
    /// It holds typed credentials, so parking the view drops it.
    pub(super) connection: Option<ConnectionForm>,
    pub(super) expanded: bool,
}

#[derive(Default)]
pub(super) struct SecretCard {
    pub(super) form: Option<SecretForm>,
    /// `Accepted` once the server stored the answer; the request settles next.
    pub(super) request: Request<()>,
}

#[derive(Default)]
pub(super) struct DraftCard {
    /// The editable draft, while it is open.
    pub(super) form: Option<DraftForm>,
    pub(super) images: Option<DraftImages>,
    pub(super) expanded: bool,
}

impl Cards {
    pub(super) fn submit(&self, question: Uuid) -> Option<&QuestionForm> {
        self.questions.get(&question)?.submit.as_ref()
    }

    pub(super) fn question_mut(&mut self, id: Uuid) -> &mut QuestionCard {
        self.questions.entry(id).or_default()
    }

    pub(super) fn connection(&self, question: Uuid) -> Option<&ConnectionForm> {
        self.questions.get(&question)?.connection.as_ref()
    }

    pub(super) fn connection_mut(&mut self, question: Uuid) -> Option<&mut ConnectionForm> {
        self.questions.get_mut(&question)?.connection.as_mut()
    }

    pub(super) fn draft_form(&self, id: Uuid) -> Option<&DraftForm> {
        self.drafts.get(&id)?.form.as_ref()
    }

    pub(super) fn draft_form_mut(&mut self, id: Uuid) -> Option<&mut DraftForm> {
        self.drafts.get_mut(&id)?.form.as_mut()
    }

    /// Drops the state that holds typed credentials or in-flight checks
    /// when the view is parked; showing it again builds them anew.
    pub(super) fn park(&mut self) {
        for card in self.questions.values_mut() {
            card.connection = None;
        }
    }
}

impl ConversationView {
    /// Brings each card's state in line with the shown conversation.
    pub(super) fn sync_cards(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_skill_cards(window, cx);
        self.sync_secret_cards(window, cx);
        self.sync_question_cards(window, cx);
        self.sync_draft_cards(window, cx);
        self.sync_social_cards(window, cx);
    }
}
