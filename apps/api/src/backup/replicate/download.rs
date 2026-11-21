//! The restore side: rebuild a remote set as a local one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;

use super::{PARALLEL, open_index};
use crate::backup::chunks::{self, ChunkKeys, ChunkRef, Encoding, INDEX};
use crate::backup::s3::{S3, S3Error};
use crate::backup::{BackupError, COMPLETE, Completion, MANIFEST};
use crate::credentials::SecretKey;

/// Download a completed remote set into a new private directory under
/// `parent`, as a local set the restore checks again. A chunked set is
/// rebuilt from its chunks: each artifact is verified against the index and
/// sealed again with the set's identity, so its manifest and completion
/// record are written anew. A set replicated whole is fetched object by
/// object. Every object is bounded before it is fetched (artifacts by
/// `max_bytes`). Needs only the bucket, not the database.
pub async fn download_set(
    s3: &S3,
    set_id: &str,
    parent: &Path,
    key: &SecretKey,
    max_bytes: u64,
) -> Result<PathBuf, BackupError> {
    if !crate::backup::valid_set_id(set_id) {
        return Err(BackupError::Invalid("not a set ID".into()));
    }
    crate::backup::ensure_private_dir(parent)?;
    let dir = parent.join(set_id);
    crate::backup::private_dir(&dir)?;
    let result = async {
        let complete = s3
            .get_bytes(
                &s3.key(&format!("sets/{set_id}/{COMPLETE}")),
                crate::backup::MAX_COMPLETE_BYTES,
            )
            .await
            .map_err(|e| match e {
                S3Error::Status { status: 404, .. } => {
                    BackupError::Invalid(format!("the remote set {set_id} is not complete"))
                }
                other => other.into(),
            })?;
        let completion: Completion = serde_json::from_slice(&complete)
            .map_err(|e| BackupError::Invalid(format!("remote completion record: {e}")))?;
        if completion.set_id != set_id {
            return Err(BackupError::Invalid(
                "the remote completion record names another set".into(),
            ));
        }
        match completion.manifest.object.as_str() {
            MANIFEST => download_whole(s3, &dir, &complete, &completion, key, max_bytes).await,
            INDEX => download_chunked(s3, &dir, &completion, key, max_bytes).await,
            _ => Err(BackupError::Invalid(
                "the remote completion record names an unknown manifest".into(),
            )),
        }
    }
    .await;
    if let Err(err) = result {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(err);
    }
    Ok(dir)
}

async fn download_chunked(
    s3: &S3,
    dir: &Path,
    completion: &Completion,
    key: &SecretKey,
    max_bytes: u64,
) -> Result<(), BackupError> {
    let index = open_index(s3, &completion.set_id, completion, key).await?;
    let identity = crate::backup::identity_for(key, &completion.encryption)?;
    let keys = Arc::new(ChunkKeys::derive(key)?);
    let mut artifacts = Vec::new();
    for (artifact, stored) in index.manifest.artifacts.iter().zip(&index.artifacts) {
        if stored.encoding == Encoding::Plain && stored.bytes > max_bytes {
            return Err(BackupError::TooLarge(max_bytes));
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, BackupError>>(PARALLEL);
        let rebuild = {
            let (target, identity, stored) =
                (dir.join(&artifact.object), identity.clone(), stored.clone());
            crate::backup::blocking(move || {
                let chunks = std::iter::from_fn(|| rx.blocking_recv());
                chunks::rebuild(&target, &identity, &stored, chunks, max_bytes)
            })
        };
        let fetch = async {
            let tx = tx;
            let mut chunks = futures_util::stream::iter(&stored.chunks)
                .map(|chunk| fetch_for_restore(s3, &keys, chunk))
                .buffered(PARALLEL);
            while let Some(chunk) = chunks.next().await {
                match chunk {
                    Ok(bytes) => {
                        if tx.send(Ok(bytes)).await.is_err() {
                            return None;
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(Err(BackupError::Interrupted)).await;
                        return Some(err);
                    }
                }
            }
            None
        };
        let (failed, sealed) = tokio::join!(fetch, rebuild);
        if let Some(err) = failed {
            return Err(err);
        }
        let sealed = sealed?;
        artifacts.push(chunks::rebuilt(artifact, stored, sealed)?);
    }
    chunks::publish_rebuilt(dir, index.manifest, artifacts, completion, &identity)
}

async fn fetch_for_restore(
    s3: &S3,
    keys: &Arc<ChunkKeys>,
    chunk: &ChunkRef,
) -> Result<Vec<u8>, BackupError> {
    let sealed = s3
        .get_bytes(
            &s3.key(&format!("{}{}", keys.root(), chunk.id)),
            chunks::MAX_SEALED_CHUNK,
        )
        .await?;
    let (keys, id) = (keys.clone(), chunk.id.clone());
    let plain = crate::backup::blocking(move || keys.open(&id, &sealed)).await?;
    if plain.len() != chunk.bytes as usize {
        return Err(BackupError::Invalid(format!(
            "chunk {} has the wrong length",
            chunk.id
        )));
    }
    Ok(plain)
}

/// A set an earlier version replicated whole: fetch only the objects its
/// decrypted manifest names.
async fn download_whole(
    s3: &S3,
    dir: &Path,
    complete: &[u8],
    completion: &Completion,
    key: &SecretKey,
    max_bytes: u64,
) -> Result<(), BackupError> {
    let set_key = |object: &str| s3.key(&format!("sets/{}/{object}", completion.set_id));
    if completion.manifest.ciphertext_bytes > crate::backup::MAX_MANIFEST_BYTES + 64 * 1024 {
        return Err(BackupError::Invalid(
            "the remote manifest is too large".into(),
        ));
    }
    crate::backup::write_new(&dir.join(COMPLETE), complete)?;
    fetch(
        s3,
        &set_key(MANIFEST),
        completion.manifest.ciphertext_bytes,
        &dir.join(MANIFEST),
    )
    .await?;
    let (_, manifest, _) = crate::backup::restore::open_set(dir, key)?;
    for artifact in &manifest.artifacts {
        // Age adds 16 bytes per 64 KiB chunk and a small header.
        if artifact.ciphertext_bytes > max_bytes + max_bytes / 4096 + 64 * 1024 {
            return Err(BackupError::TooLarge(max_bytes));
        }
        fetch(
            s3,
            &set_key(&artifact.object),
            artifact.ciphertext_bytes,
            &dir.join(&artifact.object),
        )
        .await?;
    }
    Ok(())
}

async fn fetch(s3: &S3, key: &str, bytes: u64, path: &Path) -> Result<(), BackupError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| BackupError::io(path.display(), e))?;
    s3.get_with(key, bytes, |piece| {
        file.write_all(piece)
            .map_err(|e| S3Error::Protocol(e.to_string()))
    })
    .await?;
    file.sync_all()
        .map_err(|e| BackupError::io(path.display(), e))
}
