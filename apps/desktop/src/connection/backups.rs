mod form;
mod local;
mod view;

use super::*;
use crate::api::BackupSettingsInput;
use crate::attachments::byte_size;
use else_wire::backups::BackupSnapshot;
use std::time::Duration;

use super::pane::PaneEvent;

use form::{DestinationForm, Editor, LocalForm, PolicyForm};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Idle,
    Testing,
    Saving,
    SavingLocal,
    Starting,
}

pub(super) struct View {
    client: Arc<Client>,
    snapshot: Option<BackupSnapshot>,
    editor: Option<Editor>,
    local_form: Option<LocalForm>,
    operation: Operation,
    error: Option<String>,
    poll_error: Option<String>,
    feedback: Option<String>,
    _poll: Task<()>,
    _request: Option<Task<()>>,
}

impl EventEmitter<PaneEvent> for View {}

impl View {
    pub(super) fn has_unsaved_work(&self, cx: &App) -> bool {
        self.editor.is_some() || self.busy() || self.local_dirty(cx)
    }
    /// Whether the owner has unsaved schedule edits, which hold every
    /// other backup action until they save or cancel.
    fn local_dirty(&self, cx: &App) -> bool {
        self.local_form.as_ref().is_some_and(|form| form.dirty(cx))
    }
    fn busy(&self) -> bool {
        self.operation != Operation::Idle
    }

    fn accept(&mut self, snapshot: BackupSnapshot, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .snapshot
            .as_ref()
            .is_none_or(|s| snapshot.revision >= s.revision)
        {
            let form = self
                .local_form
                .as_ref()
                .map(|form| (form.dirty(cx), form.base_revision));
            match snapshot.local.as_ref() {
                None => self.local_form = None,
                Some(local) if rebuild_local_form(form, snapshot.revision) => {
                    self.local_form = Some(LocalForm::new(snapshot.revision, local, window, cx));
                }
                Some(_) => {}
            }
            self.snapshot = Some(snapshot);
        }
    }
}

/// Rebuild the schedule form when there is none, or when a newer revision
/// arrived and the owner has no edits to keep. `form` is (dirty, revision).
fn rebuild_local_form(form: Option<(bool, i64)>, revision: i64) -> bool {
    form.is_none_or(|(dirty, base)| !dirty && base != revision)
}

impl Shell {
    pub(super) fn open_backups(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = &self.active else { return };
        let client = active.client.clone();
        let view = cx.new(|cx| View::new(client, window, cx));
        let pane = self.mount(view, super::settings::SettingsSection::Backups, window, cx);
        if let Some(settings) = self.front.settings_mut() {
            settings.backups = Some(pane);
        }
        cx.notify();
    }
}

impl View {
    fn new(client: Arc<Client>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let poll_client = client.clone();
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                let result = poll_client.clone().backups().await;
                let delay = this.update_in(cx, |view, window, cx| {
                    // An operation's own reply carries the newer snapshot.
                    if !view.busy() {
                        match result {
                            Ok(snapshot) => {
                                view.accept(snapshot, window, cx);
                                view.poll_error = None;
                            }
                            Err(err) => view.poll_error = Some(err.to_string()),
                        }
                        cx.notify();
                    }
                    if view
                        .snapshot
                        .as_ref()
                        .is_some_and(|s| s.running || s.pause.is_some())
                    {
                        Duration::from_secs(2)
                    } else {
                        Duration::from_secs(10)
                    }
                });
                let Ok(delay) = delay else { break };
                cx.background_executor().timer(delay).await;
            }
        });
        Self {
            client,
            snapshot: None,
            editor: None,
            local_form: None,
            operation: Operation::Idle,
            error: None,
            poll_error: None,
            feedback: None,
            _poll: poll,
            _request: None,
        }
    }

    /// Whether an edit may start: nothing running and no unsaved schedule.
    fn editable(&self, cx: &App) -> bool {
        !self.busy() && !self.local_dirty(cx)
    }

    fn edit_destination(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable(cx) {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let form = DestinationForm::new(snapshot, window, cx);
        self.editor = Some(Editor::Destination(form));
        self.error = None;
        self.feedback = None;
        cx.notify();
    }

    fn edit_policy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable(cx) {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let form = PolicyForm::new(snapshot, window, cx);
        self.editor = Some(Editor::Policy(form));
        self.error = None;
        self.feedback = None;
        cx.notify();
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        self.editor = None;
        self.error = None;
        self.feedback = None;
        cx.notify();
    }

    fn test_destination(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable(cx) {
            return;
        }
        let Some(Editor::Destination(form)) = &self.editor else {
            return;
        };
        let destination = match form.input(cx) {
            Ok(destination) => destination,
            Err(err) => {
                self.error = Some(err);
                cx.notify();
                return;
            }
        };
        let revision = form.base_revision;
        let client = self.client.clone();
        self.operation = Operation::Testing;
        self.error = None;
        self.feedback = None;
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.test_backup_destination(revision, destination).await;
            let _ = this.update(cx, |view, cx| {
                view.operation = Operation::Idle;
                match result {
                    Ok(message) => {
                        if let Some(Editor::Destination(form)) = &mut view.editor {
                            form.verified = true;
                        }
                        view.feedback = Some(message);
                    }
                    Err(err) => view.error = Some(err.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn save_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable(cx) {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let mut settings = BackupSettingsInput::from(snapshot);
        let result = match &self.editor {
            Some(Editor::Destination(form)) if form.verified => {
                settings.expected_revision = form.base_revision;
                form.input(cx)
                    .map(|destination| settings.destination = Some(destination))
            }
            Some(Editor::Destination(_)) => Err("Test this destination before saving.".into()),
            Some(Editor::Policy(form)) => {
                settings.expected_revision = form.base_revision;
                form.values(cx).map(|(interval, keep)| {
                    settings.interval_secs = interval;
                    settings.keep = keep;
                })
            }
            None => return,
        };
        if let Err(err) = result {
            self.error = Some(err);
            cx.notify();
            return;
        }
        self.save_settings(settings, window, cx);
    }

    fn toggle_remote(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable(cx) || self.editor.is_some() {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let mut settings = BackupSettingsInput::from(snapshot);
        settings.enabled = enabled;
        self.save_settings(settings, window, cx);
    }

    fn save_settings(
        &mut self,
        settings: BackupSettingsInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        self.operation = Operation::Saving;
        self.error = None;
        self.feedback = None;
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.save_backups(settings).await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.operation = Operation::Idle;
                match result {
                    Ok(snapshot) => {
                        view.accept(snapshot, window, cx);
                        view.editor = None;
                        view.feedback = Some("Backup settings saved.".into());
                    }
                    Err(err) => view.error = Some(format!("{err} Your edits have been kept. Cancel and reopen the editor to reload saved settings.")),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn cancel_local_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() {
            return;
        }
        if let Some((revision, local)) = self
            .snapshot
            .as_ref()
            .and_then(|s| Some((s.revision, s.local.as_ref()?)))
        {
            self.local_form = Some(LocalForm::new(revision, local, window, cx));
            self.error = None;
            self.feedback = None;
            cx.notify();
        }
    }

    fn save_local_schedule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy() || self.editor.is_some() {
            return;
        }
        if self
            .snapshot
            .as_ref()
            .and_then(|s| s.local.as_ref())
            .is_none_or(|local| local.managed_by_env)
        {
            return;
        }
        let Some(form) = &self.local_form else { return };
        let settings = match form.input(cx) {
            Ok(settings) => settings,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let client = self.client.clone();
        self.operation = Operation::SavingLocal;
        self.error = None;
        self.feedback = None;
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.save_local_backups(settings).await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.operation = Operation::Idle;
                match result {
                    Ok(snapshot) => {
                        view.local_form = None;
                        view.accept(snapshot, window, cx);
                        view.feedback = Some("Local backup schedule saved.".into());
                    }
                    Err(error) => view.error = Some(format!("{error} Your edits have been kept. Cancel to reload the saved schedule.")),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn run_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editable(cx) {
            return;
        }
        let client = self.client.clone();
        self.operation = Operation::Starting;
        self.error = None;
        self.feedback = None;
        self._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.clone().run_backup().await;
            let snapshot = if result.is_ok() {
                client.backups().await.ok()
            } else {
                None
            };
            let _ = this.update_in(cx, |view, window, cx| {
                view.operation = Operation::Idle;
                match result {
                    Ok(()) => {
                        if let Some(snapshot) = snapshot {
                            view.accept(snapshot, window, cx);
                        }
                        view.feedback =
                            Some("Backup started. You can leave settings while it runs.".into());
                    }
                    Err(err) => view.error = Some(err.to_string()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

impl Render for View {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_page(window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::rebuild_local_form;

    #[test]
    fn backup_local_form_reloads_only_without_unsaved_edits() {
        assert!(rebuild_local_form(None, 4));
        assert!(rebuild_local_form(Some((false, 3)), 4));
        assert!(!rebuild_local_form(Some((false, 4)), 4));
        assert!(!rebuild_local_form(Some((true, 3)), 4));
    }
}
