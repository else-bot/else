//! Owner-only files replaced in one step, so a reader or a crash sees the
//! old contents or the new ones, never a partial write.

use std::io::Write as _;
use std::path::Path;

/// Replace `path` with a new owner-only (0600) file holding `bytes`. The
/// file is written beside `path` under a fresh name, synced, renamed over
/// `path`, and the directory synced. A failed write leaves the old file and
/// removes the temporary one.
pub fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let mut file = staged(dir, path)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    std::fs::File::open(dir)?.sync_all()
}

/// A new owner-only temporary file in `dir`, named after `path` so a file a
/// crash leaves behind is recognizable.
pub fn staged(dir: &Path, path: &Path) -> std::io::Result<tempfile::NamedTempFile> {
    let name = path
        .file_name()
        .map_or_else(Default::default, |n| n.to_string_lossy());
    tempfile::Builder::new()
        .prefix(&format!(".{name}."))
        .suffix(".tmp")
        .tempfile_in(dir)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn a_replacement_is_private_and_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "old").unwrap();
        super::replace(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_failed_replacement_keeps_the_old_file_and_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("inside"), "").unwrap();
        assert!(super::replace(&path, b"new").is_err());
        assert!(path.join("inside").exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
