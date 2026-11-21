//! What the settings sheet shows: its section, the open pane, the two
//! panes kept warm, and the Servers page.

use super::super::pane::{Mounted, Pane};
use super::super::{ConnectForm, backups};
use super::SettingsSection;
use gpui_kit::*;
use uuid::Uuid;

/// The Servers section's page.
pub(in crate::connection) enum ServerPage {
    List,
    Add(ConnectForm),
    Edit {
        id: Uuid,
        /// The name the editor opened with; saving it unchanged keeps the
        /// name the server reports.
        opened: String,
        form: ConnectForm,
    },
    /// A saved server's update card.
    Update(Uuid),
}

impl ServerPage {
    pub(in crate::connection) fn form(&self) -> Option<&ConnectForm> {
        match self {
            Self::Add(form) | Self::Edit { form, .. } => Some(form),
            Self::List | Self::Update(_) => None,
        }
    }

    pub(in crate::connection) fn form_mut(&mut self) -> Option<&mut ConnectForm> {
        match self {
            Self::Add(form) | Self::Edit { form, .. } => Some(form),
            Self::List | Self::Update(_) => None,
        }
    }

    /// The saved server the editor edits.
    pub(in crate::connection) fn editing(&self) -> Option<Uuid> {
        match self {
            Self::Edit { id, .. } => Some(*id),
            _ => None,
        }
    }
}

pub(in crate::connection) struct Settings {
    pub section: SettingsSection,
    /// Private, so a pane can only leave through [`Settings::show`].
    pane: Pane,
    pub backups: Option<Mounted<backups::View>>,
    pub email: Option<(Entity<crate::email::EmailSettings>, Subscription)>,
    pub servers: ServerPage,
    pub focus: FocusHandle,
    pub navigation_focus: [FocusHandle; 10],
}

impl Settings {
    pub(in crate::connection) fn new(cx: &mut App) -> Self {
        Self {
            section: SettingsSection::Server,
            pane: Pane::None,
            backups: None,
            email: None,
            servers: ServerPage::List,
            focus: cx.focus_handle(),
            navigation_focus: std::array::from_fn(|_| cx.focus_handle()),
        }
    }

    pub(in crate::connection) fn pane(&self) -> &Pane {
        &self.pane
    }

    /// Replaces the open pane. Every pane leaves through here, so its
    /// secrets are wiped before it drops.
    pub(in crate::connection) fn show(&mut self, pane: Pane, window: &mut Window, cx: &mut App) {
        std::mem::replace(&mut self.pane, pane).close(window, cx);
    }

    /// Drops every pane, the warm ones too.
    pub(in crate::connection) fn clear(&mut self, window: &mut Window, cx: &mut App) {
        self.show(Pane::None, window, cx);
        self.backups = None;
        self.email = None;
    }

    /// Whether leaving would discard edits: an open server editor, or a
    /// pane with unsaved work.
    pub(in crate::connection) fn has_unsaved_work(&self, cx: &App) -> bool {
        self.servers.form().is_some()
            || self.pane.has_unsaved_work(cx)
            || self
                .backups
                .as_ref()
                .is_some_and(|pane| pane.view.read(cx).has_unsaved_work(cx))
            || self
                .email
                .as_ref()
                .is_some_and(|(view, _)| view.read(cx).has_unsaved_work(cx))
    }
}
