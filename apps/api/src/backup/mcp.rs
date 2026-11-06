use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path};

use super::BackupError;

pub const OBJECT: &str = "mcp.tar.age";
const FILES: &[&str] = &["manifest.json", "SKILL.md", "README.md", "config.json"];

fn invalid(why: impl std::fmt::Display) -> BackupError {
    BackupError::Invalid(format!("shared MCP archive: {why}"))
}

fn credential_file(name: &str) -> bool {
    name.strip_prefix("oauth-")
        .and_then(|s| s.strip_suffix(".json"))
        .and_then(|s| uuid::Uuid::parse_str(s).ok().map(|id| id.to_string() == s))
        == Some(true)
}

fn package_file(name: &str) -> bool {
    FILES.contains(&name) || credential_file(name)
}

fn valid_id(id: &str) -> bool {
    crate::mcp::PackageId::try_from(id.to_owned()).is_ok()
}

fn open_at(dir: &File, name: &str, directory: bool) -> Result<File, BackupError> {
    let name = std::ffi::CString::new(name).map_err(invalid)?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: the borrowed directory descriptor and NUL-terminated name are valid.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(invalid(std::io::Error::last_os_error()));
    }
    // SAFETY: openat returned a new descriptor owned exclusively by this File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn archive(dir: &Path, target: &Path, limit: u64) -> Result<(), BackupError> {
    let mut packages = Vec::new();
    let mut library = None;
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {
            library = Some(
                fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
                    .open(dir)
                    .map_err(invalid)?,
            );
            for entry in fs::read_dir(dir).map_err(|e| BackupError::io(dir.display(), e))? {
                let entry = entry.map_err(|e| BackupError::io(dir.display(), e))?;
                let id = entry.file_name();
                let id = id
                    .to_str()
                    .ok_or_else(|| invalid("non-UTF-8 package name"))?;
                if id.starts_with('.') {
                    continue;
                }
                if !valid_id(id) {
                    continue;
                }
                if !entry.file_type().map_err(invalid)?.is_dir() {
                    return Err(invalid(format!("invalid package {id}")));
                }
                packages.push(id.to_owned());
            }
        }
        Ok(_) => return Err(invalid("the library is not a directory")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(BackupError::io(dir.display(), e)),
    }
    packages.sort();
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(target)
        .map_err(|e| BackupError::io(target.display(), e))?;
    let mut builder = tar::Builder::new(BufWriter::new(file));
    let mut bytes = 1024u64;
    for id in packages {
        let package_dir = open_at(library.as_ref().expect("existing library"), &id, true)?;
        let package = dir.join(&id);
        let mut found = BTreeSet::new();
        for entry in fs::read_dir(&package).map_err(|e| BackupError::io(package.display(), e))? {
            let entry = entry.map_err(|e| BackupError::io(package.display(), e))?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| invalid("non-UTF-8 file name"))?;
            if name.starts_with('.') {
                continue;
            }
            if !package_file(name) || !entry.file_type().map_err(invalid)?.is_file() {
                return Err(invalid(format!("unexpected entry {id}/{name}")));
            }
            found.insert(name.to_owned());
        }
        if FILES.iter().any(|name| !found.contains(*name)) {
            return Err(invalid(format!("{id} is missing package files")));
        }
        bytes = bytes.saturating_add(512);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_mode(0o700);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_data(&mut header, &id, std::io::empty())
            .map_err(invalid)?;
        for name in &found {
            let mut file = open_at(&package_dir, name, false)?;
            let meta = file.metadata().map_err(invalid)?;
            if !meta.is_file() || (credential_file(name) && meta.len() > 64 * 1024) {
                return Err(invalid(format!("{id}/{name} is not a valid package file")));
            }
            bytes = bytes
                .saturating_add(512)
                .saturating_add(meta.len().div_ceil(512).saturating_mul(512));
            if bytes > limit {
                return Err(BackupError::TooLarge(limit));
            }
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o600);
            header.set_size(meta.len());
            header.set_cksum();
            builder
                .append_data(&mut header, Path::new(&id).join(name), &mut file)
                .map_err(invalid)?;
        }
    }
    if bytes > limit {
        return Err(BackupError::TooLarge(limit));
    }
    let mut writer = builder.into_inner().map_err(invalid)?;
    writer.flush().map_err(invalid)?;
    writer.get_ref().sync_all().map_err(invalid)?;
    Ok(())
}

pub fn unpack(archive: &Path, dir: &Path) -> Result<(), BackupError> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(dir)
        .map_err(|e| BackupError::io(dir.display(), e))?;
    let result = unpack_into(archive, dir);
    if result.is_err() {
        let _ = fs::remove_dir_all(dir);
    }
    result
}

fn unpack_into(archive: &Path, dir: &Path) -> Result<(), BackupError> {
    let file = File::open(archive).map_err(|e| BackupError::io(archive.display(), e))?;
    let mut remaining = file.metadata().map_err(invalid)?.len();
    let mut tar = tar::Archive::new(BufReader::new(file));
    let mut packages = BTreeMap::<String, BTreeSet<String>>::new();
    for entry in tar.entries().map_err(invalid)? {
        let mut entry = entry.map_err(invalid)?;
        let path = entry.path().map_err(invalid)?.into_owned();
        let names: Option<Vec<_>> = path
            .components()
            .map(|part| match part {
                Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .collect();
        let Some(names) = names else {
            return Err(invalid("paths must be relative UTF-8 package paths"));
        };
        let kind = entry.header().entry_type();
        match names.as_slice() {
            [id] if valid_id(id) && kind == tar::EntryType::Directory => {
                if packages.insert((*id).to_owned(), BTreeSet::new()).is_some() {
                    return Err(invalid("duplicate package directory"));
                }
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(dir.join(id))
                    .map_err(invalid)?;
            }
            [id, name] if valid_id(id) && package_file(name) && kind == tar::EntryType::Regular => {
                let bytes = entry.size();
                if bytes > remaining || (credential_file(name) && bytes > 64 * 1024) {
                    return Err(invalid("unpacked data exceeds the archive size"));
                }
                remaining -= bytes;
                let files = packages
                    .get_mut(*id)
                    .ok_or_else(|| invalid("package directory must precede its files"))?;
                if !files.insert((*name).to_owned()) {
                    return Err(invalid("duplicate package file"));
                }
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(dir.join(&path))
                    .map_err(invalid)?;
                std::io::copy(&mut entry, &mut file).map_err(invalid)?;
                file.sync_all().map_err(invalid)?;
            }
            _ => return Err(invalid(format!("unexpected entry {}", path.display()))),
        }
    }
    if packages
        .values()
        .any(|files| FILES.iter().any(|name| !files.contains(*name)))
    {
        return Err(invalid("a package is missing files"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn package(root: &Path) {
        fs::create_dir_all(root.join("t3-code")).unwrap();
        for name in FILES {
            fs::write(root.join("t3-code").join(name), format!("saved {name}")).unwrap();
        }
    }

    #[test]
    fn packages_round_trip_privately_without_installation_staging() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("mcp");
        package(&src);
        let credential = format!("oauth-{}.json", uuid::Uuid::new_v4());
        fs::write(src.join("t3-code").join(&credential), "encrypted tokens").unwrap();
        fs::write(src.join("t3-code/.DS_Store"), "decoration").unwrap();
        fs::write(src.join("t3-code/.README.md.swp"), "editor state").unwrap();
        fs::create_dir(src.join("Stray folder")).unwrap();
        fs::create_dir(src.join(".install-pending")).unwrap();
        fs::write(src.join(".install-pending/secret"), "unpublished").unwrap();
        fs::create_dir(src.join(".removed-old")).unwrap();
        let archive_path = temp.path().join("mcp.tar");
        archive(&src, &archive_path, 1 << 20).unwrap();
        let target = temp.path().join("out");
        unpack(&archive_path, &target).unwrap();
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
        for name in FILES {
            let path = target.join("t3-code").join(name);
            assert_eq!(fs::read_to_string(&path).unwrap(), format!("saved {name}"));
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let credential = target.join("t3-code").join(credential);
        assert_eq!(fs::read_to_string(&credential).unwrap(), "encrypted tokens");
        assert_eq!(
            fs::metadata(&credential).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!target.join("t3-code/.DS_Store").exists());
        assert!(unpack(&archive_path, &target).is_err(), "never overwrite");
    }

    #[test]
    fn missing_library_round_trips_as_empty_and_size_is_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("missing");
        let archive_path = temp.path().join("empty.tar");
        archive(&source, &archive_path, 1024).unwrap();
        let target = temp.path().join("out");
        unpack(&archive_path, &target).unwrap();
        assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        package(&source);
        assert!(matches!(
            archive(&source, &temp.path().join("small.tar"), 1024),
            Err(BackupError::TooLarge(1024))
        ));
    }

    #[test]
    fn missing_files_and_symlinks_fail_capture() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("mcp");
        package(&source);
        let config = source.join("t3-code/config.json");
        fs::remove_file(&config).unwrap();
        assert!(archive(&source, &temp.path().join("missing.tar"), 1 << 20).is_err());
        std::os::unix::fs::symlink("/etc/passwd", &config).unwrap();
        assert!(archive(&source, &temp.path().join("link.tar"), 1 << 20).is_err());
        let link = temp.path().join("linked-library");
        std::os::unix::fs::symlink(&source, &link).unwrap();
        assert!(archive(&link, &temp.path().join("root-link.tar"), 1 << 20).is_err());
    }

    #[test]
    fn hostile_archive_paths_links_and_partial_packages_are_refused() {
        for (path, kind) in [
            ("../escape", tar::EntryType::Regular),
            ("/tmp/mcp-escape", tar::EntryType::Regular),
            ("t3-code", tar::EntryType::Symlink),
            ("t3-code", tar::EntryType::Link),
            ("t3-code", tar::EntryType::Directory),
            ("t3-code/README.md", tar::EntryType::Regular),
            ("t3-code/config.json/escape", tar::EntryType::Regular),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let archive_path = temp.path().join("hostile.tar");
            let mut tar = tar::Builder::new(File::create(&archive_path).unwrap());
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_mode(0o777);
            header.set_size(0);
            if matches!(kind, tar::EntryType::Link | tar::EntryType::Symlink) {
                header.set_link_name("/tmp").unwrap();
            }
            header.as_gnu_mut().unwrap().name[..path.len()].copy_from_slice(path.as_bytes());
            header.set_cksum();
            tar.append(&header, std::io::empty()).unwrap();
            tar.finish().unwrap();
            let target = temp.path().join("out");
            assert!(unpack(&archive_path, &target).is_err(), "{path}");
            assert!(!target.exists(), "incomplete library removed");
            assert!(!temp.path().join("escape").exists());
        }
    }
}
