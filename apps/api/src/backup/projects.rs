//! Bot project directories in backup sets.
//!
//! Each active bot's project directory (`<ELSE_BOTS_DIR>/<bot id>`, where
//! its provider runs start and which its computer mounts at `~/work`) is a
//! zstd-compressed tar, sealed like every artifact as
//! `project-<bot>.tar.zst.age`, so a decrypted set can also be unpacked with
//! standard tools (`zstd -d --long=27 | tar x`). Sets of schema 2 hold a plain
//! tar, `project-<bot>.tar.age`, and still restore. It is captured at the
//! set's checkpoint, while no turn runs and every computer is stopped.
//!
//! Regular files, directories, and symbolic links (never followed) are
//! archived with their modes and modification times; sockets, FIFOs, and
//! devices are not data and are skipped. A file with several names inside
//! the directory (package managers hard-link their stores) is stored once
//! and its other names as hard links, so it restores as one file again. An
//! entry that cannot be read is left out and reported, and the bot's
//! project is then `partial`.
//! Unpacking accepts only those kinds, refuses absolute paths, `..`, hard
//! links to anything but a file unpacked earlier, and anything that would
//! land outside the destination (through a link unpacked earlier), and
//! never overwrites.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::BackupError;

/// zstd level 3 with a 128 MiB long-distance window: a workspace's second
/// checkout or copied `node_modules` sits far behind the first in the tar.
const LEVEL: i32 = 3;
const WINDOW_LOG: u32 = 27;

/// How a set stores project directories, fixed by its schema version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Schema 2: a plain tar holding every hard link as a copy.
    Tar,
    /// Schema 3: a zstd-compressed tar that keeps hard links.
    TarZstd,
}

impl Format {
    pub fn of(schema_version: u32) -> Self {
        if schema_version < 3 {
            Self::Tar
        } else {
            Self::TarZstd
        }
    }

    /// The artifact name of a bot's project.
    pub fn object(self, bot: uuid::Uuid) -> String {
        match self {
            Self::Tar => format!("project-{bot}.tar.age"),
            Self::TarZstd => format!("project-{bot}.tar.zst.age"),
        }
    }
}

type Builder = tar::Builder<zstd::stream::write::Encoder<'static, BufWriter<File>>>;

macro_rules! tune {
    ($encoder:expr) => {{
        $encoder.long_distance_matching(true)?;
        $encoder.window_log(WINDOW_LOG)?;
        let workers = std::thread::available_parallelism().map_or(1, |n| n.get().min(4) as u32);
        $encoder.multithread(workers)
    }};
}

/// Compress `tar` as [`Format::TarZstd`] while it is read.
pub fn compress<R: Read>(
    tar: R,
) -> std::io::Result<zstd::stream::read::Encoder<'static, BufReader<R>>> {
    let mut encoder = zstd::stream::read::Encoder::new(tar, LEVEL)?;
    tune!(encoder)?;
    Ok(encoder)
}

/// The tar inside a [`Format::TarZstd`] archive.
pub fn decompress<R: Read>(
    archive: R,
) -> std::io::Result<zstd::stream::read::Decoder<'static, BufReader<R>>> {
    let mut decoder = zstd::stream::read::Decoder::new(archive)?;
    decoder.window_log_max(WINDOW_LOG)?;
    Ok(decoder)
}

/// Archive `dir` into `target` (created new, mode 0600) as
/// [`Format::TarZstd`]. Returns the paths, relative to `dir`, of entries that
/// could not be read and were left out.
pub fn archive(dir: &Path, target: &Path) -> Result<Vec<PathBuf>, BackupError> {
    let fail = |e: std::io::Error| BackupError::io(target.display(), e);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(target)
        .map_err(fail)?;
    let mut encoder =
        zstd::stream::write::Encoder::new(BufWriter::new(file), LEVEL).map_err(fail)?;
    (|| tune!(encoder))().map_err(fail)?;
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    let mut unreadable = Vec::new();
    let mut linked = HashMap::new();
    add_dir(
        &mut builder,
        dir,
        Path::new(""),
        &mut linked,
        &mut unreadable,
    )?;
    let mut writer = builder.into_inner().map_err(fail)?.finish().map_err(fail)?;
    writer.flush().map_err(fail)?;
    writer.get_ref().sync_all().map_err(fail)?;
    Ok(unreadable)
}

fn add_dir(
    builder: &mut Builder,
    root: &Path,
    rel: &Path,
    linked: &mut HashMap<(u64, u64), PathBuf>,
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
            add_dir(builder, root, &rel, linked, unreadable)?;
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
            let inode = (meta.dev(), meta.ino());
            if meta.nlink() > 1
                && let Some(first) = linked.get(&inode)
            {
                let mut header = tar::Header::new_gnu();
                header.set_metadata(&meta);
                header.set_entry_type(tar::EntryType::Link);
                header.set_size(0);
                builder
                    .append_link(&mut header, &rel, first)
                    .map_err(fail)?;
                continue;
            }
            let mut header = tar::Header::new_gnu();
            header.set_metadata(&meta);
            header.set_entry_type(tar::EntryType::Regular);
            builder
                .append_data(&mut header, &rel, &mut file)
                .map_err(fail)?;
            if meta.nlink() > 1 {
                linked.insert(inode, rel);
            }
        }
    }
    Ok(())
}

/// Unpack the archive at `archive`, stored as `format`, into `dir`, which
/// must not exist yet (created with mode 0700).
pub fn unpack(archive: &Path, dir: &Path, format: Format) -> Result<(), BackupError> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(dir)
        .map_err(|e| BackupError::io(dir.display(), e))?;
    let file = File::open(archive).map_err(|e| BackupError::io(archive.display(), e))?;
    let reader: Box<dyn Read> = match format {
        Format::Tar => Box::new(BufReader::new(file)),
        Format::TarZstd => {
            Box::new(decompress(file).map_err(|e| BackupError::io(archive.display(), e))?)
        }
    };
    let mut tar = tar::Archive::new(reader);
    tar.set_preserve_permissions(true);
    tar.set_preserve_mtime(true);
    tar.set_overwrite(false);
    tar.set_unpack_xattrs(false);
    let invalid = |why: String| BackupError::Invalid(format!("project archive: {why}"));
    let relative = |path: &Path| {
        !path.is_absolute()
            && path
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
    };
    let mut files = HashSet::new();
    for entry in tar
        .entries()
        .map_err(|e| BackupError::io(archive.display(), e))?
    {
        let mut entry = entry.map_err(|e| BackupError::io(archive.display(), e))?;
        let path = entry
            .path()
            .map_err(|e| invalid(e.to_string()))?
            .into_owned();
        if !relative(&path) {
            return Err(invalid(format!(
                "{} is not a relative path",
                path.display()
            )));
        }
        let kind = entry.header().entry_type();
        if !matches!(
            kind,
            tar::EntryType::Regular
                | tar::EntryType::Directory
                | tar::EntryType::Symlink
                | tar::EntryType::Link
        ) {
            return Err(invalid(format!(
                "{} is not a file, directory, or link",
                path.display()
            )));
        }
        if kind == tar::EntryType::Link {
            let target = entry
                .link_name()
                .map_err(|e| invalid(e.to_string()))?
                .map(|t| t.into_owned());
            if !target.is_some_and(|t| relative(&t) && files.contains(&t)) {
                return Err(invalid(format!(
                    "{} links to something other than a file unpacked before it",
                    path.display()
                )));
            }
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
        if kind == tar::EntryType::Regular {
            files.insert(path);
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

        let tar = dir.path().join("p.tar.zst");
        assert!(archive(&src, &tar).unwrap().is_empty());
        let out = dir.path().join("out");
        unpack(&tar, &out, Format::TarZstd).unwrap();
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
        let bytes = plain_tar(&tar);
        assert!(bytes.windows(5).any(|w| w == b"hello"));
        assert!(!bytes.windows(7).any(|w| w == b"OUTSIDE"));
        assert!(fs::symlink_metadata(out.join("sock")).is_err());
        assert!(unpack(&tar, &out, Format::TarZstd).is_err());
    }

    fn plain_tar(archive: &Path) -> Vec<u8> {
        let mut decoder = zstd::stream::read::Decoder::new(File::open(archive).unwrap()).unwrap();
        decoder.window_log_max(WINDOW_LOG).unwrap();
        let mut bytes = Vec::new();
        decoder.read_to_end(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn hard_links_are_stored_once_and_restored_as_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("store")).unwrap();
        fs::create_dir_all(src.join("app/node_modules")).unwrap();
        let body = "x".repeat(64 * 1024);
        fs::write(src.join("store/lib.js"), &body).unwrap();
        fs::hard_link(
            src.join("store/lib.js"),
            src.join("app/node_modules/lib.js"),
        )
        .unwrap();
        fs::hard_link(src.join("store/lib.js"), src.join("copy.js")).unwrap();
        let outside = dir.path().join("cache.js");
        fs::write(&outside, "shared with a file outside").unwrap();
        fs::hard_link(&outside, src.join("cached.js")).unwrap();

        let tar = dir.path().join("p.tar.zst");
        assert!(archive(&src, &tar).unwrap().is_empty());
        let plain = plain_tar(&tar);
        let mut stored = 0;
        for entry in tar::Archive::new(plain.as_slice()).entries().unwrap() {
            let entry = entry.unwrap();
            stored += entry.header().size().unwrap();
        }
        assert_eq!(
            stored,
            body.len() as u64 + "shared with a file outside".len() as u64
        );

        let out = dir.path().join("out");
        unpack(&tar, &out, Format::TarZstd).unwrap();
        let ino = |p: &str| fs::metadata(out.join(p)).unwrap().ino();
        assert_eq!(ino("store/lib.js"), ino("app/node_modules/lib.js"));
        assert_eq!(ino("store/lib.js"), ino("copy.js"));
        fs::write(out.join("copy.js"), "edited").unwrap();
        assert_eq!(
            fs::read_to_string(out.join("app/node_modules/lib.js")).unwrap(),
            "edited"
        );
        assert_eq!(
            fs::read_to_string(out.join("cached.js")).unwrap(),
            "shared with a file outside"
        );
        assert_eq!(fs::metadata(out.join("cached.js")).unwrap().nlink(), 1);
    }

    #[test]
    fn schema_2_plain_tars_still_unpack() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("sub/notes.md"), "legacy").unwrap();
        let tar = dir.path().join("p.tar");
        let mut builder = tar::Builder::new(File::create(&tar).unwrap());
        builder.append_dir("sub", src.join("sub")).unwrap();
        builder
            .append_path_with_name(src.join("sub/notes.md"), "sub/notes.md")
            .unwrap();
        builder.finish().unwrap();
        drop(builder);
        let out = dir.path().join("out");
        assert!(unpack(&tar, &dir.path().join("as-zstd"), Format::TarZstd).is_err());
        unpack(&tar, &out, Format::Tar).unwrap();
        assert_eq!(
            fs::read_to_string(out.join("sub/notes.md")).unwrap(),
            "legacy"
        );
    }

    #[test]
    fn unreadable_entries_are_reported_and_left_out() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("ok.txt"), "ok").unwrap();
        fs::write(src.join("locked.txt"), "no").unwrap();
        fs::set_permissions(src.join("locked.txt"), fs::Permissions::from_mode(0o000)).unwrap();
        let tar = dir.path().join("p.tar.zst");
        let unreadable = archive(&src, &tar).unwrap();
        assert_eq!(unreadable, vec![PathBuf::from("locked.txt")]);
        let out = dir.path().join("out");
        unpack(&tar, &out, Format::TarZstd).unwrap();
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
            vec![("hard", Link, "../escape.txt")],
            vec![("hard", Link, "not-unpacked")],
            vec![("dir", Directory, ""), ("hard", Link, "dir")],
            vec![("soft", Symlink, "/etc/passwd"), ("hard", Link, "soft")],
            vec![("fifo", Fifo, "")],
        ] {
            let dir = hostile(&entries);
            let out = dir.path().join("out-dir");
            assert!(
                unpack(&dir.path().join("h.tar"), &out, Format::Tar).is_err(),
                "{entries:?}"
            );
            assert!(!dir.path().join("escape.txt").exists());
            assert!(!Path::new("/tmp/else-abs-escape.txt").exists());
            assert!(!Path::new("/tmp/else-link-escape.txt").exists());
        }
    }
}
