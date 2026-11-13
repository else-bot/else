use std::fs::{File, OpenOptions, TryLockError};
use std::net::SocketAddr;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::config::DatabaseUrl;
use crate::credentials::{CredentialError, ensure_private_dir};
use crate::db::DbError;

/// The listener manifest's name in the data directory.
pub const MANIFEST: &str = "api.json";

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another else.bot API is already running with data directory {0}")]
    StorageInUse(String),
    #[error("another else.bot API is already using this database")]
    DatabaseInUse,
    #[error(transparent)]
    Directory(#[from] CredentialError),
    #[error(transparent)]
    Database(#[from] DbError),
    #[error("runtime lock {path}: {error}")]
    Io { path: String, error: std::io::Error },
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> LockError + '_ {
    move |error| LockError::Io {
        path: path.display().to_string(),
        error,
    }
}

pub struct RuntimeLock {
    data_dir: PathBuf,
    files: Vec<File>,
}

impl Drop for RuntimeLock {
    fn drop(&mut self) {
        // A concurrent process spawn may briefly inherit these descriptors.
        // Unlock explicitly so their copies cannot delay a normal handoff.
        for file in &self.files {
            let _ = file.unlock();
        }
    }
}

fn lock(path: &Path, in_use: LockError) -> Result<File, LockError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(io_error(path))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(in_use),
        Err(TryLockError::Error(error)) => Err(io_error(path)(error)),
    }
}

fn canonical_database(url: &DatabaseUrl) -> Result<Option<PathBuf>, LockError> {
    let options = crate::db::options(url.expose())?;
    let path = options.get_filename();
    if path == Path::new(":memory:") {
        return Ok(None);
    }
    let parent = match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(parent) => {
            ensure_private_dir(parent)?;
            parent
        }
        None => Path::new("."),
    };
    let name = path.file_name().ok_or(DbError::InvalidUrl)?;
    let dir = std::fs::canonicalize(parent).map_err(io_error(parent))?;
    Ok(Some(dir.join(name)))
}

/// Take ownership of the data directory and the database before anything
/// reads credentials, opens SQLite, or changes either. A manifest left by a
/// process that held these locks earlier is stale and removed.
pub fn acquire(data_dir: &Path, database: &DatabaseUrl) -> Result<RuntimeLock, LockError> {
    acquire_with(data_dir, database, lock)
}

fn acquire_with(
    data_dir: &Path,
    database: &DatabaseUrl,
    lock: impl Fn(&Path, LockError) -> Result<File, LockError>,
) -> Result<RuntimeLock, LockError> {
    ensure_private_dir(data_dir)?;
    let mut held = RuntimeLock {
        data_dir: data_dir.to_owned(),
        files: vec![lock(
            &data_dir.join("api.lock"),
            LockError::StorageInUse(data_dir.display().to_string()),
        )?],
    };
    if let Some(path) = canonical_database(database)? {
        let db_lock = PathBuf::from(format!("{}.api.lock", path.display()));
        held.files.push(lock(&db_lock, LockError::DatabaseInUse)?);
    }
    let manifest = held.data_dir.join(MANIFEST);
    match std::fs::remove_file(&manifest) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(io_error(&manifest)(error));
        }
        _ => {}
    }
    Ok(held)
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    pub pid: u32,
    pub url: String,
}

/// `$ELSE_DATA_DIR/api.json` while this process serves HTTP; removed when
/// the guard drops.
pub struct Published<'a> {
    path: PathBuf,
    _lock: &'a RuntimeLock,
}

impl Drop for Published<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl RuntimeLock {
    /// Atomically publish the bound listener so launchers never parse logs.
    pub fn publish(&self, addr: SocketAddr) -> Result<Published<'_>, LockError> {
        let manifest = Manifest {
            pid: std::process::id(),
            url: format!("http://{addr}"),
        };
        let path = self.data_dir.join(MANIFEST);
        let body = serde_json::to_vec(&manifest).expect("manifest serializes");
        crate::private_file::replace(&path, &body).map_err(io_error(&path))?;
        Ok(Published { path, _lock: self })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(path: &Path) -> DatabaseUrl {
        DatabaseUrl::new(format!("sqlite://{}", path.display()))
    }

    #[test]
    fn dropping_the_guard_releases_locks_even_with_duplicated_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let database = url(&dir.path().join("else.sqlite3"));
        let guard = acquire(&data, &database).unwrap();
        let _inherited: Vec<File> = guard.files.iter().map(|f| f.try_clone().unwrap()).collect();
        drop(guard);
        acquire(&data, &database).unwrap();
    }

    #[test]
    fn a_refused_database_releases_the_data_directory_despite_an_inherited_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let busy = url(&dir.path().join("busy.sqlite3"));
        let _other = acquire(&dir.path().join("other"), &busy).unwrap();
        let inherited = std::cell::RefCell::new(Vec::new());
        let refused = acquire_with(&data, &busy, |path, in_use| {
            let file = lock(path, in_use)?;
            inherited.borrow_mut().push(file.try_clone().unwrap());
            Ok(file)
        });
        assert!(matches!(refused, Err(LockError::DatabaseInUse)));
        assert_eq!(inherited.borrow().len(), 1);
        acquire(&data, &url(&dir.path().join("else.sqlite3"))).unwrap();
    }

    #[test]
    fn database_aliases_from_other_data_directories_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("db")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("db"), dir.path().join("alias")).unwrap();
        let _held = acquire(
            &dir.path().join("a"),
            &url(&dir.path().join("db/else.sqlite3")),
        )
        .unwrap();
        for alias in ["alias/else.sqlite3", "db/../db/else.sqlite3"] {
            assert!(matches!(
                acquire(&dir.path().join("b"), &url(&dir.path().join(alias))),
                Err(LockError::DatabaseInUse)
            ));
        }
        assert!(matches!(
            acquire(
                &dir.path().join("a"),
                &url(&dir.path().join("other.sqlite3"))
            ),
            Err(LockError::StorageInUse(_))
        ));
        acquire(
            &dir.path().join("b"),
            &url(&dir.path().join("other.sqlite3")),
        )
        .unwrap();
    }

    #[test]
    fn the_manifest_names_the_bound_listener_until_dropped_and_stale_ones_are_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let database = url(&dir.path().join("else.sqlite3"));
        let held = acquire(&data, &database).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let published = held.publish(addr).unwrap();
        let read: Manifest =
            serde_json::from_slice(&std::fs::read(data.join(MANIFEST)).unwrap()).unwrap();
        assert_eq!(
            read,
            Manifest {
                pid: std::process::id(),
                url: format!("http://127.0.0.1:{}", addr.port()),
            }
        );
        drop(published);
        assert!(!data.join(MANIFEST).exists());

        std::mem::forget(held.publish(addr).unwrap());
        drop(held);
        assert!(data.join(MANIFEST).exists(), "a crash leaves the manifest");
        let _again = acquire(&data, &database).unwrap();
        assert!(!data.join(MANIFEST).exists());
    }
}
