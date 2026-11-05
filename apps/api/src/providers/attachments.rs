//! Owner attachments staged in a bot's project directory, where its
//! provider runs start and its computer mounts `~/work`, so the bot can open
//! them with its own tools. SQLite keeps the authoritative bytes; a staged
//! copy is rewritten whenever it differs.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::app::messages::Attachment;
use crate::app::providers::InputFile;

/// Largest image given to Claude inline; larger ones stay on disk only,
/// where its Read tool can still open them.
pub const MAX_INLINE_IMAGE_BYTES: usize = 3 * 1024 * 1024 + 768 * 1024;

/// Where a file is staged, relative to the project directory. Names are
/// validated as one path component at upload.
pub fn relative_path(file: &Attachment) -> PathBuf {
    Path::new(".attachments")
        .join(file.id.to_string())
        .join(&file.name)
}

/// The image types both providers accept natively.
pub fn is_image(file: &Attachment) -> bool {
    matches!(
        file.media_type.as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

/// Write every file under `project`, skipping copies already identical.
pub async fn stage(project: &Path, files: &[InputFile]) -> std::io::Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let project = project.to_owned();
    let files = files.to_vec();
    tokio::task::spawn_blocking(move || files.iter().try_for_each(|file| stage_one(&project, file)))
        .await
        .map_err(std::io::Error::other)?
}

fn stage_one(project: &Path, file: &InputFile) -> std::io::Result<()> {
    let path = project.join(relative_path(&file.attachment));
    let dir = path.parent().expect("staged path has a parent");
    let same = fs::symlink_metadata(&path)
        .is_ok_and(|meta| meta.is_file() && meta.len() == file.data.len() as u64)
        && fs::read(&path).is_ok_and(|data| *data == *file.data);
    if same {
        return Ok(());
    }
    fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".{}.partial", Uuid::new_v4()));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .and_then(|mut out| {
            out.write_all(&file.data)?;
            out.sync_all()
        })
        .and_then(|()| fs::rename(&temp, &path));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, data: &[u8]) -> InputFile {
        InputFile {
            attachment: Attachment {
                id: Uuid::new_v4(),
                name: name.into(),
                media_type: "text/plain".into(),
                size: data.len() as u64,
            },
            data: data.into(),
        }
    }

    #[tokio::test]
    async fn staging_writes_and_repairs_copies() {
        let project = tempfile::tempdir().unwrap();
        let notes = file("notes.txt", b"hello");
        stage(project.path(), std::slice::from_ref(&notes))
            .await
            .unwrap();
        let path = project.path().join(relative_path(&notes.attachment));
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        fs::write(&path, b"edited").unwrap();
        stage(project.path(), std::slice::from_ref(&notes))
            .await
            .unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            b"hello",
            "a changed copy is restored"
        );
        let left: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(left.len(), 1, "no partial file is left behind");
    }
}
