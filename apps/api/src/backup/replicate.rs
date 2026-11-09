//! Replication of completed local sets to an S3 or R2 bucket.
//!
//! Remote layout: `<prefix>sets/<set id>/complete.json` and
//! `<prefix>sets/<set id>/index.json.age` per set, and the chunks every set
//! shares at `<prefix>chunks/<id>` (see [`super::chunks`]). For each
//! completed local set without a verified remote completion, the worker
//! cuts every artifact into chunks, uploads only the chunks the bucket does
//! not list (create-only), verifies each new chunk by readback, uploads the
//! index, and only then publishes `complete.json` with `If-None-Match: *`,
//! reading it back. A set whose remote completion exists with other
//! contents is a conflict and is never replaced. Sets replicated whole by
//! earlier versions (`sets/<id>/<object>`) stay restorable and age out.
//!
//! Retention keeps the newest complete sets, removing a set's completion
//! record before its other objects, then sweeps chunks no remaining set
//! references once they are older than a grace period, which covers an
//! interrupted upload that resumes later. Each run also downloads a random
//! sample of chunks and deletes any that fail to verify; a later upload of
//! the same bytes, or a repair from the local copy of a set that uses them,
//! puts them back. All of it runs under the worker's backup lock.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::{StreamExt, TryStreamExt};
use sqlx::SqlitePool;

use super::chunks::{self, CHUNKS, ChunkKeys, ChunkRef, Encoding, INDEX, Index, Stored};
use super::s3::{Put, S3, S3Error};
use super::{BackupError, COMPLETE, Completion, MANIFEST, ObjectRef};
use crate::credentials::SecretKey;

const PARALLEL: usize = 4;
const SAMPLE: usize = 8;

impl From<S3Error> for BackupError {
    fn from(err: S3Error) -> Self {
        BackupError::Remote(err.to_string())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    /// New chunks uploaded.
    pub uploaded: usize,
    /// Chunks the bucket already held.
    pub reused: usize,
    /// Every byte this run wrote to the bucket.
    pub uploaded_bytes: u64,
    pub aborted: usize,
    /// Sampled chunks verified, and those found damaged and deleted.
    pub checked: usize,
    pub damaged: usize,
    /// Missing chunks uploaded again from local sets.
    pub repaired: usize,
    /// Unreferenced chunks removed.
    pub swept: usize,
    pub completed: Vec<String>,
    pub removed: Vec<String>,
}

/// A test hook run after the uploads and before final verification.
pub type BeforePublish =
    std::sync::Arc<dyn Fn(String) -> futures_util::future::BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone)]
pub struct Replicator {
    pub s3: S3,
    pool: SqlitePool,
    /// Completed remote sets kept (at least one).
    pub keep: usize,
    /// Unreferenced chunks younger than this are kept.
    pub grace: chrono::Duration,
    /// Chunks verified by download on each run.
    pub sample: usize,
    pub before_publish: Option<BeforePublish>,
}

struct Upload<'a> {
    keys: &'a Arc<ChunkKeys>,
    /// Chunk IDs the bucket holds or this run uploaded.
    known: HashSet<String>,
    stats: Stats,
}

impl Replicator {
    pub fn new(s3: S3, pool: SqlitePool, keep: usize) -> Self {
        Self {
            s3,
            pool,
            keep,
            grace: chrono::Duration::hours(24),
            sample: SAMPLE,
            before_publish: None,
        }
    }

    fn set_key(&self, set_id: &str, object: &str) -> String {
        self.s3.key(&format!("sets/{set_id}/{object}"))
    }

    fn chunk_key(&self, id: &str) -> String {
        self.s3.key(&format!("{CHUNKS}{id}"))
    }

    async fn record(
        &self,
        set_id: &str,
        state: &str,
        error: Option<&str>,
        whole_assistant: bool,
        uploaded_bytes: Option<u64>,
    ) -> Result<(), BackupError> {
        let error: Option<String> = error.map(|e| e.chars().take(500).collect());
        sqlx::query(
            "INSERT INTO backup_remote (destination, set_id, state, error, whole_assistant,
                 uploaded_bytes, updated_at, verified_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), CASE WHEN ?3 = 'completed' THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') END)
             ON CONFLICT (destination, set_id) DO UPDATE SET state = ?3, error = ?4,
                 whole_assistant = ?5, uploaded_bytes = COALESCE(?6, backup_remote.uploaded_bytes),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 verified_at = CASE WHEN ?3 = 'completed' THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                                    ELSE backup_remote.verified_at END",
        )
        .bind(self.s3.destination())
        .bind(set_id)
        .bind(state)
        .bind(&error)
        .bind(whole_assistant)
        .bind(uploaded_bytes.map(|b| b as i64))
        .execute(&self.pool)
        .await?;
        sqlx::query("UPDATE backup_runs SET remote_status = ?2 WHERE set_id = ?1")
            .bind(set_id)
            .bind(state)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn verified(&self) -> Result<Vec<(String, bool)>, BackupError> {
        Ok(sqlx::query_as(
            "SELECT set_id, whole_assistant FROM backup_remote
             WHERE destination = ?1 AND state = 'completed' ORDER BY set_id",
        )
        .bind(self.s3.destination())
        .fetch_all(&self.pool)
        .await?)
    }

    /// Local sets whose remote completion is not verified at this
    /// destination.
    pub async fn awaiting(&self, dir: &Path) -> Result<Vec<String>, BackupError> {
        let done: Vec<String> = sqlx::query_scalar(
            "SELECT set_id FROM backup_remote
             WHERE destination = ?1 AND state IN ('completed', 'retired')",
        )
        .bind(self.s3.destination())
        .fetch_all(&self.pool)
        .await?;
        Ok(super::completed_sets(dir)?
            .into_iter()
            .filter(|s| !done.contains(s))
            .collect())
    }

    /// Refuse a bucket whose lifecycle rules would expire chunks that newer
    /// sets still use. A bucket that does not report its rules is accepted.
    pub async fn refuse_expiring_rules(&self) -> Result<(), BackupError> {
        match self.s3.expiring_rule(&self.s3.key(CHUNKS)).await {
            Ok(Some(rule)) => Err(BackupError::Refused(expiring_rule_message(&rule))),
            Ok(None) => Ok(()),
            Err(err) => {
                tracing::warn!(error = %err, "the bucket's lifecycle rules could not be read");
                Ok(())
            }
        }
    }

    /// Replicate every awaiting local set (oldest first), then apply remote
    /// retention. A failing set is recorded and the error returned after
    /// the others were tried.
    pub async fn replicate_pending(
        &self,
        dir: &Path,
        key: &SecretKey,
    ) -> Result<Stats, BackupError> {
        let awaiting = self.awaiting(dir).await?;
        let keys = Arc::new(ChunkKeys::derive(key)?);
        let mut run = Upload {
            keys: &keys,
            known: HashSet::new(),
            stats: Stats::default(),
        };
        let prepared = async {
            self.refuse_expiring_rules().await?;
            run.known = self.chunk_listing().await?.into_keys().collect();
            self.check_sample(&mut run).await
        }
        .await;
        if let Err(err) = prepared {
            for set_id in &awaiting {
                self.record(set_id, "failed", Some(&err.to_string()), false, None)
                    .await?;
            }
            return Err(err);
        }
        let mut first_error = None;
        for set_id in awaiting {
            let before = run.stats.uploaded_bytes;
            match self.replicate_set(&dir.join(&set_id), key, &mut run).await {
                Ok(whole) => {
                    let uploaded = run.stats.uploaded_bytes - before;
                    self.record(&set_id, "completed", None, whole, Some(uploaded))
                        .await?;
                    run.stats.completed.push(set_id);
                }
                Err(err) => {
                    tracing::warn!(set = %set_id, error = %err, "replication failed");
                    self.record(&set_id, "failed", Some(&err.to_string()), false, None)
                        .await?;
                    first_error.get_or_insert(err);
                }
            }
        }
        if let Some(err) = first_error {
            return Err(err);
        }
        run.stats.removed = self.apply_remote_retention(dir, key, &mut run).await?;
        Ok(run.stats)
    }

    /// Chunks in the bucket by ID.
    async fn chunk_listing(&self) -> Result<HashMap<String, super::s3::Object>, BackupError> {
        let root = self.s3.key(CHUNKS);
        Ok(self
            .s3
            .list(&root)
            .await?
            .into_iter()
            .filter_map(|o| {
                let id = o.key.strip_prefix(&root)?.to_owned();
                chunks::valid_id(&id).then_some((id, o))
            })
            .collect())
    }

    /// Download and open a chunk: `None` when it is missing or damaged.
    async fn fetch_chunk(
        &self,
        keys: &Arc<ChunkKeys>,
        id: &str,
    ) -> Result<Option<Vec<u8>>, BackupError> {
        let sealed = match self
            .s3
            .get_bytes(&self.chunk_key(id), chunks::MAX_SEALED_CHUNK)
            .await
        {
            Ok(sealed) => sealed,
            Err(S3Error::Status { status: 404, .. } | S3Error::Protocol(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let (keys, id) = (keys.clone(), id.to_owned());
        tokio::task::spawn_blocking(move || keys.open(&id, &sealed).ok())
            .await
            .map_err(|e| BackupError::Io(e.to_string()))
    }

    /// Of `ids`, the chunks that are missing or fail to verify.
    async fn unreadable(
        &self,
        keys: &Arc<ChunkKeys>,
        ids: Vec<String>,
    ) -> Result<Vec<String>, BackupError> {
        futures_util::stream::iter(ids)
            .map(|id| async move {
                let ok = self.fetch_chunk(keys, &id).await?.is_some();
                Ok::<_, BackupError>((!ok).then_some(id))
            })
            .buffer_unordered(PARALLEL)
            .try_filter_map(|id| async move { Ok(id) })
            .try_collect()
            .await
    }

    /// Verify a random sample of the bucket's chunks; delete any that fail,
    /// so the next upload of their bytes replaces them.
    async fn check_sample(&self, run: &mut Upload<'_>) -> Result<(), BackupError> {
        let mut ids: Vec<String> = run.known.iter().cloned().collect();
        let mut picked = Vec::new();
        while picked.len() < self.sample && !ids.is_empty() {
            let mut random = [0u8; 8];
            getrandom::fill(&mut random).expect("random source");
            let at = (u64::from_le_bytes(random) % ids.len() as u64) as usize;
            picked.push(ids.swap_remove(at));
        }
        run.stats.checked += picked.len();
        for id in self.unreadable(run.keys, picked).await? {
            tracing::warn!(chunk = %id, "a damaged chunk was removed from the bucket");
            self.s3.delete(&self.chunk_key(&id)).await?;
            run.known.remove(&id);
            run.stats.damaged += 1;
        }
        Ok(())
    }

    /// Whether `set_id` is published: by the record this installation
    /// published whole, or by one naming an index that opens with `key`.
    /// Another record is a conflict. A record of ours whose index is damaged
    /// is withdrawn, so the set uploads again.
    async fn published(
        &self,
        set_id: &str,
        local_complete: &[u8],
        key: &SecretKey,
    ) -> Result<bool, BackupError> {
        let complete_key = self.set_key(set_id, COMPLETE);
        if self.s3.head(&complete_key).await?.is_none() {
            return Ok(false);
        }
        let remote = self
            .s3
            .get_bytes(&complete_key, super::MAX_COMPLETE_BYTES)
            .await?;
        if remote == local_complete {
            return Ok(true);
        }
        match self.read_index(set_id, &remote, key).await {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Err(BackupError::Refused(format!(
                "the remote set {set_id} has a different completion record; it is never replaced"
            ))),
            Err(err @ BackupError::Remote(_)) => Err(err),
            Err(err) => {
                tracing::warn!(set = %set_id, error = %err, "a damaged remote set is uploaded again");
                self.s3.delete(&complete_key).await?;
                Ok(false)
            }
        }
    }

    /// The index a remote completion names, if the set is a chunked set
    /// made with `key`. `None` for a set replicated whole or made with
    /// another key; an error when an index that should open does not.
    async fn read_index(
        &self,
        set_id: &str,
        complete: &[u8],
        key: &SecretKey,
    ) -> Result<Option<Index>, BackupError> {
        let Ok(completion) = serde_json::from_slice::<Completion>(complete) else {
            return Ok(None);
        };
        if completion.set_id != set_id
            || completion.manifest.object != INDEX
            || completion.encryption.key_id != super::key_id(key)
        {
            return Ok(None);
        }
        Ok(Some(open_index(&self.s3, set_id, &completion, key).await?))
    }

    async fn replicate_set(
        &self,
        set_dir: &Path,
        key: &SecretKey,
        run: &mut Upload<'_>,
    ) -> Result<bool, BackupError> {
        let set_id = set_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        let local_complete =
            super::read_regular(&set_dir.join(COMPLETE), super::MAX_COMPLETE_BYTES)?;
        let (completion, manifest, identity) = super::restore::open_set(set_dir, key)?;
        let whole = manifest.coverage.whole_assistant;
        if self.published(&set_id, &local_complete, key).await? {
            return Ok(whole);
        }
        let leftovers = self.set_key(&set_id, "");
        for (upload_key, upload_id) in self.s3.list_multipart(&leftovers).await? {
            self.s3.abort_multipart(&upload_key, &upload_id).await?;
            run.stats.aborted += 1;
        }
        for object in self.s3.list(&leftovers).await? {
            self.s3.delete(&object.key).await?;
        }
        // Chunks this set uploads count as present for later sets only once
        // they are verified.
        let before = run.known.clone();
        let stored = match self
            .upload_verified(&set_id, set_dir, &manifest, &identity, run)
            .await
        {
            Ok(stored) => stored,
            Err(err) => {
                run.known = before;
                return Err(err);
            }
        };
        let index = Index {
            index_version: chunks::INDEX_VERSION,
            manifest,
            artifacts: stored,
        };
        let sealed_index = chunks::encrypt(
            &identity,
            &serde_json::to_vec(&index).expect("serializable"),
        )?;
        let index_ref = ObjectRef {
            object: INDEX.into(),
            ciphertext_bytes: sealed_index.len() as u64,
            ciphertext_sha256: super::sha256_hex(&sealed_index),
        };
        let index_key = self.set_key(&set_id, INDEX);
        run.stats.uploaded_bytes += sealed_index.len() as u64;
        self.s3.put(&index_key, sealed_index.clone(), false).await?;
        if self
            .s3
            .get_bytes(&index_key, chunks::MAX_INDEX_BYTES + 64 * 1024)
            .await?
            != sealed_index
        {
            return Err(BackupError::Invalid(
                "the remote chunk index does not match; the set is not complete".into(),
            ));
        }
        let remote_complete = serde_json::to_vec_pretty(&Completion {
            manifest: index_ref,
            ..completion
        })
        .expect("serializable");
        let complete_key = self.set_key(&set_id, COMPLETE);
        run.stats.uploaded_bytes += remote_complete.len() as u64;
        if self
            .s3
            .put(&complete_key, remote_complete.clone(), true)
            .await?
            == Put::Exists
        {
            return Err(BackupError::Refused(format!(
                "the remote set {set_id} was completed with other contents"
            )));
        }
        let read_back = self
            .s3
            .get_bytes(&complete_key, super::MAX_COMPLETE_BYTES)
            .await?;
        if read_back != remote_complete {
            return Err(BackupError::Invalid(
                "the published completion record differs".into(),
            ));
        }
        Ok(whole)
    }

    /// Upload every artifact's missing chunks and verify them by readback.
    /// A chunk that fails is deleted, so the next run uploads it again.
    async fn upload_verified(
        &self,
        set_id: &str,
        set_dir: &Path,
        manifest: &super::Manifest,
        identity: &age::x25519::Identity,
        run: &mut Upload<'_>,
    ) -> Result<Vec<Stored>, BackupError> {
        let mut stored = Vec::new();
        let mut fresh = Vec::new();
        for artifact in &manifest.artifacts {
            let encoding = Encoding::of(artifact, manifest.schema_version);
            let (entry, uploaded) = self
                .upload_artifact(set_dir, artifact, identity, encoding, run)
                .await?;
            stored.push(entry);
            fresh.extend(uploaded);
        }
        if let Some(hook) = &self.before_publish {
            hook(set_id.to_owned()).await;
        }
        let failed = self.unreadable(run.keys, fresh).await?;
        if let Some(id) = failed.first() {
            for id in &failed {
                self.s3.delete(&self.chunk_key(id)).await?;
            }
            return Err(BackupError::Invalid(format!(
                "the remote copy of chunk {id} does not match; the set is not complete"
            )));
        }
        Ok(stored)
    }

    /// Cut one local artifact into chunks and upload those the bucket does
    /// not hold yet. Returns its index entry and the new chunks' IDs.
    async fn upload_artifact(
        &self,
        set_dir: &Path,
        artifact: &super::Artifact,
        identity: &age::x25519::Identity,
        encoding: Encoding,
        run: &mut Upload<'_>,
    ) -> Result<(Stored, Vec<String>), BackupError> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, Vec<u8>)>(PARALLEL);
        let producer = {
            let (set_dir, artifact, identity) =
                (set_dir.to_owned(), artifact.clone(), identity.clone());
            let keys = run.keys.clone();
            let mut known = std::mem::take(&mut run.known);
            tokio::task::spawn_blocking(move || {
                let split =
                    chunks::read_stored(&set_dir, &artifact, &identity, encoding, |input| {
                        chunks::split(&keys, input, |id, bytes| {
                            if known.insert(id.to_owned()) {
                                let sealed = keys.seal(bytes)?;
                                tx.blocking_send((id.to_owned(), sealed))
                                    .map_err(|_| BackupError::Interrupted)?;
                            }
                            Ok(())
                        })
                    });
                (split, known)
            })
        };
        let uploads = futures_util::stream::poll_fn(|cx| rx.poll_recv(cx))
            .map(|(id, sealed)| async move {
                let bytes = sealed.len() as u64;
                self.s3.put(&self.chunk_key(&id), sealed, true).await?;
                if crate::faults::hit("replication.after_chunk") {
                    return Err(BackupError::Interrupted);
                }
                Ok::<_, BackupError>((id, bytes))
            })
            .buffer_unordered(PARALLEL)
            .try_collect::<Vec<_>>();
        let (uploaded, produced) = tokio::join!(uploads, producer);
        let (split, known) = produced.map_err(|e| BackupError::Io(e.to_string()))?;
        run.known = known;
        let uploaded = uploaded?;
        let (chunks, bytes, sha256) = split?;
        run.stats.uploaded += uploaded.len();
        run.stats.reused += chunks.len() - uploaded.len();
        run.stats.uploaded_bytes += uploaded.iter().map(|(_, b)| b).sum::<u64>();
        Ok((
            Stored {
                object: artifact.object.clone(),
                encoding,
                bytes,
                sha256,
                chunks,
            },
            uploaded.into_iter().map(|(id, _)| id).collect(),
        ))
    }

    async fn remote_sets(&self) -> Result<Vec<(String, bool, Vec<String>)>, BackupError> {
        let root = self.s3.key("sets/");
        let mut sets: std::collections::BTreeMap<String, (bool, Vec<String>)> = Default::default();
        for object in self.s3.list(&root).await? {
            let Some(rest) = object.key.strip_prefix(&root) else {
                continue;
            };
            let Some((set, name)) = rest.split_once('/') else {
                continue;
            };
            if !super::valid_set_id(set) {
                continue;
            }
            let entry = sets.entry(set.to_owned()).or_default();
            entry.0 |= name == COMPLETE;
            entry.1.push(object.key);
        }
        Ok(sets
            .into_iter()
            .map(|(id, (complete, keys))| (id, complete, keys))
            .collect())
    }

    /// Completed remote set IDs, oldest first.
    pub async fn completed_remote(&self) -> Result<Vec<String>, BackupError> {
        Ok(self
            .remote_sets()
            .await?
            .into_iter()
            .filter(|(_, complete, _)| *complete)
            .map(|(id, ..)| id)
            .collect())
    }

    async fn apply_remote_retention(
        &self,
        dir: &Path,
        key: &SecretKey,
        run: &mut Upload<'_>,
    ) -> Result<Vec<String>, BackupError> {
        let (sets, verified) = tokio::join!(self.remote_sets(), self.verified());
        let sets = sets?;
        let verified = verified?;
        let present: Vec<&(String, bool)> = verified
            .iter()
            .filter(|(id, _)| sets.iter().any(|(s, complete, _)| s == id && *complete))
            .collect();
        let keep = self.keep.max(1);
        let mut kept: Vec<String> = present
            .iter()
            .rev()
            .take(keep)
            .map(|(id, _)| id.clone())
            .collect();
        if let Some((id, _)) = present.iter().rev().find(|(_, whole)| *whole) {
            kept.push(id.clone());
        }
        let local = super::completed_sets(dir)?;
        let mut removed = Vec::new();
        for (id, has_complete, keys) in &sets {
            let ours = present.iter().any(|(v, _)| v == id);
            let old = ours && !kept.contains(id);
            let abandoned = !has_complete && !local.contains(id);
            if !old && !abandoned {
                continue;
            }
            if *has_complete {
                self.s3.delete(&self.set_key(id, COMPLETE)).await?;
            }
            for key in keys
                .iter()
                .filter(|k| !k.ends_with(&format!("/{COMPLETE}")))
            {
                self.s3.delete(key).await?;
            }
            if old {
                sqlx::query(
                    "UPDATE backup_remote SET state = 'retired', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                     WHERE destination = ?1 AND set_id = ?2",
                )
                .bind(self.s3.destination())
                .bind(id)
                .execute(&self.pool)
                .await?;
            }
            removed.push(id.clone());
        }
        let mut indexes = Vec::new();
        for (id, has_complete, _) in &sets {
            if !*has_complete || removed.contains(id) {
                continue;
            }
            let complete = self
                .s3
                .get_bytes(&self.set_key(id, COMPLETE), super::MAX_COMPLETE_BYTES)
                .await?;
            if let Some(index) = self.read_index(id, &complete, key).await? {
                indexes.push((id.clone(), index));
            }
        }
        self.sweep(dir, key, &indexes, run).await?;
        let stored: u64 = self
            .s3
            .list(&self.s3.key(""))
            .await?
            .iter()
            .map(|o| o.size)
            .sum();
        sqlx::query(
            "INSERT INTO backup_bucket_usage (destination, stored_bytes) VALUES (?1, ?2)
             ON CONFLICT (destination) DO UPDATE SET stored_bytes = ?2,
                 measured_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')",
        )
        .bind(self.s3.destination())
        .bind(stored as i64)
        .execute(&self.pool)
        .await?;
        Ok(removed)
    }

    /// Delete chunks no remaining set references once they are older than
    /// the grace period, and put back referenced chunks that are missing.
    async fn sweep(
        &self,
        dir: &Path,
        key: &SecretKey,
        indexes: &[(String, Index)],
        run: &mut Upload<'_>,
    ) -> Result<(), BackupError> {
        let listing = self.chunk_listing().await?;
        let referenced: HashSet<&str> = indexes.iter().flat_map(|(_, i)| i.chunk_ids()).collect();
        let now = chrono::Utc::now();
        for (id, object) in &listing {
            let expired = object.modified.is_some_and(|at| now - at >= self.grace);
            if !referenced.contains(id.as_str()) && expired {
                self.s3.delete(&object.key).await?;
                run.stats.swept += 1;
            }
        }
        let missing: HashSet<String> = referenced
            .into_iter()
            .filter(|id| !listing.contains_key(*id))
            .map(str::to_owned)
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let restored = self.repair(dir, key, indexes, &missing, run).await?;
        let broken: Vec<&str> = indexes
            .iter()
            .filter(|(_, index)| {
                index
                    .chunk_ids()
                    .any(|id| missing.contains(id) && !restored.contains(id))
            })
            .map(|(id, _)| id.as_str())
            .collect();
        if broken.is_empty() {
            return Ok(());
        }
        Err(BackupError::Invalid(format!(
            "chunks are missing from the bucket and no local set holds them; the remote sets {} cannot be restored",
            broken.join(", ")
        )))
    }

    /// Upload missing chunks again from local sets that still hold them.
    async fn repair(
        &self,
        dir: &Path,
        key: &SecretKey,
        indexes: &[(String, Index)],
        missing: &HashSet<String>,
        run: &mut Upload<'_>,
    ) -> Result<HashSet<String>, BackupError> {
        let local = super::completed_sets(dir)?;
        let mut restored = HashSet::new();
        for (set_id, index) in indexes {
            if !local.contains(set_id) {
                continue;
            }
            for stored in &index.artifacts {
                let mut offset = 0u64;
                let mut wanted = Vec::new();
                for chunk in &stored.chunks {
                    if missing.contains(&chunk.id) && !restored.contains(&chunk.id) {
                        wanted.push((offset, chunk.clone()));
                    }
                    offset += chunk.bytes as u64;
                }
                if wanted.is_empty() {
                    continue;
                }
                let set_dir = dir.join(set_id);
                let found = match super::restore::open_set(&set_dir, key) {
                    Ok((_, manifest, identity)) => {
                        let artifact = manifest
                            .artifacts
                            .into_iter()
                            .find(|a| a.object == stored.object);
                        let (keys, encoding) = (run.keys.clone(), stored.encoding);
                        tokio::task::spawn_blocking(move || {
                            extract(&set_dir, artifact, &identity, encoding, &keys, &wanted)
                        })
                        .await
                        .map_err(|e| BackupError::Io(e.to_string()))?
                    }
                    Err(err) => Err(err),
                };
                let found = match found {
                    Ok(found) => found,
                    Err(err) => {
                        tracing::warn!(set = %set_id, error = %err, "a local set could not repair the bucket");
                        continue;
                    }
                };
                for (id, sealed) in found {
                    run.stats.uploaded_bytes += sealed.len() as u64;
                    self.s3.put(&self.chunk_key(&id), sealed, false).await?;
                    if self.fetch_chunk(run.keys, &id).await?.is_some() {
                        run.known.insert(id.clone());
                        restored.insert(id);
                        run.stats.repaired += 1;
                    }
                }
            }
        }
        Ok(restored)
    }
}

/// The settings message for a lifecycle rule that would expire chunks.
pub fn expiring_rule_message(rule: &str) -> String {
    format!(
        "The bucket's lifecycle rule \"{rule}\" deletes old objects under this prefix. \
         Incremental backups reuse old chunks, so that rule would break newer backups. \
         Remove the rule or limit it to another prefix."
    )
}

/// Read the chunks at `wanted` (offset in the stored stream, chunk) from a
/// local set's artifact and seal those whose bytes still match.
fn extract(
    set_dir: &Path,
    artifact: Option<super::Artifact>,
    identity: &age::x25519::Identity,
    encoding: Encoding,
    keys: &ChunkKeys,
    wanted: &[(u64, ChunkRef)],
) -> Result<Vec<(String, Vec<u8>)>, BackupError> {
    use std::io::Read;
    let artifact =
        artifact.ok_or_else(|| BackupError::Invalid("the local set lacks the artifact".into()))?;
    chunks::read_stored(set_dir, &artifact, identity, encoding, |input| {
        let mut position = 0u64;
        let mut found = Vec::new();
        for (offset, chunk) in wanted {
            let skip = offset - position;
            std::io::copy(&mut input.take(skip), &mut std::io::sink())
                .map_err(|e| BackupError::Io(e.to_string()))?;
            let mut bytes = vec![0u8; chunk.bytes as usize];
            input
                .read_exact(&mut bytes)
                .map_err(|e| BackupError::Io(e.to_string()))?;
            position = offset + chunk.bytes as u64;
            if keys.id(&bytes) == chunk.id {
                found.push((chunk.id.clone(), keys.seal(&bytes)?));
            }
        }
        Ok(found)
    })
}

/// Fetch and open the index a chunked completion names.
async fn open_index(
    s3: &S3,
    set_id: &str,
    completion: &Completion,
    key: &SecretKey,
) -> Result<Index, BackupError> {
    if completion.manifest.ciphertext_bytes > chunks::MAX_INDEX_BYTES + 64 * 1024 {
        return Err(BackupError::Invalid(
            "the remote chunk index is too large".into(),
        ));
    }
    let sealed = s3
        .get_bytes(
            &s3.key(&format!("sets/{set_id}/{INDEX}")),
            completion.manifest.ciphertext_bytes,
        )
        .await?;
    if super::sha256_hex(&sealed) != completion.manifest.ciphertext_sha256 {
        return Err(BackupError::Invalid(
            "the remote chunk index does not match its completion record".into(),
        ));
    }
    let identity = super::identity_for(key, &completion.encryption)?;
    let plain = super::open_small(&sealed, &identity, chunks::MAX_INDEX_BYTES)?;
    Index::parse(&plain, completion)
}

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
    if !super::valid_set_id(set_id) {
        return Err(BackupError::Invalid("not a set ID".into()));
    }
    super::ensure_private_dir(parent)?;
    let dir = parent.join(set_id);
    super::private_dir(&dir)?;
    let result = async {
        let complete = s3
            .get_bytes(
                &s3.key(&format!("sets/{set_id}/{COMPLETE}")),
                super::MAX_COMPLETE_BYTES,
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
    let identity = super::identity_for(key, &completion.encryption)?;
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
            tokio::task::spawn_blocking(move || {
                let chunks = std::iter::from_fn(|| rx.blocking_recv());
                chunks::rebuild(&target, &identity, &stored, chunks, max_bytes)
            })
        };
        let fetch = async {
            // Owned here, so the rebuild sees the end when fetching stops.
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
        let sealed = sealed.map_err(|e| BackupError::Io(e.to_string()))??;
        let rebuilt = super::Artifact {
            ciphertext_bytes: sealed.ciphertext_bytes,
            ciphertext_sha256: sealed.ciphertext_sha256,
            plaintext_bytes: sealed.plaintext_bytes,
            plaintext_sha256: sealed.plaintext_sha256,
            ..artifact.clone()
        };
        if stored.encoding == Encoding::Plain
            && (rebuilt.plaintext_bytes, &rebuilt.plaintext_sha256)
                != (artifact.plaintext_bytes, &artifact.plaintext_sha256)
        {
            return Err(BackupError::Invalid(format!(
                "the rebuilt {} does not match the set's manifest",
                artifact.object
            )));
        }
        artifacts.push(rebuilt);
    }
    let manifest = super::Manifest {
        artifacts,
        ..index.manifest
    };
    let json = serde_json::to_vec(&manifest).expect("serializable");
    let sealed = super::seal(
        &dir.join(MANIFEST),
        &identity,
        &mut json.as_slice(),
        super::MAX_MANIFEST_BYTES,
    )?;
    let local = Completion {
        manifest: ObjectRef {
            object: MANIFEST.into(),
            ciphertext_bytes: sealed.ciphertext_bytes,
            ciphertext_sha256: sealed.ciphertext_sha256,
        },
        ..completion.clone()
    };
    write_new(
        &dir.join(COMPLETE),
        &serde_json::to_vec_pretty(&local).expect("serializable"),
    )
}

async fn fetch_for_restore(
    s3: &S3,
    keys: &Arc<ChunkKeys>,
    chunk: &ChunkRef,
) -> Result<Vec<u8>, BackupError> {
    let sealed = s3
        .get_bytes(
            &s3.key(&format!("{CHUNKS}{}", chunk.id)),
            chunks::MAX_SEALED_CHUNK,
        )
        .await?;
    let (keys, id) = (keys.clone(), chunk.id.clone());
    let plain = tokio::task::spawn_blocking(move || keys.open(&id, &sealed))
        .await
        .map_err(|e| BackupError::Io(e.to_string()))??;
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
    if completion.manifest.ciphertext_bytes > super::MAX_MANIFEST_BYTES + 64 * 1024 {
        return Err(BackupError::Invalid(
            "the remote manifest is too large".into(),
        ));
    }
    write_new(&dir.join(COMPLETE), complete)?;
    fetch(
        s3,
        &set_key(MANIFEST),
        completion.manifest.ciphertext_bytes,
        &dir.join(MANIFEST),
    )
    .await?;
    let (_, manifest, _) = super::restore::open_set(dir, key)?;
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

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), BackupError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| BackupError::io(path.display(), e))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| BackupError::io(path.display(), e))
}
