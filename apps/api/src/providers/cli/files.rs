//! Private files in a bot's provider home.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use uuid::Uuid;

/// Environment variable carrying the turn's task grant; provider
/// configurations reference it, so the grant is never written to disk.
pub const GRANT_ENV: &str = "ELSE_TASK_GRANT";

/// Replace `dir/name` atomically with a private (0600) file. The temporary
/// file is created exclusively without following links and synced before
/// the rename, so a planted symlink is never written through and a crash
/// never leaves a partial file under `name`.
pub fn replace_private(dir: &Path, name: &str, contents: &[u8]) -> std::io::Result<()> {
    if !fs::symlink_metadata(dir)?.is_dir() {
        return Err(std::io::Error::other("provider home is not a directory"));
    }
    let temp = dir.join(format!(".{name}.{}.tmp", Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temp, dir.join(name)).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

/// Point `dir/name` at the host login `source`, replacing whatever is there
/// (an earlier link, or a placeholder file). The login is never copied, so
/// token refreshes write through to the one file.
pub fn link_login(dir: &Path, name: &str, source: &Path) -> std::io::Result<()> {
    let temp = dir.join(format!(".{name}.{}.link", Uuid::new_v4()));
    std::os::unix::fs::symlink(source, &temp)?;
    fs::rename(&temp, dir.join(name)).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}
