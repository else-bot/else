//! The directories a capture stages its set in and publishes it from.

use std::path::{Path, PathBuf};

use crate::backup::{BackupError, COMPLETE, Completion};

fn publish_completion(set_dir: &Path, completion: &Completion) -> Result<(), BackupError> {
    use std::io::Write;
    let path = set_dir.join(COMPLETE);
    let fail = |e| BackupError::io(path.display(), e);
    let mut file = crate::private_file::staged(set_dir, &path).map_err(fail)?;
    let bytes = serde_json::to_vec_pretty(completion).expect("serializable");
    file.write_all(&bytes)
        .and_then(|()| file.as_file().sync_all())
        .map_err(fail)?;
    file.persist_noclobber(&path).map_err(|e| fail(e.error))?;
    crate::backup::sync_dir(set_dir)
}

/// A staging directory this attempt created and marked as its own. Dropping
/// it removes the directory, so a failed or aborted capture (a shutdown
/// that outlives its grace) leaves no plaintext behind; after
/// [`Self::move_into`] nothing is left at the staging path.
pub(super) struct StagedSet(pub(super) PathBuf);

impl Drop for StagedSet {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A set directory this attempt renamed into place, not yet complete.
pub(super) struct OwnedFinalSet(PathBuf);

impl StagedSet {
    pub(super) fn create(dir: &Path, set_id: &str) -> Result<Self, BackupError> {
        let staging = crate::backup::staging_path(dir, set_id);
        crate::backup::private_dir(&staging)?;
        let staged = Self(staging);
        crate::backup::mark_owned(&staged.0)?;
        Ok(staged)
    }

    /// A failed rename never touches the destination: another set may own it.
    pub(super) fn move_into(self, dir: &Path, set_id: &str) -> Result<OwnedFinalSet, BackupError> {
        let final_dir = dir.join(set_id);
        crate::backup::rename_noreplace(&self.0, &final_dir)?;
        let owned = OwnedFinalSet(final_dir);
        match crate::backup::sync_dir(dir) {
            Ok(()) => Ok(owned),
            Err(err) => Err(owned.discard(err)),
        }
    }
}

impl OwnedFinalSet {
    fn discard(self, err: BackupError) -> BackupError {
        let _ = std::fs::remove_dir_all(&self.0);
        err
    }

    pub(super) fn publish(self, completion: &Completion) -> Result<PathBuf, BackupError> {
        if crate::faults::hit("backup.before_complete") {
            return Err(BackupError::Interrupted);
        }
        match publish_completion(&self.0, completion) {
            Ok(()) => Ok(self.0),
            Err(err) => Err(self.discard(err)),
        }
    }
}
