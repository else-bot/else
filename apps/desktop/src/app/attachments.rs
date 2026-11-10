mod view;
use gpui_kit::component::FocusableExt as _;
use view::{Preview, show_preview};
pub(super) use view::{chip, draft_chip};

use super::{Draft, ElseApp};
use crate::api::ClientError;
use crate::attachments::{Attachment, MAX_FILE_BYTES, MAX_FILES, MessagePart};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::InputContent;
use gpui_kit::component::{WindowExt as _, h_flex};
use gpui_kit::*;
use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(super) enum PasteSource {
    Path(PathBuf),
    Bytes(Arc<Vec<u8>>),
    Snapshot {
        file: Arc<tempfile::NamedTempFile>,
        size: u64,
    },
}

impl PasteSource {
    fn read(self) -> Result<Arc<Vec<u8>>, String> {
        let bytes = match self {
            Self::Bytes(bytes) => {
                return if bytes.len() <= MAX_FILE_BYTES {
                    Ok(bytes)
                } else {
                    Err("Files must be at most 10 MB.".into())
                };
            }
            Self::Snapshot { file, .. } => return Self::Path(file.path().to_owned()).read(),
            Self::Path(path) => {
                let file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
                if !file.metadata().map_err(|e| e.to_string())?.is_file() {
                    return Err("Only files can be attached.".into());
                }
                let mut bytes = Vec::new();
                file.take(MAX_FILE_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                bytes
            }
        };
        if bytes.len() > MAX_FILE_BYTES {
            return Err("Files must be at most 10 MB.".into());
        }
        Ok(Arc::new(bytes))
    }

    fn load(self) -> Result<(Arc<Vec<u8>>, Self), String> {
        let existing = match &self {
            Self::Snapshot { file, .. } => Some(file.clone()),
            _ => None,
        };
        let bytes = self.read()?;
        let snapshot = match existing {
            Some(file) => file,
            None => {
                let mut file = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
                file.write_all(&bytes).map_err(|e| e.to_string())?;
                Arc::new(file)
            }
        };
        let source = Self::Snapshot {
            file: snapshot,
            size: bytes.len() as u64,
        };
        Ok((bytes, source))
    }

    fn size(&self) -> u64 {
        match self {
            Self::Path(_) => 0,
            Self::Bytes(bytes) => bytes.len() as u64,
            Self::Snapshot { size, .. } => *size,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) enum UploadState {
    Preparing { source: PasteSource },
    Uploading { source: PasteSource },
    Ready { file: Attachment },
    Failed { source: PasteSource, error: String },
    Unconfirmed { source: PasteSource, error: String },
}

impl UploadState {
    fn begin_upload(&mut self) -> Option<PasteSource> {
        let source = match self {
            Self::Preparing { source }
            | Self::Failed { source, .. }
            | Self::Unconfirmed { source, .. } => source.clone(),
            Self::Uploading { .. } | Self::Ready { .. } => return None,
        };
        *self = Self::Uploading {
            source: source.clone(),
        };
        Some(source)
    }

    fn loaded(&mut self, snapshot: PasteSource) -> bool {
        let Self::Uploading { source } = self else {
            return false;
        };
        *source = snapshot;
        true
    }

    fn fail(&mut self, error: String) -> bool {
        let Self::Uploading { source } = self else {
            return false;
        };
        *self = Self::Failed {
            source: source.clone(),
            error,
        };
        true
    }

    fn complete(&mut self, result: Result<Attachment, ClientError>) -> bool {
        let Self::Uploading { source } = self else {
            return false;
        };
        *self = match result {
            Ok(file) => Self::Ready { file },
            Err(error) if error.outcome_unknown() => Self::Unconfirmed {
                source: source.clone(),
                error: error.to_string(),
            },
            Err(error) => Self::Failed {
                source: source.clone(),
                error: error.to_string(),
            },
        };
        true
    }
}

#[derive(Debug, Clone)]
pub(super) struct DraftFile {
    name: String,
    media_type: String,
    pub(super) state: UploadState,
    image: Option<Arc<Image>>,
}

impl DraftFile {
    fn preparing(name: String, media_type: String, source: PasteSource) -> Self {
        Self {
            name,
            media_type,
            state: UploadState::Preparing { source },
            image: None,
        }
    }

    pub(super) fn ready(file: Attachment) -> Self {
        Self {
            name: file.name.clone(),
            media_type: file.media_type.clone(),
            state: UploadState::Ready { file },
            image: None,
        }
    }

    fn metadata(&self, id: Uuid, size: u64) -> Attachment {
        Attachment {
            id,
            name: self.name.clone(),
            media_type: self.media_type.clone(),
            size,
        }
    }

    pub(super) fn preview_metadata(&self, id: Uuid) -> Attachment {
        match &self.state {
            UploadState::Ready { file } => file.clone(),
            UploadState::Preparing { source }
            | UploadState::Uploading { source }
            | UploadState::Failed { source, .. }
            | UploadState::Unconfirmed { source, .. } => self.metadata(id, source.size()),
        }
    }
}

pub(super) fn active_file_ids(content: &InputContent) -> impl Iterator<Item = Uuid> + '_ {
    content
        .tokens()
        .iter()
        .filter_map(|span| crate::attachments::file_id(span.token().id()))
}

impl Draft {
    pub(super) fn set_content(&mut self, content: InputContent) -> bool {
        let changed = !active_file_ids(&content).eq(active_file_ids(&self.content));
        self.content = content;
        if changed {
            let active: std::collections::HashSet<_> = active_file_ids(&self.content).collect();
            for (id, file) in &mut self.files {
                if !active.contains(id) {
                    file.image = None;
                }
            }
        }
        changed
    }
}

pub(super) fn parts(
    content: &InputContent,
    files: &std::collections::HashMap<Uuid, DraftFile>,
) -> Result<Vec<MessagePart>, String> {
    if content.tokens().is_empty() {
        return Ok(Vec::new());
    }
    let mut parts = Vec::new();
    let mut offset = 0;
    for span in content.tokens() {
        let id = crate::attachments::file_id(span.token().id()).ok_or("Invalid file token.")?;
        let file = files
            .get(&id)
            .ok_or("The attached file is no longer available. Paste it again.")?;
        let attachment = match &file.state {
            UploadState::Ready { file } => file,
            UploadState::Preparing { .. } | UploadState::Uploading { .. } => {
                return Err("Wait for the files to finish uploading.".into());
            }
            UploadState::Failed { error, .. } => {
                return Err(format!("{} could not upload: {error}", file.name));
            }
            UploadState::Unconfirmed { error, .. } => {
                return Err(format!(
                    "{} upload is unconfirmed: {error}. Retry the upload.",
                    file.name
                ));
            }
        };
        let range = span.range();
        if offset < range.start {
            parts.push(MessagePart::Text {
                text: content.text()[offset..range.start].to_owned(),
            });
        }
        parts.push(MessagePart::File {
            file: attachment.clone(),
        });
        offset = range.end;
    }
    if offset < content.text().len() {
        parts.push(MessagePart::Text {
            text: content.text()[offset..].to_owned(),
        });
    }
    Ok(parts)
}

pub(super) fn append(first: InputContent, second: InputContent) -> InputContent {
    if first.text().trim().is_empty() {
        return second;
    }
    let offset = first.text().len() + 2;
    let mut combined = InputContent::new(format!("{}\n\n{}", first.text(), second.text()));
    for span in first.tokens() {
        combined = combined
            .with_token(span.range(), span.token().clone())
            .expect("existing token range");
    }
    for span in second.tokens() {
        let range = span.range();
        combined = combined
            .with_token(
                range.start + offset..range.end + offset,
                span.token().clone(),
            )
            .expect("shifted token range");
    }
    combined
}

impl ElseApp {
    pub(super) fn composer_changed(&mut self, cx: &mut Context<Self>) {
        let Some(bot) = self.selected else {
            return;
        };
        let content = self.composer.read(cx).content();
        let draft = self.drafts.entry(bot).or_default();
        if draft.set_content(content) {
            cx.notify();
        }
    }

    pub(super) fn paste_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.clipboard_read.is_some() {
            self.notice = Some("The previous paste is still loading.".into());
            cx.notify();
            return true;
        }
        let clipboard = cx.read_from_clipboard();
        if let Some(item) = &clipboard
            && self.paste_files(item, window, cx)
        {
            return true;
        }
        #[cfg(target_os = "linux")]
        if std::env::var_os("WAYLAND_DISPLAY").is_some()
            && crate::attachments::clipboard::candidate(
                clipboard.as_ref().and_then(|item| item.text()).as_deref(),
            )
        {
            let content = self.composer.read(cx).content();
            let selection = self.composer.read(cx).selected_range();
            let bot = self.selected;
            let fallback = clipboard.and_then(|item| item.text());
            self.clipboard_read = Some(cx.spawn_in(window, async move |this, cx| {
                let paths = cx
                    .background_executor()
                    .spawn(async { crate::attachments::clipboard::file_paths() })
                    .await;
                let _ = this.update_in(cx, |app, window, cx| {
                    app.clipboard_read = None;
                    let state = app.composer.read(cx);
                    if app.selected != bot
                        || state.content() != content
                        || state.selected_range() != selection
                        || !state.focus_handle(cx).is_focused(window)
                    {
                        return;
                    }
                    match paths {
                        Some(Ok(paths)) => {
                            app.paste_files(
                                &ClipboardItem {
                                    entries: vec![ClipboardEntry::ExternalPaths(ExternalPaths(
                                        paths.into(),
                                    ))],
                                },
                                window,
                                cx,
                            );
                        }
                        Some(Err(error)) => {
                            app.notice = Some(error);
                            cx.notify();
                        }
                        None => {
                            if let Some(text) = fallback {
                                app.composer
                                    .update(cx, |state, cx| state.replace(text, window, cx));
                            }
                        }
                    }
                });
            }));
            return true;
        }
        false
    }

    pub(super) fn paste_files(
        &mut self,
        item: &ClipboardItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(bot) = self.selected else {
            return false;
        };
        let mut incoming = Vec::new();
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) if image.bytes.len() > MAX_FILE_BYTES => {
                    self.notice = Some("Files must be at most 10 MB.".into());
                    cx.notify();
                    return true;
                }
                ClipboardEntry::Image(image) => incoming.push((
                    format!("image.{}", image.format.extension()),
                    image.format.mime_type().to_owned(),
                    PasteSource::Bytes(Arc::new(image.bytes.clone())),
                )),
                ClipboardEntry::ExternalPaths(paths) => {
                    for path in &paths.0 {
                        let name = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        let mime = mime_guess::from_path(path)
                            .first_or_octet_stream()
                            .to_string();
                        incoming.push((name, mime, PasteSource::Path(path.clone())));
                    }
                }
                _ => {}
            }
        }
        if incoming.is_empty() {
            return false;
        }
        let editor = self.composer.read(cx);
        let selection = editor.selected_range();
        let count = editor
            .tokens()
            .iter()
            .filter(|span| {
                let range = span.range();
                range.end <= selection.start || range.start >= selection.end
            })
            .count();
        if count + incoming.len() > MAX_FILES {
            self.notice = Some(format!("Attach at most {MAX_FILES} files per message."));
            cx.notify();
            return true;
        }
        let uploading = self
            .drafts
            .values()
            .flat_map(|draft| draft.files.values())
            .filter(|file| {
                matches!(
                    file.state,
                    UploadState::Preparing { .. } | UploadState::Uploading { .. }
                )
            })
            .count();
        if uploading + incoming.len() > MAX_FILES {
            self.notice = Some("Wait for the current uploads before pasting more files.".into());
            cx.notify();
            return true;
        }
        if incoming.iter().any(|(name, _, _)| {
            name.is_empty()
                || name.len() > 255
                || name
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
        }) {
            self.notice = Some(
                "This filename cannot be attached. Rename the file and paste it again.".into(),
            );
            cx.notify();
            return true;
        }
        for (name, media_type, source) in incoming {
            let file = Attachment {
                id: Uuid::new_v4(),
                name,
                media_type,
                size: 0,
            };
            let inserted = self.composer.update(cx, |state, cx| {
                state.replace_with_token(file.token(), window, cx)
            });
            if let Err(error) = inserted {
                self.notice = Some(format!("Could not paste file: {error}"));
                break;
            }
            let id = file.id;
            self.drafts
                .entry(bot)
                .or_default()
                .files
                .insert(id, DraftFile::preparing(file.name, file.media_type, source));
            self.upload_file(bot, id, window, cx);
        }
        let content = self.composer.read(cx).content();
        if let Some(draft) = self.drafts.get_mut(&bot) {
            draft.set_content(content);
        }
        cx.notify();
        true
    }

    fn upload_file(&mut self, bot: Uuid, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(file) = self.drafts.get_mut(&bot).and_then(|d| d.files.get_mut(&id)) else {
            return;
        };
        let Some(source) = file.state.begin_upload() else {
            return;
        };
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move { source.load() })
                .await;
            let (loaded, snapshot) = match loaded {
                Ok(loaded) => loaded,
                Err(error) => {
                    let _ = this.update(cx, |app, cx| {
                        if let Some(file) =
                            app.drafts.get_mut(&bot).and_then(|d| d.files.get_mut(&id))
                        {
                            file.state.fail(error);
                        }
                        cx.notify();
                    });
                    return;
                }
            };
            let upload = this
                .update(cx, |app, cx| {
                    let content = if app.selected == Some(bot) {
                        app.composer.read(cx).content()
                    } else {
                        app.drafts.get(&bot)?.content.clone()
                    };
                    let active = active_file_ids(&content).any(|active| active == id);
                    let file = app.drafts.get_mut(&bot)?.files.get_mut(&id)?;
                    if !file.state.loaded(snapshot) {
                        return None;
                    }
                    file.image = active
                        .then(|| ImageFormat::from_mime_type(&file.media_type))
                        .flatten()
                        .map(|format| Arc::new(Image::from_bytes(format, loaded.as_ref().clone())));
                    cx.notify();
                    Some(file.metadata(id, loaded.len() as u64))
                })
                .ok()
                .flatten();
            let Some(file) = upload else {
                return;
            };
            let result = client.upload_attachment(bot, file, loaded).await;
            let _ = this.update(cx, |app, cx| {
                if let Some(file) = app.drafts.get_mut(&bot).and_then(|d| d.files.get_mut(&id)) {
                    file.state.complete(result);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn preview_attachment(
        &mut self,
        bot: Uuid,
        file: Attachment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(draft_file) = self.drafts.get(&bot).and_then(|d| d.files.get(&file.id)) {
            let error = match &draft_file.state {
                UploadState::Failed { error, .. } => Some(error.clone()),
                UploadState::Unconfirmed { error, .. } => Some(format!(
                    "The upload may have completed. Retry to confirm it. {error}"
                )),
                _ => None,
            };
            if let Some(error) = error {
                let owner = cx.entity().downgrade();
                let (name, id) = (file.name.clone(), file.id);
                window.open_dialog(cx, move |dialog, _, _| {
                    let owner = owner.clone();
                    dialog.title(name.clone()).child(error.clone()).footer(
                        h_flex().child(
                            Button::new("retry-file-upload")
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Retry upload")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = owner
                                        .update(cx, |app, cx| app.upload_file(bot, id, window, cx));
                                }),
                        ),
                    )
                });
                return;
            }
            if let Some(image) = &draft_file.image {
                show_preview(file, Preview::Image(image.clone()), None, window, cx);
                return;
            }
            if let UploadState::Preparing {
                source: PasteSource::Bytes(bytes),
            }
            | UploadState::Uploading {
                source: PasteSource::Bytes(bytes),
            } = &draft_file.state
            {
                let preview = Preview::from_bytes(&file, bytes.as_ref().clone());
                show_preview(file, preview, Some(bytes.clone()), window, cx);
                return;
            }
            if matches!(
                draft_file.state,
                UploadState::Preparing { .. } | UploadState::Uploading { .. }
            ) {
                self.notice = Some("The file is still loading.".into());
                cx.notify();
                return;
            }
        }
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.attachment_bytes(bot, file.id).await;
            let _ = this.update_in(cx, |app, window, cx| match result {
                Ok(bytes) => {
                    let bytes = Arc::new(bytes);
                    let preview = Preview::from_bytes(&file, bytes.as_ref().clone());
                    show_preview(file, preview, Some(bytes), window, cx)
                }
                Err(error) => {
                    app.notice = Some(format!("Could not open {}: {error}", file.name));
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::{DraftFile, PasteSource, UploadState, append, parts};
    use crate::attachments::{Attachment, MAX_FILE_BYTES, MessagePart, input_content};
    use gpui_kit::component::input::InputContent;
    use uuid::Uuid;

    fn file() -> Attachment {
        Attachment {
            id: Uuid::new_v4(),
            name: "capture.png".into(),
            media_type: "image/png".into(),
            size: 42,
        }
    }

    #[test]
    fn removing_the_last_file_token_leaves_no_unsaved_work_and_preserves_undo() {
        let file = file();
        let content = input_content(
            &file.fallback(),
            &[MessagePart::File { file: file.clone() }],
        );
        let mut draft = crate::app::reply::Draft {
            content: content.clone(),
            files: [(file.id, DraftFile::ready(file.clone()))].into(),
            ..Default::default()
        };
        assert!(draft.has_unsaved_work());
        draft.set_content(InputContent::new(""));
        assert!(
            !draft.has_unsaved_work(),
            "removing the last file token must clear the draft's unsaved work"
        );
        assert!(parts(&draft.content, &draft.files).unwrap().is_empty());
        draft.set_content(content);
        assert!(draft.has_unsaved_work());
        assert_eq!(
            parts(&draft.content, &draft.files).unwrap(),
            vec![MessagePart::File { file }]
        );
    }

    #[test]
    fn sending_uses_token_identity_and_preserves_unicode_and_whitespace() {
        let file = file();
        let expected = vec![
            MessagePart::Text {
                text: " 🥔 before\n".into(),
            },
            MessagePart::File { file: file.clone() },
            MessagePart::Text {
                text: " after  ".into(),
            },
        ];
        let text = format!(" 🥔 before\n{} after  ", file.fallback());
        let content = input_content(&text, &expected);
        let files = [(file.id, DraftFile::ready(file))].into_iter().collect();
        assert_eq!(parts(&content, &files).unwrap(), expected);
        assert!(parts(&InputContent::new(text), &files).unwrap().is_empty());
    }

    #[test]
    fn uploading_or_failed_files_cannot_be_sent_as_text() {
        let file = file();
        let content = input_content(
            &file.fallback(),
            &[MessagePart::File { file: file.clone() }],
        );
        let mut entry = DraftFile::preparing(
            file.name.clone(),
            file.media_type.clone(),
            PasteSource::Bytes(std::sync::Arc::new(vec![42])),
        );
        entry.state.begin_upload().unwrap();
        let mut files = [(file.id, entry)]
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        assert!(
            parts(&content, &files)
                .unwrap_err()
                .contains("finish uploading")
        );
        files
            .get_mut(&file.id)
            .unwrap()
            .state
            .fail("offline".into());
        assert!(parts(&content, &files).unwrap_err().contains("offline"));
        files.clear();
        assert!(parts(&content, &files).is_err());
    }

    #[test]
    fn restoring_a_rejected_message_keeps_both_drafts_files() {
        let first = file();
        let second = file();
        let a = input_content(
            &first.fallback(),
            &[MessagePart::File {
                file: first.clone(),
            }],
        );
        let b = input_content(
            &second.fallback(),
            &[MessagePart::File {
                file: second.clone(),
            }],
        );
        let result = append(a, b);
        let files = [
            (first.id, DraftFile::ready(first.clone())),
            (second.id, DraftFile::ready(second.clone())),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            parts(&result, &files).unwrap(),
            vec![
                MessagePart::File { file: first },
                MessagePart::Text {
                    text: "\n\n".into()
                },
                MessagePart::File { file: second }
            ]
        );
    }

    #[test]
    fn upload_retry_uses_the_pasted_snapshot_when_the_original_changes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("notes.txt");
        std::fs::write(&path, "original").unwrap();
        let (bytes, snapshot) = PasteSource::Path(path.clone()).load().unwrap();
        std::fs::write(path, "changed").unwrap();
        assert_eq!(bytes.as_slice(), b"original");
        assert_eq!(snapshot.read().unwrap().as_slice(), b"original");
    }

    #[test]
    fn upload_transitions_keep_retry_sources_and_require_confirmation_before_sending() {
        let file = file();
        let content = input_content(
            &file.fallback(),
            &[MessagePart::File { file: file.clone() }],
        );
        let mut state = UploadState::Preparing {
            source: PasteSource::Bytes(std::sync::Arc::new(vec![42; 42])),
        };
        assert!(!state.complete(Ok(file.clone())));
        assert!(!state.fail("not started".into()));
        let source = state.begin_upload().unwrap();
        assert!(state.begin_upload().is_none());
        assert!(state.fail("could not read file".into()));
        let (bytes, snapshot) = state.begin_upload().unwrap().load().unwrap();
        assert_eq!(source.read().unwrap().as_slice(), bytes.as_slice());
        assert!(state.loaded(snapshot));
        assert!(state.complete(Err(crate::api::ClientError::Unauthorized)));
        assert_eq!(
            state.begin_upload().unwrap().read().unwrap().as_slice(),
            bytes.as_slice()
        );
        assert!(state.complete(Err(crate::api::ClientError::Unreachable {
            origin: "http://127.0.0.1:8080".into(),
            cause: "connection closed".into(),
        })));
        let mut entry = DraftFile::ready(file.clone());
        entry.state = state;
        let mut files = std::collections::HashMap::from([(file.id, entry)]);
        let state = &mut files.get_mut(&file.id).unwrap().state;
        assert!(matches!(state, UploadState::Unconfirmed { .. }));
        assert!(parts(&content, &files).unwrap_err().contains("unconfirmed"));
        let state = &mut files.get_mut(&file.id).unwrap().state;
        assert_eq!(
            state.begin_upload().unwrap().read().unwrap().as_slice(),
            bytes.as_slice()
        );
        assert!(state.complete(Ok(file.clone())));
        assert!(state.begin_upload().is_none());
        assert!(!state.loaded(PasteSource::Bytes(bytes)));
        assert!(!state.complete(Err(crate::api::ClientError::Unauthorized)));
        assert!(!state.fail("late failure".into()));
        assert_eq!(
            parts(&content, &files).unwrap(),
            vec![MessagePart::File { file }]
        );
    }

    #[test]
    fn removing_tokens_clears_only_inactive_previews_and_undo_restores_files() {
        let first = file();
        let second = file();
        let first_part = MessagePart::File {
            file: first.clone(),
        };
        let second_part = MessagePart::File {
            file: second.clone(),
        };
        let content = input_content(
            &format!("{}{}", first.fallback(), second.fallback()),
            &[first_part.clone(), second_part.clone()],
        );
        let mut draft = crate::app::reply::Draft {
            content: content.clone(),
            files: [
                (first.id, DraftFile::ready(first.clone())),
                (second.id, DraftFile::ready(second.clone())),
            ]
            .into(),
            ..Default::default()
        };
        for file in draft.files.values_mut() {
            file.image = Some(std::sync::Arc::new(gpui_kit::Image::from_bytes(
                gpui_kit::ImageFormat::Png,
                vec![42],
            )));
        }
        draft.set_content(input_content(
            &second.fallback(),
            std::slice::from_ref(&second_part),
        ));
        assert!(draft.has_unsaved_work());
        assert!(draft.files[&first.id].image.is_none());
        assert!(draft.files[&second.id].image.is_some());
        assert_eq!(
            parts(&draft.content, &draft.files).unwrap(),
            vec![second_part.clone()]
        );
        draft.set_content(InputContent::new(""));
        assert!(!draft.has_unsaved_work());
        assert!(draft.files.values().all(|file| file.image.is_none()));
        draft.set_content(content);
        assert_eq!(
            parts(&draft.content, &draft.files).unwrap(),
            vec![first_part, second_part]
        );
    }

    #[test]
    fn restoration_replaces_a_whitespace_only_draft() {
        let second = InputContent::new("restored message");
        assert_eq!(append(InputContent::new(" \n  "), second.clone()), second);
    }

    #[test]
    fn file_reads_reject_directories_and_stop_at_the_size_limit() {
        let directory = tempfile::tempdir().unwrap();
        assert!(PasteSource::Path(directory.path().into()).read().is_err());
        let path = directory.path().join("large.bin");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_FILE_BYTES as u64 + 1)
            .unwrap();
        assert!(
            PasteSource::Path(path)
                .read()
                .unwrap_err()
                .contains("10 MB")
        );
    }
}
