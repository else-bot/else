mod form;
mod local;
mod view;

use super::*;
use crate::api::{BackupSettingsInput, BackupSnapshot};
use std::time::Duration;
use uuid::Uuid;

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
    id: Uuid,
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

impl View {
    pub(super) fn has_unsaved_work(&self, cx: &App) -> bool {
        self.editor.is_some()
            || self.busy()
            || self.local_form.as_ref().is_some_and(|form| form.dirty(cx))
    }
    fn busy(&self) -> bool {
        self.operation != Operation::Idle
    }

    fn accept(&mut self, snapshot: BackupSnapshot, window: &mut Window, cx: &mut Context<Shell>) {
        if self
            .snapshot
            .as_ref()
            .is_none_or(|s| snapshot.revision >= s.revision)
        {
            if self.local_form.is_none()
                || (self.local_form.as_ref().is_some_and(|form| !form.dirty(cx))
                    && self
                        .snapshot
                        .as_ref()
                        .is_none_or(|saved| saved.revision != snapshot.revision))
            {
                self.local_form = Some(LocalForm::new(&snapshot, window, cx));
            }
            self.snapshot = Some(snapshot);
        }
    }
}

impl Shell {
    pub(super) fn open_backups(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = &self.active else { return };
        let client = active.client.clone();
        let poll_client = client.clone();
        let id = Uuid::new_v4();
        let poll = cx.spawn_in(window, async move |this, cx| {
            loop {
                let result = poll_client.clone().backups().await;
                let delay = this.update_in(cx, |shell, window, cx| {
                    let view = shell.backups.as_mut().filter(|v| v.id == id)?;
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
                    Some(
                        if view
                            .snapshot
                            .as_ref()
                            .is_some_and(|s| s.running || s.pause.is_some())
                        {
                            Duration::from_secs(2)
                        } else {
                            Duration::from_secs(10)
                        },
                    )
                });
                let Ok(Some(delay)) = delay else { break };
                cx.background_executor().timer(delay).await;
            }
        });
        self.backups = Some(View {
            id,
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
        });
        cx.notify();
    }

    fn edit_backup_destination(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self
            .backups
            .as_ref()
            .filter(|v| !v.busy() && v.local_form.as_ref().is_none_or(|f| !f.dirty(cx)))
        else {
            return;
        };
        let Some(snapshot) = &view.snapshot else {
            return;
        };
        let form = DestinationForm::new(snapshot, window, cx);
        let view = self.backups.as_mut().unwrap();
        view.editor = Some(Editor::Destination(form));
        view.error = None;
        view.feedback = None;
        cx.notify();
    }

    fn edit_backup_policy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self
            .backups
            .as_ref()
            .filter(|v| !v.busy() && v.local_form.as_ref().is_none_or(|f| !f.dirty(cx)))
        else {
            return;
        };
        let Some(snapshot) = &view.snapshot else {
            return;
        };
        let form = PolicyForm::new(snapshot, window, cx);
        let view = self.backups.as_mut().unwrap();
        view.editor = Some(Editor::Policy(form));
        view.error = None;
        view.feedback = None;
        cx.notify();
    }

    fn cancel_backup_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = &mut self.backups {
            if view.busy() {
                return;
            }
            view.editor = None;
            view.error = None;
            view.feedback = None;
            cx.notify();
        }
    }

    fn test_backup_destination(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self
            .backups
            .as_mut()
            .filter(|v| !v.busy() && v.local_form.as_ref().is_none_or(|f| !f.dirty(cx)))
        else {
            return;
        };
        let Some(Editor::Destination(form)) = &view.editor else {
            return;
        };
        let destination = match form.input(cx) {
            Ok(destination) => destination,
            Err(err) => {
                view.error = Some(err);
                cx.notify();
                return;
            }
        };
        let revision = form.base_revision;
        let client = view.client.clone();
        let id = view.id;
        view.operation = Operation::Testing;
        view.error = None;
        view.feedback = None;
        view._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.test_backup_destination(revision, destination).await;
            let _ = this.update_in(cx, |shell, _, cx| {
                let Some(view) = shell.backups.as_mut().filter(|v| v.id == id) else {
                    return;
                };
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

    fn save_backup_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self
            .backups
            .as_mut()
            .filter(|v| !v.busy() && v.local_form.as_ref().is_none_or(|f| !f.dirty(cx)))
        else {
            return;
        };
        let Some(snapshot) = &view.snapshot else {
            return;
        };
        let mut settings = snapshot.input();
        let result = match &view.editor {
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
            view.error = Some(err);
            cx.notify();
            return;
        }
        self.save_backup_settings(settings, window, cx);
    }

    fn toggle_remote_backups(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.backups.as_ref().filter(|v| {
            !v.busy() && v.editor.is_none() && v.local_form.as_ref().is_none_or(|f| !f.dirty(cx))
        }) else {
            return;
        };
        let Some(snapshot) = &view.snapshot else {
            return;
        };
        let mut settings = snapshot.input();
        settings.enabled = enabled;
        self.save_backup_settings(settings, window, cx);
    }

    fn save_backup_settings(
        &mut self,
        settings: BackupSettingsInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self.backups.as_mut().unwrap();
        let client = view.client.clone();
        let id = view.id;
        view.operation = Operation::Saving;
        view.error = None;
        view.feedback = None;
        view._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.save_backups(settings).await;
            let _ = this.update_in(cx, |shell, window, cx| {
                let Some(view) = shell.backups.as_mut().filter(|v| v.id == id) else { return };
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

    fn cancel_local_backup_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self.backups.as_mut().filter(|v| !v.busy()) else {
            return;
        };
        if let Some(snapshot) = &view.snapshot {
            view.local_form = Some(LocalForm::new(snapshot, window, cx));
            view.error = None;
            view.feedback = None;
            cx.notify();
        }
    }

    fn save_local_backup_schedule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self
            .backups
            .as_mut()
            .filter(|v| !v.busy() && v.editor.is_none())
        else {
            return;
        };
        if view
            .snapshot
            .as_ref()
            .is_none_or(|s| s.local.managed_by_env)
        {
            return;
        }
        let Some(form) = &view.local_form else { return };
        let settings = match form.input(cx) {
            Ok(settings) => settings,
            Err(error) => {
                view.error = Some(error);
                cx.notify();
                return;
            }
        };
        let client = view.client.clone();
        let id = view.id;
        view.operation = Operation::SavingLocal;
        view.error = None;
        view.feedback = None;
        view._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.save_local_backups(settings).await;
            let _ = this.update_in(cx, |shell, window, cx| {
                let Some(view) = shell.backups.as_mut().filter(|v| v.id == id) else { return };
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

    fn run_remote_backup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self
            .backups
            .as_mut()
            .filter(|v| !v.busy() && v.local_form.as_ref().is_none_or(|f| !f.dirty(cx)))
        else {
            return;
        };
        let client = view.client.clone();
        let id = view.id;
        view.operation = Operation::Starting;
        view.error = None;
        view.feedback = None;
        view._request = Some(cx.spawn_in(window, async move |this, cx| {
            let result = client.clone().run_backup().await;
            let snapshot = if result.is_ok() {
                client.backups().await.ok()
            } else {
                None
            };
            let _ = this.update_in(cx, |shell, window, cx| {
                let Some(view) = shell.backups.as_mut().filter(|v| v.id == id) else {
                    return;
                };
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
