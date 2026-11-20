//! Replication of completed local sets to an S3 or R2 bucket.
//!
//! Remote layout: `<prefix>sets/<set id>/complete.json` and
//! `<prefix>sets/<set id>/index.json.age` per set, and the chunks every set
//! shares at `<prefix>chunks/<key id>/<id>` (see [`super::chunks`]). For each
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

use super::chunks::{self, CHUNKS, ChunkKeys, ChunkRef, Encoding, INDEX, Index};
use super::s3::{Put, S3, S3Error};
use super::{BackupError, COMPLETE, Completion, MANIFEST, runs};
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

/// `backup_remote.state`: a set's replication to one destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum RemoteState {
    Completed,
    Failed,
    /// Removed from the bucket by retention.
    Retired,
}

/// How replicating one set ended.
enum Recorded<'a> {
    Completed { whole: bool, uploaded: u64 },
    Failed(&'a BackupError),
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

    fn chunk_key(&self, keys: &ChunkKeys, id: &str) -> String {
        self.s3.key(&format!("{}{id}", keys.root()))
    }

    /// Record how replicating `set_id` ended, here and on its local run,
    /// in one transaction. A retired set stays retired: its remote objects
    /// are gone.
    async fn record(&self, set_id: &str, recorded: Recorded<'_>) -> Result<(), BackupError> {
        let (state, outcome, error, whole_assistant, uploaded_bytes) = match recorded {
            Recorded::Completed { whole, uploaded } => (
                RemoteState::Completed,
                runs::RemoteOutcome::Completed,
                None,
                whole,
                Some(i64::try_from(uploaded).unwrap_or(i64::MAX)),
            ),
            Recorded::Failed(err) => (
                RemoteState::Failed,
                runs::RemoteOutcome::Failed,
                Some(err.to_string().chars().take(500).collect::<String>()),
                false,
                None,
            ),
        };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let moved = sqlx::query(
            "INSERT INTO backup_remote (destination, set_id, state, error, whole_assistant,
                 uploaded_bytes, updated_at, verified_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), CASE WHEN ?3 = 'completed' THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') END)
             ON CONFLICT (destination, set_id) DO UPDATE SET state = ?3, error = ?4,
                 whole_assistant = ?5, uploaded_bytes = COALESCE(?6, backup_remote.uploaded_bytes),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 verified_at = CASE WHEN ?3 = 'completed' THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                                    ELSE backup_remote.verified_at END
             WHERE backup_remote.state <> ?7",
        )
        .bind(self.s3.destination())
        .bind(set_id)
        .bind(state)
        .bind(&error)
        .bind(whole_assistant)
        .bind(uploaded_bytes)
        .bind(RemoteState::Retired)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if moved == 0 {
            tracing::warn!(set = %set_id, "a retired remote set was not recorded again");
            return Ok(());
        }
        runs::remote(&mut tx, set_id, outcome).await?;
        tx.commit().await?;
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
            run.known = self.chunk_listing(run.keys).await?.into_keys().collect();
            self.check_sample(&mut run).await
        }
        .await;
        if let Err(err) = prepared {
            for set_id in &awaiting {
                self.record(set_id, Recorded::Failed(&err)).await?;
            }
            return Err(err);
        }
        let mut first_error = None;
        for set_id in awaiting {
            let before = run.stats.uploaded_bytes;
            match self.replicate_set(&dir.join(&set_id), key, &mut run).await {
                Ok(whole) => {
                    let uploaded = run.stats.uploaded_bytes - before;
                    self.record(&set_id, Recorded::Completed { whole, uploaded })
                        .await?;
                    run.stats.completed.push(set_id);
                }
                Err(err) => {
                    tracing::warn!(set = %set_id, error = %err, "replication failed");
                    self.record(&set_id, Recorded::Failed(&err)).await?;
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
    async fn chunk_listing(
        &self,
        keys: &ChunkKeys,
    ) -> Result<HashMap<String, super::s3::Object>, BackupError> {
        let root = self.s3.key(&keys.root());
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
            .get_bytes(&self.chunk_key(keys, id), chunks::MAX_SEALED_CHUNK)
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
            self.s3.delete(&self.chunk_key(run.keys, &id)).await?;
            run.known.remove(&id);
            run.stats.damaged += 1;
        }
        Ok(())
    }

    /// Whether `set_id` is published. Only a record with the local set's
    /// encryption (its key and fresh salt) is ours: one replicated whole by
    /// an earlier version counts as published, and a chunked one counts once
    /// its index opens. A record of ours whose index is damaged is withdrawn,
    /// so the set uploads again. Another record is a conflict.
    async fn published(&self, local: &Completion, key: &SecretKey) -> Result<bool, BackupError> {
        let set_id = &local.set_id;
        let complete_key = self.set_key(set_id, COMPLETE);
        if self.s3.head(&complete_key).await?.is_none() {
            return Ok(false);
        }
        let remote = self
            .s3
            .get_bytes(&complete_key, super::MAX_COMPLETE_BYTES)
            .await?;
        let remote = serde_json::from_slice::<Completion>(&remote)
            .ok()
            .filter(|r| r.set_id == *set_id && r.encryption == local.encryption)
            .ok_or_else(|| {
                BackupError::Refused(format!(
                    "the remote set {set_id} has a different completion record; it is never replaced"
                ))
            })?;
        if remote.manifest.object != INDEX {
            return Ok(true);
        }
        match open_index(&self.s3, set_id, &remote, key).await {
            Ok(_) => Ok(true),
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

    /// Publish a local set: its missing chunks as the store holds them, then
    /// its own index and completion record.
    async fn replicate_set(
        &self,
        set_dir: &Path,
        key: &SecretKey,
        run: &mut Upload<'_>,
    ) -> Result<bool, BackupError> {
        let local_complete =
            super::read_regular(&set_dir.join(COMPLETE), super::MAX_COMPLETE_BYTES)?;
        let (completion, index) = super::store::open_index(set_dir, key)?;
        let set_id = completion.set_id.clone();
        let whole = index.manifest.coverage.whole_assistant;
        if self.published(&completion, key).await? {
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
        let before = run.known.clone();
        if let Err(err) = self.upload_chunks(&set_id, set_dir, &index, run).await {
            run.known = before;
            return Err(err);
        }
        let sealed_index =
            super::read_regular(&set_dir.join(INDEX), chunks::MAX_INDEX_BYTES + 64 * 1024)?;
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
        let complete_key = self.set_key(&set_id, COMPLETE);
        run.stats.uploaded_bytes += local_complete.len() as u64;
        if self
            .s3
            .put(&complete_key, local_complete.clone(), true)
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
        if read_back != local_complete {
            return Err(BackupError::Invalid(
                "the published completion record differs".into(),
            ));
        }
        Ok(whole)
    }

    /// Check the chunks this set uploaded by readback before it publishes.
    /// A chunk that fails is deleted, so the next run uploads it again.
    async fn verify_fresh(
        &self,
        set_id: &str,
        fresh: Vec<String>,
        run: &mut Upload<'_>,
    ) -> Result<(), BackupError> {
        if let Some(hook) = &self.before_publish {
            hook(set_id.to_owned()).await;
        }
        let failed = self.unreadable(run.keys, fresh).await?;
        if let Some(id) = failed.first() {
            for id in &failed {
                self.s3.delete(&self.chunk_key(run.keys, id)).await?;
            }
            return Err(BackupError::Invalid(format!(
                "the remote copy of chunk {id} does not match; the set is not complete"
            )));
        }
        Ok(())
    }

    /// Upload the chunks of `index` the bucket does not hold yet, as the
    /// local store holds them, and verify them by readback.
    async fn upload_chunks(
        &self,
        set_id: &str,
        set_dir: &Path,
        index: &Index,
        run: &mut Upload<'_>,
    ) -> Result<(), BackupError> {
        let backups = set_dir
            .parent()
            .ok_or_else(|| BackupError::Invalid("the set has no backup directory".into()))?;
        let store = Arc::new(super::store::Store::open(backups, run.keys));
        let mut wanted = Vec::new();
        let mut reused = 0;
        for id in index.chunk_ids() {
            if run.known.insert(id.to_owned()) {
                wanted.push(id.to_owned());
            } else {
                reused += 1;
            }
        }
        let keys = run.keys;
        let uploaded: Vec<(String, u64)> = futures_util::stream::iter(wanted)
            .map(|id| {
                let store = store.clone();
                async move {
                    let read = {
                        let id = id.clone();
                        tokio::task::spawn_blocking(move || store.sealed(&id))
                    };
                    let sealed = read.await.map_err(|e| BackupError::Io(e.to_string()))??;
                    let bytes = sealed.len() as u64;
                    self.s3
                        .put(&self.chunk_key(keys, &id), sealed, true)
                        .await?;
                    if crate::faults::hit("replication.after_chunk") {
                        return Err(BackupError::Interrupted);
                    }
                    Ok::<_, BackupError>((id, bytes))
                }
            })
            .buffer_unordered(PARALLEL)
            .try_collect()
            .await?;
        run.stats.uploaded += uploaded.len();
        run.stats.reused += reused;
        run.stats.uploaded_bytes += uploaded.iter().map(|(_, b)| b).sum::<u64>();
        let fresh = uploaded.into_iter().map(|(id, _)| id).collect();
        self.verify_fresh(set_id, fresh, run).await
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
                    "UPDATE backup_remote SET state = ?3, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                     WHERE destination = ?1 AND set_id = ?2 AND state = ?4",
                )
                .bind(self.s3.destination())
                .bind(id)
                .bind(RemoteState::Retired)
                .bind(RemoteState::Completed)
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
        self.sweep(dir, &indexes, run).await?;
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
        indexes: &[(String, Index)],
        run: &mut Upload<'_>,
    ) -> Result<(), BackupError> {
        let listing = self.chunk_listing(run.keys).await?;
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
        let restored = self.repair(dir, &missing, run).await?;
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

    /// Upload missing chunks again from the local chunk store.
    async fn repair(
        &self,
        dir: &Path,
        missing: &HashSet<String>,
        run: &mut Upload<'_>,
    ) -> Result<HashSet<String>, BackupError> {
        let store = super::store::Store::open(dir, run.keys);
        let mut restored = HashSet::new();
        for id in missing {
            let Ok(sealed) = store.sealed(id) else {
                continue;
            };
            run.stats.uploaded_bytes += sealed.len() as u64;
            self.s3
                .put(&self.chunk_key(run.keys, id), sealed, false)
                .await?;
            if self.fetch_chunk(run.keys, id).await?.is_some() {
                run.known.insert(id.clone());
                restored.insert(id.clone());
                run.stats.repaired += 1;
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
    super::write_new(&dir.join(COMPLETE), complete)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn replicator() -> Replicator {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let s3 = S3::new(super::super::s3::S3Config {
            endpoint: "http://127.0.0.1:9".into(),
            region: "auto".into(),
            bucket: "bucket".into(),
            prefix: String::new(),
            access_key_id: "id".into(),
            secret_access_key: String::from("secret").into(),
            path_style: true,
        })
        .unwrap();
        Replicator::new(s3, pool, 1)
    }

    async fn states(r: &Replicator, set: &str) -> (String, Option<String>) {
        sqlx::query_as(
            "SELECT r.state, b.remote_status FROM backup_remote r
             JOIN backup_runs b ON b.set_id = r.set_id WHERE r.set_id = ?1",
        )
        .bind(set)
        .fetch_one(&r.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_recorded_outcome_moves_both_rows_until_the_set_is_retired() {
        let r = replicator().await;
        let set = "20261121T000000000Z-0a1b2c3d";
        runs::start(&r.pool, set).await.unwrap();
        let failure = BackupError::Remote("down".into());
        r.record(set, Recorded::Failed(&failure)).await.unwrap();
        assert_eq!(
            states(&r, set).await,
            ("failed".into(), Some("failed".into()))
        );
        r.record(
            set,
            Recorded::Completed {
                whole: true,
                uploaded: 3,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            states(&r, set).await,
            ("completed".into(), Some("completed".into()))
        );
        sqlx::query("UPDATE backup_remote SET state = 'retired' WHERE set_id = ?1")
            .bind(set)
            .execute(&r.pool)
            .await
            .unwrap();
        r.record(set, Recorded::Failed(&failure)).await.unwrap();
        assert_eq!(
            states(&r, set).await,
            ("retired".into(), Some("completed".into())),
            "a retired set is never recorded again, and its run keeps its outcome"
        );
    }
}
