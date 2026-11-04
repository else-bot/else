//! Process lifetime file locks, scoped to runtime storage and the database.
use sqlx::SqlitePool;
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another else.bot API is already running with data directory {0}")]
    StorageInUse(String),
    #[error("another else.bot API is already using this database")]
    DatabaseInUse,
    #[error("runtime lock {path}: {error}")]
    Io { path: String, error: std::io::Error },
}

pub struct RuntimeLock {
    _files: Vec<File>,
}

impl Drop for RuntimeLock {
    fn drop(&mut self) {
        // A concurrent process spawn may briefly inherit these descriptors.
        // Unlock explicitly so their copies cannot delay a normal handoff.
        for file in &self._files {
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
        .map_err(|error| LockError::Io {
            path: path.display().to_string(),
            error,
        })?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(in_use),
        Err(TryLockError::Error(error)) => Err(LockError::Io {
            path: path.display().to_string(),
            error,
        }),
    }
}
pub async fn acquire(data_dir: &Path, pool: &SqlitePool) -> Result<RuntimeLock, LockError> {
    let file = lock(
        &data_dir.join("api.lock"),
        LockError::StorageInUse(data_dir.display().to_string()),
    )?;
    let path = pool.connect_options().get_filename().to_owned();
    let mut files = vec![file];
    if path != Path::new(":memory:") {
        let canonical = std::fs::canonicalize(&path).map_err(|error| LockError::Io {
            path: path.display().to_string(),
            error,
        })?;
        let db_lock = PathBuf::from(format!("{}.api.lock", canonical.display()));
        files.push(lock(&db_lock, LockError::DatabaseInUse)?);
    }
    Ok(RuntimeLock { _files: files })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_the_guard_releases_locks_even_with_duplicated_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.lock");
        let guard = RuntimeLock {
            _files: vec![lock(&path, LockError::DatabaseInUse).unwrap()],
        };
        let _inherited = guard._files[0].try_clone().unwrap();
        drop(guard);
        lock(&path, LockError::DatabaseInUse).unwrap();
    }
}
