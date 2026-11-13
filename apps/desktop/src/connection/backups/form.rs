use super::*;
use crate::api::{BackupDestinationInput, BackupService, LocalBackupSettings};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::Input;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    Disableable as _, FocusableExt as _, Icon, Selectable as _, Sizable as _, h_flex,
};

pub(super) enum Editor {
    Destination(DestinationForm),
    Policy(PolicyForm),
}

pub(super) struct DestinationForm {
    pub base_revision: i64,
    pub verified: bool,
    service: BackupService,
    endpoint: Entity<InputState>,
    bucket: Entity<InputState>,
    prefix: Entity<InputState>,
    access: Entity<InputState>,
    secret: Entity<InputState>,
    region: Entity<InputState>,
    saved_secret: bool,
    path_style: bool,
    advanced: bool,
    _subscriptions: Vec<Subscription>,
}

fn input(
    value: &str,
    placeholder: &'static str,
    window: &mut Window,
    cx: &mut Context<Shell>,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
}

impl DestinationForm {
    pub fn new(snapshot: &BackupSnapshot, window: &mut Window, cx: &mut Context<Shell>) -> Self {
        let destination = snapshot.destination.as_ref();
        let endpoint = input(
            destination.map_or("", |d| &d.endpoint),
            "https://<account>.r2.cloudflarestorage.com",
            window,
            cx,
        );
        let bucket = input(
            destination.map_or("", |d| &d.bucket),
            "else-backups",
            window,
            cx,
        );
        let prefix = input(
            destination.map_or("else-backups/", |d| &d.prefix),
            "my-workspace/",
            window,
            cx,
        );
        let access = input(
            destination.map_or("", |d| &d.access_key_id),
            "Access key ID",
            window,
            cx,
        );
        let secret = cx.new(|cx| {
            InputState::new(window, cx).masked(true).placeholder(
                if destination.is_some_and(|d| d.has_secret) {
                    "Saved on server. Leave blank to keep."
                } else {
                    "Secret access key"
                },
            )
        });
        let region = input(
            destination.map_or("auto", |d| &d.region),
            "auto",
            window,
            cx,
        );
        let subscriptions = [&endpoint, &bucket, &prefix, &access, &secret, &region]
            .into_iter()
            .map(|state| {
                cx.subscribe_in(state, window, |shell, _, event: &InputEvent, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        if let Some(view) = &mut shell.backups {
                            if let Some(Editor::Destination(form)) = &mut view.editor {
                                form.verified = false;
                            }
                            view.feedback = None;
                            view.error = None;
                        }
                        cx.notify();
                    }
                })
            })
            .collect();
        Self {
            base_revision: snapshot.revision,
            verified: false,
            service: destination.map_or(BackupService::R2, |d| d.service),
            endpoint,
            bucket,
            prefix,
            access,
            secret,
            region,
            saved_secret: destination.is_some_and(|d| d.has_secret),
            path_style: destination.is_none_or(|d| d.path_style),
            advanced: false,
            _subscriptions: subscriptions,
        }
    }

    pub fn input(&self, cx: &App) -> Result<BackupDestinationInput, String> {
        let secret = self.secret.read(cx).value().to_string();
        let destination = BackupDestinationInput {
            service: self.service,
            endpoint: self.endpoint.read(cx).value().trim().into(),
            bucket: self.bucket.read(cx).value().trim().into(),
            prefix: self.prefix.read(cx).value().trim().into(),
            access_key_id: self.access.read(cx).value().trim().into(),
            region: self.region.read(cx).value().trim().into(),
            path_style: self.path_style,
            secret_access_key: (!secret.is_empty()).then_some(secret),
        };
        if destination.endpoint.is_empty()
            || destination.bucket.is_empty()
            || destination.access_key_id.is_empty()
            || (!self.saved_secret && destination.secret_access_key.is_none())
        {
            return Err("Enter an endpoint, bucket, access key ID, and secret access key.".into());
        }
        Ok(destination)
    }

    fn field(&self, label: &'static str, state: &Entity<InputState>, busy: bool) -> Div {
        v_flex()
            .flex_1()
            .min_w_0()
            .gap_1p5()
            .child(div().text_xs().child(label))
            .child(
                Input::new(state)
                    .focus_ring(false)
                    .aria_label(label)
                    .disabled(busy),
            )
    }

    pub fn render(&self, busy: bool, stacked: bool, cx: &mut Context<Shell>) -> Div {
        let theme = cx.theme();
        let mut services = h_flex().gap_2().w_full();
        for service in [
            BackupService::R2,
            BackupService::S3,
            BackupService::Compatible,
        ] {
            services = services.child(
                Button::new(ElementId::Name(
                    format!("backup-service-{service:?}").into(),
                ))
                .focus_ring(false)
                .cursor_pointer()
                .small()
                .label(service.label())
                .flex_1()
                .selected(self.service == service)
                .toggled(self.service == service)
                .disabled(busy)
                .on_click(cx.listener(move |shell, _, window, cx| {
                    shell.select_backup_service(service, window, cx)
                })),
            );
        }
        v_flex().gap_4()
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Backup destination"))
            .child(v_flex().gap_2().child(div().text_xs().child("Storage service")).child(services))
            .child(self.field("Endpoint URL", &self.endpoint, busy))
            .child(h_flex().items_start().gap_4().when(stacked, |row| row.flex_col()).child(self.field("Bucket", &self.bucket, busy)).child(self.field("Folder / key prefix", &self.prefix, busy)))
            .child(h_flex().items_start().gap_4().when(stacked, |row| row.flex_col()).child(self.field("Access key ID", &self.access, busy)).child(self.field("Secret access key", &self.secret, busy)))
            .child(div().text_xs().text_color(theme.muted_foreground).child(if self.saved_secret { "Leave the secret blank to keep the saved key. A new key replaces it only after saving." } else { "Credentials are encrypted on the workspace server." }))
            .child(div().text_xs().text_color(theme.muted_foreground).child("Backups share unchanged data between copies. Do not add a bucket rule that expires objects under this prefix: it would delete data newer backups still use."))
            .child(Button::new("backup-advanced").focus_ring(false).cursor_pointer().small().label(if self.advanced { "Hide advanced settings" } else { "Advanced connection settings" }).disabled(busy).on_click(cx.listener(|shell, _, _, cx| {
                if let Some(Editor::Destination(form)) = shell.backups.as_mut().and_then(|view| view.editor.as_mut()) { form.advanced.toggle(); }
                cx.notify();
            })))
            .when(self.advanced, |content| content.child(self.field("Region", &self.region, busy)).child(h_flex().gap_3().child(Switch::new("backup-path-style").focus_ring(false).cursor_pointer().small().checked(self.path_style).disabled(busy).accessibility_label("Use path-style addressing").on_click(cx.listener(|shell, checked: &bool, _, cx| {
                if let Some(Editor::Destination(form)) = shell.backups.as_mut().and_then(|view| view.editor.as_mut()) { form.path_style = *checked; form.verified = false; }
                cx.notify();
            }))).child(div().text_xs().child("Use path-style addressing"))))
    }
}

pub(super) struct PolicyForm {
    pub base_revision: i64,
    hours: Entity<InputState>,
    keep: Entity<InputState>,
    _edits: Vec<Subscription>,
}

impl PolicyForm {
    fn hours_value(seconds: u64) -> String {
        (seconds as f64 / 3600.).to_string()
    }

    pub fn new(snapshot: &BackupSnapshot, window: &mut Window, cx: &mut Context<Shell>) -> Self {
        let hours = input(&Self::hours_value(snapshot.interval_secs), "24", window, cx);
        let keep = input(&snapshot.keep.to_string(), "7", window, cx);
        let edits = [&hours, &keep]
            .into_iter()
            .map(|state| cx.subscribe_in(state, window, |_, _, _: &InputEvent, _, cx| cx.notify()))
            .collect();
        Self {
            base_revision: snapshot.revision,
            hours,
            keep,
            _edits: edits,
        }
    }

    pub fn values(&self, cx: &App) -> Result<(u64, usize), String> {
        let hours: f64 = self.hours.read(cx).value().trim().parse().map_err(|_| {
            "Enter a backup interval in hours. Use 0 for manual backups.".to_owned()
        })?;
        if !hours.is_finite()
            || !(0. ..=720.).contains(&hours)
            || (hours > 0. && hours * 3600. < 60.)
        {
            return Err(
                "Use 0 for manual backups, or an interval from 1 minute to 30 days.".into(),
            );
        }
        let keep: usize = self
            .keep
            .read(cx)
            .value()
            .trim()
            .parse()
            .map_err(|_| "Enter a whole number of backups to keep.".to_owned())?;
        if !(1..=1000).contains(&keep) {
            return Err("Keep between 1 and 1,000 backups.".into());
        }
        Ok(((hours * 3600.).round() as u64, keep))
    }

    pub fn render(
        &self,
        snapshot: &BackupSnapshot,
        busy: bool,
        stacked: bool,
        cx: &mut Context<Shell>,
    ) -> Div {
        let theme = cx.theme();
        let field = |label, state| {
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1p5()
                .child(div().text_xs().child(label))
                .child(
                    Input::new(state)
                        .focus_ring(false)
                        .aria_label(label)
                        .disabled(busy),
                )
        };
        let mut presets = h_flex().gap_2().flex_wrap();
        for (seconds, label) in [
            (21_600, "Every 6h"),
            (86_400, "Every 24h"),
            (604_800, "Every 7 days"),
            (0, "Manual only"),
        ] {
            presets = presets.child(
                Button::new(ElementId::Name(format!("backup-interval-{seconds}").into()))
                    .focus_ring(false)
                    .cursor_pointer()
                    .small()
                    .label(label)
                    .disabled(busy)
                    .on_click(cx.listener(move |shell, _, window, cx| {
                        if let Some(Editor::Policy(form)) =
                            shell.backups.as_mut().and_then(|view| view.editor.as_mut())
                        {
                            form.hours.update(cx, |input, cx| {
                                input.set_value(Self::hours_value(seconds), window, cx)
                            });
                        }
                        cx.notify();
                    })),
            );
        }
        v_flex().gap_4().child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Schedule & retention"))
            .child(presets)
            .child(h_flex().gap_4().items_start().when(stacked, |row| row.flex_col()).child(field("Remote interval (hours)", &self.hours)).child(field("Backups to keep", &self.keep)))
            .child(div().text_xs().text_color(theme.muted_foreground).child("Use 0 hours for manual backups. Local backups keep their existing schedule."))
            .child(div().text_xs().text_color(theme.muted_foreground).child("Only complete remote backups count. Older copies are removed after a new backup is verified. The last full workspace backup may be kept in addition to this count."))
            .when_some(snapshot.estimate.as_ref().zip(self.values(cx).ok()), |content, (estimate, (_, keep))| content.child(div().text_xs().child(format!("The first upload is about {}. Later backups add only what changed, so {keep} copies store far less than {keep} full copies.", byte_size(estimate.bytes)))))
    }
}

impl Shell {
    fn select_backup_service(
        &mut self,
        service: BackupService,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = &mut self.backups else {
            return;
        };
        let Some(Editor::Destination(form)) = &mut view.editor else {
            return;
        };
        if form.service == service {
            return;
        }
        form.service = service;
        form.verified = false;
        let (endpoint, region) = match service {
            BackupService::R2 => ("", "auto"),
            BackupService::S3 => ("https://s3.us-east-1.amazonaws.com", "us-east-1"),
            BackupService::Compatible => ("", "us-east-1"),
        };
        form.endpoint
            .update(cx, |input, cx| input.set_value(endpoint, window, cx));
        form.region
            .update(cx, |input, cx| input.set_value(region, window, cx));
        view.feedback = None;
        view.error = None;
        cx.notify();
    }
}

pub(super) struct LocalForm {
    pub base_revision: i64,
    enabled: bool,
    time: Entity<InputState>,
    keep: Entity<InputState>,
    saved: LocalBackupSettings,
    _edits: Vec<Subscription>,
}

fn local_values(time: &str, keep: &str) -> Result<(String, usize), String> {
    let time = time.trim();
    let valid_time = time.len() == 5
        && time.as_bytes()[2] == b':'
        && time
            .bytes()
            .enumerate()
            .all(|(i, c)| i == 2 || c.is_ascii_digit())
        && time[..2].parse::<u8>().is_ok_and(|hour| hour < 24)
        && time[3..].parse::<u8>().is_ok_and(|minute| minute < 60);
    if !valid_time {
        return Err("Enter a daily backup time in 24-hour HH:MM format.".into());
    }
    let keep = keep
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|keep| (1..=1000).contains(keep))
        .ok_or("Keep between 1 and 1,000 local backups.")?;
    Ok((time.into(), keep))
}

impl LocalForm {
    pub fn new(
        revision: i64,
        local: &LocalBackupSettings,
        window: &mut Window,
        cx: &mut Context<Shell>,
    ) -> Self {
        let time = input(&local.time, "03:00", window, cx);
        let keep = input(&local.keep.to_string(), "7", window, cx);
        let edits = [&time, &keep]
            .into_iter()
            .map(|field| cx.subscribe_in(field, window, |_, _, _: &InputEvent, _, cx| cx.notify()))
            .collect();
        Self {
            base_revision: revision,
            enabled: local.enabled,
            time,
            keep,
            saved: local.clone(),
            _edits: edits,
        }
    }

    pub fn dirty(&self, cx: &App) -> bool {
        self.enabled != self.saved.enabled
            || self.time.read(cx).value().trim() != self.saved.time
            || self.keep.read(cx).value().trim() != self.saved.keep.to_string()
    }

    pub fn input(&self, cx: &App) -> Result<crate::api::LocalBackupSettingsInput, String> {
        let (time, keep) = local_values(&self.time.read(cx).value(), &self.keep.read(cx).value())?;
        Ok(crate::api::LocalBackupSettingsInput {
            expected_revision: self.base_revision,
            enabled: self.enabled,
            time,
            keep,
        })
    }

    pub fn render(
        &self,
        local: &LocalBackupSettings,
        saving: bool,
        busy: bool,
        stacked: bool,
        cx: &mut Context<Shell>,
    ) -> Div {
        use gpui_kit::component::button::ButtonVariants as _;
        let theme = cx.theme();
        let managed = local.managed_by_env;
        let disabled = busy || managed;
        let selected = self.time.read(cx).value();
        let mut presets = h_flex().gap_2().flex_wrap();
        for time in ["01:00", "02:00", "03:00", "04:00", "05:00"] {
            presets = presets.child(
                Button::new(ElementId::Name(format!("local-backup-{time}").into()))
                    .focus_ring(false)
                    .cursor_pointer()
                    .small()
                    .label(if time == "03:00" {
                        "03:00 · recommended"
                    } else {
                        time
                    })
                    .selected(selected.as_ref() == time)
                    .toggled(selected.as_ref() == time)
                    .disabled(disabled)
                    .on_click(cx.listener(move |shell, _, window, cx| {
                        if let Some(form) = shell
                            .backups
                            .as_mut()
                            .and_then(|view| view.local_form.as_mut())
                        {
                            form.time
                                .update(cx, |field, cx| field.set_value(time, window, cx));
                        }
                        cx.notify();
                    })),
            );
        }
        let field = |label, state| {
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_2()
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(label))
                .child(
                    Input::new(state)
                        .focus_ring(false)
                        .aria_label(label)
                        .disabled(disabled),
                )
        };
        let note = |text: String| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .whitespace_normal()
                .child(text)
        };
        let heading =
            |text: &'static str| div().text_sm().font_weight(FontWeight::MEDIUM).child(text);
        // The lock sits under the heading it locks, in place of the
        // description of what a backup does to running work.
        let explanation = if managed {
            h_flex()
                .gap_2()
                .items_start()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(Icon::new(IconName::Lock).size_3().mt(px(2.)).flex_none())
                .child(div().flex_1().min_w_0().child(
                    "Locked by ELSE_BACKUP_INTERVAL_SECS on the server. Change that variable to \
                     edit this schedule.",
                ))
        } else {
            h_flex().child(note(
                "else.bot pauses new work while it backs up. Running tasks finish first.".into(),
            ))
        };
        let schedule = v_flex()
            .gap_4()
            .child(
                h_flex()
                    .gap_3()
                    .items_start()
                    .justify_between()
                    .when(stacked, |row| row.flex_col())
                    .child(
                        v_flex()
                            .gap_1()
                            .flex_1()
                            .min_w_0()
                            .child(heading("Daily backup time"))
                            .child(explanation),
                    )
                    .child(
                        h_flex().gap_2().text_xs().child("Automatic").child(
                            Switch::new("local-backups-enabled")
                                .focus_ring(false)
                                .cursor_pointer()
                                .small()
                                .checked(self.enabled)
                                .disabled(disabled)
                                .accessibility_label("Automatic daily local backups")
                                .on_click(cx.listener(|shell, enabled: &bool, _, cx| {
                                    if let Some(form) = shell
                                        .backups
                                        .as_mut()
                                        .and_then(|view| view.local_form.as_mut())
                                    {
                                        form.enabled = *enabled;
                                    }
                                    cx.notify();
                                })),
                        ),
                    ),
            )
            .child(presets)
            .child(
                h_flex()
                    .gap_4()
                    .items_start()
                    .when(stacked, |row| row.flex_col())
                    .child(field("Exact time", &self.time))
                    .child(div().flex_1().min_w_0()),
            )
            .child(note(format!(
                "Times use the server's timezone ({}). A missed backup runs at the next \
                 scheduled time.",
                local.timezone
            )));
        let retention = v_flex()
            .gap_4()
            .child(
                h_flex()
                    .gap_4()
                    .items_start()
                    .when(stacked, |row| row.flex_col())
                    .child(field("Keep copies", &self.keep))
                    .child(div().flex_1().min_w_0()),
            )
            .child(note(
                "Old copies are removed after a new backup is verified.".into(),
            ));
        v_flex()
            .gap_8()
            .child(schedule)
            .child(retention)
            .when(!managed, |panel| {
                panel.child(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("local-backup-cancel")
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Cancel")
                                .disabled(disabled || !self.dirty(cx))
                                .on_click(cx.listener(|shell, _, window, cx| {
                                    shell.cancel_local_backup_edit(window, cx)
                                })),
                        )
                        .child(
                            Button::new("local-backup-save")
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Save schedule")
                                .primary()
                                .loading(saving)
                                .disabled(disabled || !self.dirty(cx))
                                .on_click(cx.listener(|shell, _, window, cx| {
                                    shell.save_local_backup_schedule(window, cx)
                                })),
                        ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::local_values;

    #[test]
    fn backup_local_schedule_validates_time_and_retention() {
        assert_eq!(local_values("03:00", "7"), Ok(("03:00".into(), 7)));
        assert_eq!(local_values("23:59", "1000"), Ok(("23:59".into(), 1000)));
        assert_eq!(local_values("00:00", "1"), Ok(("00:00".into(), 1)));
        for time in ["3:00", "24:00", "03:60", "03.00", "03:0a", "é:00", ""] {
            assert!(local_values(time, "7").is_err(), "{time}");
        }
        for keep in ["0", "1001", "-1", "7.5", ""] {
            assert!(local_values("03:00", keep).is_err(), "{keep}");
        }
    }
}
