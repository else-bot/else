//! The local chunk store.
//!
//! A published local set is `<set id>/complete.json` and
//! `<set id>/index.json.age`, the layout of a remote set (see [`super::chunks`]),
//! over one `chunks/<key id>/` directory that every set in the backup
//! directory shares. Capture publishes a set whole, then [`adopt`] moves
//! each artifact into the store, so bytes that did not change since an
//! earlier set are stored once however many sets keep them. Sets an earlier
//! version kept whole move the same way. Each chunk file is the object a bucket
//! holds under the same name, so replication uploads it as it is.
//!
//! A chunk is written under a temporary name, synced, and renamed into
//! place, so a chunk file present under its id is whole, and the store's
//! directories are synced before a set names its chunks. A capture checks an
//! existing chunk before reusing it and replaces a damaged one. Only
//! [`sweep`], under the backup lock during recovery and after retention,
//! removes chunks: temporary files and chunks no published set's index
//! names. A crash leaves at most unreferenced chunks, which the next sweep
//! removes.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::chunks::{self, CHUNKS, ChunkKeys, ChunkRef, Encoding, INDEX, Index, Stored};
use super::{BackupError, Completion, ObjectRef};
use crate::credentials::SecretKey;

const TEMP_PREFIX: &str = ".tmp-";

/// One key's chunks in a backup directory.
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(backups: &Path, keys: &ChunkKeys) -> Self {
        Self {
            root: backups.join(keys.root()),
        }
    }

    fn path(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn ensure(&self) -> Result<(), BackupError> {
        if let Some(parent) = self.root.parent() {
            super::ensure_private_dir(parent)?;
        }
        super::ensure_private_dir(&self.root)
    }

    /// Store a chunk unless an intact copy is already present: a damaged
    /// one is replaced, so later sets do not keep naming it. Returns whether
    /// it was written.
    pub fn put(&self, keys: &ChunkKeys, id: &str, bytes: &[u8]) -> Result<bool, BackupError> {
        let path = self.path(id);
        if let Ok(sealed) = self.sealed(id)
            && keys.open(id, &sealed).is_ok()
        {
            return Ok(false);
        }
        let sealed = keys.seal(bytes)?;
        let mut suffix = [0u8; 4];
        getrandom::fill(&mut suffix).map_err(|e| BackupError::Crypto(e.to_string()))?;
        let temp = self
            .root
            .join(format!("{TEMP_PREFIX}{id}-{}", hex::encode(suffix)));
        let written = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temp)
            .and_then(|mut file| {
                file.write_all(&sealed)?;
                file.sync_all()
            })
            .and_then(|()| fs::rename(&temp, &path));
        if let Err(err) = written {
            let _ = fs::remove_file(&temp);
            return Err(BackupError::io(path.display(), err));
        }
        Ok(true)
    }

    /// A chunk's sealed bytes, as a bucket stores them.
    pub fn sealed(&self, id: &str) -> Result<Vec<u8>, BackupError> {
        if !chunks::valid_id(id) {
            return Err(BackupError::Invalid("not a chunk ID".into()));
        }
        super::read_regular(&self.path(id), chunks::MAX_SEALED_CHUNK)
    }

    /// A chunk's bytes, checked against its id and length.
    pub fn get(&self, keys: &ChunkKeys, chunk: &ChunkRef) -> Result<Vec<u8>, BackupError> {
        let missing = |_| BackupError::Invalid(format!("chunk {} is missing", chunk.id));
        let plain = keys.open(&chunk.id, &self.sealed(&chunk.id).map_err(missing)?)?;
        if plain.len() != chunk.bytes as usize {
            return Err(BackupError::Invalid(format!(
                "chunk {} has the wrong length",
                chunk.id
            )));
        }
        Ok(plain)
    }
}

/// Move the published set at `set_dir` into the store of its backup
/// directory, if it is still whole, and leave only its index. Replacing the
/// completion record is the one step that switches the set over, after its
/// chunks and index are synced, so a crash leaves a whole set or a chunked
/// one. Run again on a chunked set, it only removes whole files an
/// interrupted run left. The caller holds the backup lock.
pub fn adopt(set_dir: &Path, key: &SecretKey) -> Result<(), BackupError> {
    let completion = super::restore::read_completion(set_dir)?;
    if completion.manifest.object != INDEX {
        let (completion, manifest, identity) = super::restore::open_set(set_dir, key)?;
        let backups = set_dir
            .parent()
            .ok_or_else(|| BackupError::Invalid("the set has no backup directory".into()))?;
        let keys = ChunkKeys::derive(key)?;
        let store = Store::open(backups, &keys);
        store.ensure()?;
        let mut artifacts = Vec::new();
        for artifact in &manifest.artifacts {
            let encoding = Encoding::of(artifact, manifest.schema_version);
            let (chunks, bytes, sha256) =
                chunks::read_stored(set_dir, artifact, &identity, encoding, |input| {
                    chunks::split(&keys, input, |id, bytes| {
                        store.put(&keys, id, bytes).map(drop)
                    })
                })?;
            artifacts.push(Stored {
                object: artifact.object.clone(),
                encoding,
                bytes,
                sha256,
                chunks,
            });
        }
        super::sync_dir(&store.root)?;
        if let Some(chunks) = store.root.parent() {
            super::sync_dir(chunks)?;
        }
        super::sync_dir(backups)?;
        let index = Index {
            index_version: chunks::INDEX_VERSION,
            manifest,
            artifacts,
        };
        let plain = serde_json::to_vec(&index).expect("serializable");
        if plain.len() as u64 > chunks::MAX_INDEX_BYTES {
            return Err(BackupError::Invalid("the chunk index is too large".into()));
        }
        let sealed = chunks::encrypt(&identity, &plain)?;
        let index_path = set_dir.join(INDEX);
        crate::private_file::replace(&index_path, &sealed)
            .map_err(|e| BackupError::io(index_path.display(), e))?;
        let chunked = Completion {
            manifest: ObjectRef {
                object: INDEX.into(),
                ciphertext_bytes: sealed.len() as u64,
                ciphertext_sha256: super::sha256_hex(&sealed),
            },
            ..completion
        };
        let complete = set_dir.join(super::COMPLETE);
        crate::private_file::replace(
            &complete,
            &serde_json::to_vec_pretty(&chunked).expect("serializable"),
        )
        .map_err(|e| BackupError::io(complete.display(), e))?;
    }
    for entry in fs::read_dir(set_dir).map_err(|e| BackupError::io(set_dir.display(), e))? {
        let entry = entry.map_err(|e| BackupError::io(set_dir.display(), e))?;
        let name = entry.file_name();
        if ![super::MARKER, super::COMPLETE, INDEX]
            .map(std::ffi::OsStr::new)
            .contains(&name.as_os_str())
        {
            let path = entry.path();
            fs::remove_file(&path).map_err(|e| BackupError::io(path.display(), e))?;
        }
    }
    super::sync_dir(set_dir)
}

/// Open the index of the chunked local set at `set_dir`.
pub fn open_index(set_dir: &Path, key: &SecretKey) -> Result<(Completion, Index), BackupError> {
    let completion = super::restore::read_completion(set_dir)?;
    if completion.manifest.object != INDEX {
        return Err(BackupError::Invalid("the set is not chunked".into()));
    }
    let sealed = super::read_regular(&set_dir.join(INDEX), chunks::MAX_INDEX_BYTES + 64 * 1024)?;
    if sealed.len() as u64 != completion.manifest.ciphertext_bytes
        || super::sha256_hex(&sealed) != completion.manifest.ciphertext_sha256
    {
        return Err(BackupError::Invalid(
            "the chunk index does not match its completion record".into(),
        ));
    }
    let identity = super::identity_for(key, &completion.encryption)?;
    let plain = super::open_small(&sealed, &identity, chunks::MAX_INDEX_BYTES)?;
    let index = Index::parse(&plain, &completion)?;
    Ok((completion, index))
}

/// A chunked local set opened for [`rebuild`].
pub struct Opened {
    completion: Completion,
    index: Index,
    identity: age::x25519::Identity,
    keys: ChunkKeys,
}

pub fn open(set_dir: &Path, key: &SecretKey) -> Result<Opened, BackupError> {
    let (completion, index) = open_index(set_dir, key)?;
    Ok(Opened {
        identity: super::identity_for(key, &completion.encryption)?,
        keys: ChunkKeys::derive(key)?,
        completion,
        index,
    })
}

/// Rebuild the chunked local set at `set_dir` as a whole set in `out`, a
/// new directory named like the set, for a restore to read.
pub fn rebuild(
    set_dir: &Path,
    out: &Path,
    opened: Opened,
    max_bytes: u64,
) -> Result<(), BackupError> {
    let backups = set_dir
        .parent()
        .ok_or_else(|| BackupError::Invalid("the set has no backup directory".into()))?;
    let Opened {
        completion,
        index,
        identity,
        keys,
    } = opened;
    let store = Store::open(backups, &keys);
    super::private_dir(out)?;
    let mut artifacts = Vec::new();
    for (artifact, stored) in index.manifest.artifacts.iter().zip(&index.artifacts) {
        if stored.encoding == Encoding::Plain && stored.bytes > max_bytes {
            return Err(BackupError::TooLarge(max_bytes));
        }
        let pieces = stored.chunks.iter().map(|c| store.get(&keys, c));
        let sealed = chunks::rebuild(
            &out.join(&artifact.object),
            &identity,
            stored,
            pieces,
            max_bytes,
        )?;
        artifacts.push(chunks::rebuilt(artifact, stored, sealed)?);
    }
    chunks::publish_rebuilt(out, index.manifest, artifacts, &completion, &identity)
}

/// Remove this key's temporary chunk files and the chunks no completed set
/// in `backups` names. Refuses to remove anything while one of this key's
/// indexes cannot be read. The caller holds the backup lock.
pub fn sweep(backups: &Path, key: &SecretKey) -> Result<usize, BackupError> {
    let keys = ChunkKeys::derive(key)?;
    let store = Store::open(backups, &keys);
    let entries = match fs::read_dir(&store.root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(BackupError::io(store.root.display(), err)),
    };
    let ours = super::key_id(key);
    let mut referenced = HashSet::new();
    for set_dir in published_sets(backups)? {
        let completion = super::restore::read_completion(&set_dir)?;
        if completion.manifest.object != INDEX || completion.encryption.key_id != ours {
            continue;
        }
        let (_, index) = open_index(&set_dir, key)?;
        referenced.extend(index.chunk_ids().map(str::to_owned));
    }
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let unused = chunks::valid_id(&name) && !referenced.contains(&name);
        if name.starts_with(TEMP_PREFIX) || unused {
            let path = entry.path();
            fs::remove_file(&path).map_err(|e| BackupError::io(path.display(), e))?;
            removed += 1;
        }
    }
    super::sync_dir(&store.root)?;
    Ok(removed)
}

/// Every published set directory in `backups`, failing on anything that
/// cannot be read rather than leaving a set out, so a sweep never takes a
/// set's chunks for unused.
fn published_sets(backups: &Path) -> Result<Vec<PathBuf>, BackupError> {
    let io = |path: &Path, err| BackupError::io(path.display(), err);
    let present = |path: &Path| match fs::symlink_metadata(path) {
        Ok(meta) => Ok(meta.is_file()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(io(path, err)),
    };
    let mut sets = Vec::new();
    for entry in fs::read_dir(backups).map_err(|e| io(backups, e))? {
        let entry = entry.map_err(|e| io(backups, e))?;
        let path = entry.path();
        let named = entry.file_name().to_str().is_some_and(super::valid_set_id);
        let is_dir = entry.file_type().map_err(|e| io(&path, e))?.is_dir();
        if named
            && is_dir
            && present(&path.join(super::MARKER))?
            && present(&path.join(super::COMPLETE))?
        {
            sets.push(path);
        }
    }
    Ok(sets)
}

/// The bytes the backup directory's chunk stores hold.
pub fn stored_bytes(backups: &Path) -> u64 {
    let Ok(keys) = fs::read_dir(backups.join(CHUNKS.trim_end_matches('/'))) else {
        return 0;
    };
    keys.flatten()
        .filter(|k| k.file_type().is_ok_and(|t| t.is_dir()))
        .flat_map(|k| fs::read_dir(k.path()).into_iter().flatten().flatten())
        .filter_map(|c| c.metadata().ok())
        .filter(|m| m.is_file())
        .fold(0u64, |sum, m| sum.saturating_add(m.len()))
}

/// Whether the published set at `set_dir` keeps its artifacts in the store.
pub fn is_chunked(set_dir: &Path) -> Result<bool, BackupError> {
    Ok(super::restore::read_completion(set_dir)?.manifest.object == INDEX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn key() -> SecretKey {
        use base64::Engine;
        SecretKey::parse(&base64::engine::general_purpose::STANDARD.encode([4u8; 32])).unwrap()
    }

    #[test]
    fn a_damaged_chunk_is_replaced_instead_of_reused() {
        let backups = tempfile::tempdir().unwrap();
        let keys = ChunkKeys::derive(&key()).unwrap();
        let store = Store::open(backups.path(), &keys);
        store.ensure().unwrap();
        let bytes = b"unchanged since the last capture".repeat(100);
        let id = keys.id(&bytes);
        assert!(store.put(&keys, &id, &bytes).unwrap());
        assert!(
            !store.put(&keys, &id, &bytes).unwrap(),
            "an intact chunk is reused"
        );
        let path = store.path(&id);
        let mut sealed = fs::read(&path).unwrap();
        let at = sealed.len() / 2;
        sealed[at] ^= 1;
        fs::write(&path, sealed).unwrap();
        let chunk = ChunkRef {
            id: id.clone(),
            bytes: bytes.len() as u32,
        };
        assert!(store.get(&keys, &chunk).is_err());
        assert!(
            store.put(&keys, &id, &bytes).unwrap(),
            "a damaged chunk is written again"
        );
        assert_eq!(store.get(&keys, &chunk).unwrap(), bytes);
    }

    #[test]
    fn a_set_that_cannot_be_read_stops_the_sweep() {
        let backups = tempfile::tempdir().unwrap();
        let keys = ChunkKeys::derive(&key()).unwrap();
        let store = Store::open(backups.path(), &keys);
        store.ensure().unwrap();
        let orphan = "cd".repeat(32);
        fs::write(store.path(&orphan), "unused").unwrap();
        let set = backups.path().join("20261116T000000000Z-00000000");
        super::super::private_dir(&set).unwrap();
        super::super::mark_owned(&set).unwrap();
        fs::set_permissions(&set, fs::Permissions::from_mode(0o000)).unwrap();
        let swept = sweep(backups.path(), &key());
        fs::set_permissions(&set, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(swept.is_err(), "{swept:?}");
        assert!(store.path(&orphan).exists(), "nothing is removed");
        assert_eq!(sweep(backups.path(), &key()).unwrap(), 1);
        assert!(!store.path(&orphan).exists());
    }
}
