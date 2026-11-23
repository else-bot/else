//! Files in messages: photos as thumbnails, other files as chips. Tapping
//! one opens the share sheet, where Quick Look, Files and other apps take it.
//! A file's bytes are fetched once with the owner key and kept in the app's
//! temporary folder.

use std::path::PathBuf;

use else_wire::items::Attachment;
use nsh_core::prelude::*;
use nsh_core::share::ShareRequest;
use uuid::Uuid;

use super::parts::colors;
use super::session;

/// A file a message carries, or one the composer is about to send.
pub fn is_image(media_type: &str) -> bool {
    matches!(
        media_type,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// Where a file's bytes are kept: its own folder, so its name stays as the
/// owner sees it in the share sheet.
fn cached(file: &Attachment) -> PathBuf {
    let name: String = file
        .name
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();
    let name = name.trim_start_matches('.');
    let name = if name.is_empty() { "file" } else { name };
    std::env::temp_dir()
        .join("else-files")
        .join(file.id.to_string())
        .join(name)
}

/// The file's local path, fetching it first when it is not kept yet.
async fn fetch(bot: Uuid, file: &Attachment) -> Result<PathBuf, String> {
    let path = cached(file);
    if path.exists() {
        return Ok(path);
    }
    let client = session::server().client.clone();
    let bytes = client
        .attachment(bot, file.id)
        .await
        .map_err(|err| err.to_string())?;
    let dir = path.parent().expect("a cached file has a folder");
    std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;
    // Written beside it first, so a file cut short is never taken as kept.
    let partial = dir.join(".partial");
    std::fs::write(&partial, &bytes).map_err(|err| err.to_string())?;
    std::fs::rename(&partial, &path).map_err(|err| err.to_string())?;
    Ok(path)
}

fn open(bot: Uuid, file: Attachment) {
    spawn(async move {
        if let Ok(path) = fetch(bot, &file).await {
            let _ = share(ShareRequest::new().file(path)).await;
        }
    });
}

/// A byte count as people read file sizes: 840 B, 6.2 kB, 3.1 MB.
pub fn size_label(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{:.1} kB", bytes as f64 / 1e3),
        _ => format!("{:.1} MB", bytes as f64 / 1e6),
    }
}

/// The files of one message, aligned to its side of the chat.
pub fn strip(bot: Uuid, files: Vec<Attachment>, trailing: bool) -> impl IntoElement {
    let row = view().flex_row().flex_wrap().gap_2();
    let row = if trailing {
        row.justify_end()
    } else {
        row.justify_start()
    };
    row.children(files.into_iter().map(move |file| {
        if is_image(&file.media_type) {
            thumbnail(bot, file).into_element()
        } else {
            chip(file.name.clone(), size_label(file.size))
                .role(AccessibilityRole::Button)
                .on_press(move || open(bot, file.clone()))
                .into_element()
        }
    }))
}

fn thumbnail(bot: Uuid, file: Attachment) -> impl IntoElement {
    let path = signal(None::<String>);
    {
        let file = file.clone();
        spawn(async move {
            if let Ok(local) = fetch(bot, &file).await {
                path.set(Some(local.to_string_lossy().into_owned()));
            }
        });
    }
    let name = file.name.clone();
    view()
        .size(px(180.0))
        .rounded(px(14.0))
        .border_curve(BorderCurve::Continuous)
        .overflow_hidden()
        .bg(colors::fill())
        .role(AccessibilityRole::Button)
        .label(name)
        .active(|s| s.opacity(0.8))
        .on_press(move || open(bot, file.clone()))
        .child(dynamic(move || {
            path.get().map(|path| {
                image(path)
                    .size_full()
                    .object_fit(nsh_core::ResizeMode::Cover)
            })
        }))
}

/// A file as a rounded chip with its name and a detail line.
pub fn chip(name: String, detail: String) -> View {
    view()
        .flex_row()
        .items_center()
        .gap_2()
        .max_w(px(260.0))
        .px_3()
        .py_2()
        .rounded(px(14.0))
        .border_curve(BorderCurve::Continuous)
        .bg(colors::fill())
        .active(|s| s.opacity(0.7))
        .label(format!("{name}, {detail}"))
        .child(text("📄").text_size(px(22.0)))
        .child(
            view()
                .flex_shrink(1.0)
                .child(
                    text(name)
                        .number_of_lines(1)
                        .text_size(px(15.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(colors::label()),
                )
                .child(
                    text(detail)
                        .text_size(px(12.0))
                        .text_color(colors::secondary()),
                ),
        )
}
