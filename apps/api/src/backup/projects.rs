//! Bot project directories in backup sets.
//!
//! Each active bot's project directory (`<ELSE_BOTS_DIR>/<bot id>`, where
//! its provider runs start and which its computer mounts at `~/work`) is a
//! plain tar, sealed like every artifact as `project-<bot>.tar.age`, so a
//! decrypted set can also be unpacked with standard tools. It is captured at
//! the set's checkpoint, while no turn runs and every computer is stopped.
//!
//! Regular files, directories, and symbolic links (never followed) are
//! archived with their modes and modification times; sockets, FIFOs, and
//! devices are not data and are skipped. An entry that cannot be read is
//! left out and reported, and the bot's project is then `partial`.
//! Unpacking accepts only those three kinds, refuses absolute paths, `..`,
//! and anything that would land outside the destination (through a link
//! unpacked earlier), and never overwrites.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::BackupError;

/// The artifact name of a bot's project.
pub fn object(bot: uuid::Uuid) -> String {
    format!("project-{bot}.tar.age")
}

/// Archive `dir` into `target` (created new, mode 0600). Returns the paths,
/// relative to `dir`, of entries that could not be read and were left out.
pub fn archive(dir: &Path, target: &Path) -> Result<Vec<PathBuf>, BackupError> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(target)
        .map_err(|e| BackupError::io(target.display(), e))?;
    let mut builder = tar::Builder::new(BufWriter::new(file));
    builder.follow_symlinks(false);
    let mut unreadable = Vec::new();
    add_dir(&mut builder, dir, Path::new(""), &mut unreadable)?;
    let mut writer = builder
        .into_inner()
        .map_err(|e| BackupError::io(target.display(), e))?;
    writer
        .flush()
        .map_err(|e| BackupError::io(target.display(), e))?;
    writer
        .get_ref()
        .sync_all()
        .map_err(|e| BackupError::io(target.display(), e))?;
    Ok(unreadable)
}

fn add_dir(
    builder: &mut tar::Builder<BufWriter<File>>,
    root: &Path,
    rel: &Path,
    unreadable: &mut Vec<PathBuf>,
) -> Result<(), BackupError> {
    let dir = root.join(rel);
    let mut entries: Vec<_> = match fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect(),
        Err(_) => {
            unreadable.push(rel.to_owned());
            return Ok(());
        }
    };
    entries.sort();
    for name in entries {
        let rel = rel.join(&name);
        let path = root.join(&rel);
        let Ok(meta) = fs::symlink_metadata(&path) else {
            unreadable.push(rel);
            continue;
        };
        let kind = meta.file_type();
        let fail = |e: std::io::Error| BackupError::io(path.display(), e);
        if kind.is_symlink() {
            let Ok(link) = fs::read_link(&path) else {
                unreadable.push(rel);
                continue;
            };
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&meta);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            builder
                .append_link(&mut header, &rel, &link)
                .map_err(fail)?;
        } else if kind.is_dir() {
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&meta);
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            builder
                .append_data(&mut header, &rel, std::io::empty())
                .map_err(fail)?;
            add_dir(builder, root, &rel, unreadable)?;
        } else if kind.is_file() {
            let Ok(mut file) = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
            else {
                unreadable.push(rel);
                continue;
            };
            let meta = file.metadata().map_err(fail)?;
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&meta);
            header.set_entry_type(tar::EntryType::Regular);
            builder
                .append_data(&mut header, &rel, &mut file)
                .map_err(fail)?;
        }
    }
    Ok(())
}

/// Unpack the archive at `archive` into `dir`, which must not exist yet
/// (created with mode 0700).
pub fn unpack(archive: &Path, dir: &Path) -> Result<(), BackupError> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(dir)
        .map_err(|e| BackupError::io(dir.display(), e))?;
    let file = File::open(archive).map_err(|e| BackupError::io(archive.display(), e))?;
    let mut tar = tar::Archive::new(std::io::BufReader::new(file));
    tar.set_preserve_permissions(true);
    tar.set_preserve_mtime(true);
    tar.set_overwrite(false);
    tar.set_unpack_xattrs(false);
    let invalid = |why: String| BackupError::Invalid(format!("project archive: {why}"));
    for entry in tar
        .entries()
        .map_err(|e| BackupError::io(archive.display(), e))?
    {
        let mut entry = entry.map_err(|e| BackupError::io(archive.display(), e))?;
        let path = entry
            .path()
            .map_err(|e| invalid(e.to_string()))?
            .into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(invalid(format!(
                "{} is not a relative path",
                path.display()
            )));
        }
        let kind = entry.header().entry_type();
        if !matches!(
            kind,
            tar::EntryType::Regular | tar::EntryType::Directory | tar::EntryType::Symlink
        ) {
            return Err(invalid(format!(
                "{} is not a file, directory, or link",
                path.display()
            )));
        }
        let inside = entry
            .unpack_in(dir)
            .map_err(|e| invalid(format!("{}: {e}", path.display())))?;
        if !inside {
            return Err(invalid(format!(
                "{} is outside the project",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_project_round_trips_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("sub/deep")).unwrap();
        fs::write(src.join("notes.md"), "hello").unwrap();
        fs::write(src.join("sub/deep/data.csv"), "a,b\n1,2\n").unwrap();
        fs::write(src.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(src.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        let outside = dir.path().join("secret.txt");
        fs::write(&outside, "OUTSIDE").unwrap();
        std::os::unix::fs::symlink(&outside, src.join("link")).unwrap();
        std::os::unix::fs::symlink("sub/deep", src.join("rel")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(src.join("sock")).unwrap();

        let tar = dir.path().join("p.tar");
        assert!(archive(&src, &tar).unwrap().is_empty());
        let out = dir.path().join("out");
        unpack(&tar, &out).unwrap();
        assert_eq!(fs::read_to_string(out.join("notes.md")).unwrap(), "hello");
        assert_eq!(
            fs::read_to_string(out.join("sub/deep/data.csv")).unwrap(),
            "a,b\n1,2\n"
        );
        let mode = fs::metadata(out.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(fs::read_link(out.join("link")).unwrap(), outside);
        assert_eq!(
            fs::read_link(out.join("rel")).unwrap(),
            Path::new("sub/deep")
        );
        let bytes = fs::read(&tar).unwrap();
        assert!(!bytes.windows(7).any(|w| w == b"OUTSIDE"));
        assert!(fs::symlink_metadata(out.join("sock")).is_err());
        assert!(unpack(&tar, &out).is_err());
    }

    #[test]
    fn unreadable_entries_are_reported_and_left_out() {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("ok.txt"), "ok").unwrap();
        fs::write(src.join("locked.txt"), "no").unwrap();
        fs::set_permissions(src.join("locked.txt"), fs::Permissions::from_mode(0o000)).unwrap();
        let tar = dir.path().join("p.tar");
        let unreadable = archive(&src, &tar).unwrap();
        assert_eq!(unreadable, vec![PathBuf::from("locked.txt")]);
        let out = dir.path().join("out");
        unpack(&tar, &out).unwrap();
        assert_eq!(fs::read_to_string(out.join("ok.txt")).unwrap(), "ok");
        assert!(!out.join("locked.txt").exists());
    }

    fn hostile(entries: &[(&str, tar::EntryType, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(File::create(dir.path().join("h.tar")).unwrap());
        for (path, kind, link) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_mode(0o644);
            header.set_size(if *kind == tar::EntryType::Regular {
                1
            } else {
                0
            });
            let name = header.as_gnu_mut().unwrap();
            name.name[..path.len()].copy_from_slice(path.as_bytes());
            if !link.is_empty() {
                header.set_link_name(link).unwrap();
            }
            header.set_cksum();
            let data: &[u8] = if *kind == tar::EntryType::Regular {
                b"x"
            } else {
                b""
            };
            builder.append(&header, data).unwrap();
        }
        builder.finish().unwrap();
        dir
    }

    #[test]
    fn hostile_archives_cannot_write_outside_the_project() {
        use tar::EntryType::*;
        for entries in [
            vec![("../escape.txt", Regular, "")],
            vec![("/tmp/else-abs-escape.txt", Regular, "")],
            vec![
                ("out", Symlink, "/tmp"),
                ("out/else-link-escape.txt", Regular, ""),
            ],
            vec![("hard", Link, "/etc/passwd")],
            vec![("fifo", Fifo, "")],
        ] {
            let dir = hostile(&entries);
            let out = dir.path().join("out-dir");
            assert!(
                unpack(&dir.path().join("h.tar"), &out).is_err(),
                "{entries:?}"
            );
            assert!(!dir.path().join("escape.txt").exists());
            assert!(!Path::new("/tmp/else-abs-escape.txt").exists());
            assert!(!Path::new("/tmp/else-link-escape.txt").exists());
        }
    }
}
