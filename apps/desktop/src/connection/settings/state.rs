//! What the settings sheet shows: its section, the open pane, the two
//! panes kept warm, and the Servers page. Each leaves through a setter
//! that wipes its typed secrets before it drops.

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

    /// Wipes the editor's typed owner token before the page drops.
    fn close(self, window: &mut Window, cx: &mut App) {
        if let Self::Add(form) | Self::Edit { form, .. } = self {
            form.close(window, cx);
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

type EmailPane = (Entity<crate::email::EmailSettings>, Subscription);

pub(in crate::connection) struct Settings {
    pub section: SettingsSection,
    /// Private, so a pane can only leave through [`Settings::show`].
    pane: Pane,
    /// Private, so the warm panes can only leave through
    /// [`Settings::set_backups`] and [`Settings::set_email`].
    backups: Option<Mounted<backups::View>>,
    email: Option<EmailPane>,
    /// Private, so an editor can only leave through [`Settings::set_servers`].
    servers: ServerPage,
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

    pub(in crate::connection) fn servers(&self) -> &ServerPage {
        &self.servers
    }

    pub(in crate::connection) fn form_mut(&mut self) -> Option<&mut ConnectForm> {
        self.servers.form_mut()
    }

    /// Replaces the Servers page, wiping a dropped editor's owner token.
    pub(in crate::connection) fn set_servers(
        &mut self,
        page: ServerPage,
        window: &mut Window,
        cx: &mut App,
    ) {
        std::mem::replace(&mut self.servers, page).close(window, cx);
    }

    /// Drops every pane, the warm ones too, and the server editor.
    pub(in crate::connection) fn clear(&mut self, window: &mut Window, cx: &mut App) {
        self.show(Pane::None, window, cx);
        self.set_servers(ServerPage::List, window, cx);
        self.set_backups(None, window, cx);
        self.set_email(None, window, cx);
    }

    pub(in crate::connection) fn backups(&self) -> Option<&Mounted<backups::View>> {
        self.backups.as_ref()
    }

    /// Replaces the Backups pane, wiping a dropped editor's secret key.
    pub(in crate::connection) fn set_backups(
        &mut self,
        pane: Option<Mounted<backups::View>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(old) = std::mem::replace(&mut self.backups, pane) {
            old.view.update(cx, |view, cx| view.close(window, cx));
        }
    }

    pub(in crate::connection) fn email(&self) -> Option<&EmailPane> {
        self.email.as_ref()
    }

    /// Replaces the Email pane, wiping a dropped form's app password.
    pub(in crate::connection) fn set_email(
        &mut self,
        pane: Option<EmailPane>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some((old, _)) = std::mem::replace(&mut self.email, pane) {
            old.update(cx, |view, cx| view.close(window, cx));
        }
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
