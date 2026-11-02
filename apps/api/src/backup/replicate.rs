//! Replication of completed local sets to an S3 or R2 bucket (POC-023).
//!
//! Remote layout: `<prefix>sets/<set id>/<object>`. For each completed local
//! set without a verified remote completion, the worker aborts abandoned
//! multipart uploads of that set, uploads every encrypted object (skipping
//! one whose remote bytes already hash to the recorded SHA-256), verifies
//! every object again by streamed readback and SHA-256, and only then
//! publishes `complete.json` with `If-None-Match: *`, reading it back. A set
//! whose remote completion exists with other contents is a conflict and is
//! never replaced. Local and remote outcomes are recorded separately.
//! Remote retention keeps the newest complete sets and removes a set's
//! completion record before its objects. All of it runs under the worker's
//! backup lock.

use std::path::{Path, PathBuf};

use sqlx::PgPool;

use super::s3::{Put, S3, S3Error};
use super::{BackupError, COMPLETE, Completion, MANIFEST};
use crate::credentials::SecretKey;

/// Objects at or below this size are uploaded whole.
const PART_BYTES: u64 = 16 * 1024 * 1024;

impl From<S3Error> for BackupError {
    fn from(err: S3Error) -> Self {
        BackupError::Remote(err.to_string())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub uploaded: usize,
    pub skipped: usize,
    pub aborted: usize,
    pub completed: Vec<String>,
    pub removed: Vec<String>,
}

/// A test hook run after the uploads and before final verification.
pub type BeforePublish =
    std::sync::Arc<dyn Fn(String) -> futures_util::future::BoxFuture<'static, ()> + Send + Sync>;

#[derive(Clone)]
pub struct Replicator {
    pub s3: S3,
    pool: PgPool,
    /// Completed remote sets kept (at least one).
    pub keep: usize,
    /// Objects above this size are uploaded in parts of this size.
    pub part_bytes: u64,
    pub before_publish: Option<BeforePublish>,
}

impl Replicator {
    pub fn new(s3: S3, pool: PgPool, keep: usize) -> Self {
        Self {
            s3,
            pool,
            keep,
            part_bytes: PART_BYTES,
            before_publish: None,
        }
    }

    fn set_key(&self, set_id: &str, object: &str) -> String {
        self.s3.key(&format!("sets/{set_id}/{object}"))
    }

    async fn record(
        &self,
        set_id: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), BackupError> {
        let error: Option<String> = error.map(|e| e.chars().take(500).collect());
        sqlx::query(
            "INSERT INTO backup_remote (set_id, state, error, updated_at,
                 verified_at)
             VALUES ($1, $2, $3, now(), CASE WHEN $2 = 'completed' THEN now() END)
             ON CONFLICT (set_id) DO UPDATE SET state = $2, error = $3, updated_at = now(),
                 verified_at = CASE WHEN $2 = 'completed' THEN now() ELSE backup_remote.verified_at END",
        )
        .bind(set_id)
        .bind(state)
        .bind(&error)
        .execute(&self.pool)
        .await?;
        sqlx::query("UPDATE backup_runs SET remote_status = $2 WHERE set_id = $1")
            .bind(set_id)
            .bind(state)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Local sets whose remote completion is not verified.
    pub async fn awaiting(&self, dir: &Path) -> Result<Vec<String>, BackupError> {
        let done: Vec<String> =
            sqlx::query_scalar("SELECT set_id FROM backup_remote WHERE state = 'completed'")
                .fetch_all(&self.pool)
                .await?;
        Ok(super::completed_sets(dir)?
            .into_iter()
            .filter(|s| !done.contains(s))
            .collect())
    }

    /// Replicate every awaiting local set (oldest first), then apply remote
    /// retention. A failing set is recorded and the error returned after
    /// the others were tried.
    pub async fn replicate_pending(
        &self,
        dir: &Path,
        key: &SecretKey,
    ) -> Result<Stats, BackupError> {
        let mut stats = Stats::default();
        let mut first_error = None;
        for set_id in self.awaiting(dir).await? {
            match self
                .replicate_set(&dir.join(&set_id), key, &mut stats)
                .await
            {
                Ok(()) => {
                    self.record(&set_id, "completed", None).await?;
                    stats.completed.push(set_id);
                }
                Err(err) => {
                    tracing::warn!(set = %set_id, error = %err, "replication failed");
                    self.record(&set_id, "failed", Some(&err.to_string()))
                        .await?;
                    first_error.get_or_insert(err);
                }
            }
        }
        if let Some(err) = first_error {
            return Err(err);
        }
        stats.removed = self.apply_remote_retention(dir).await?;
        Ok(stats)
    }

    async fn verify(&self, key: &str, bytes: u64, sha256: &str) -> Result<bool, BackupError> {
        match self.s3.head(key).await? {
            Some(len) if len == bytes => {}
            _ => return Ok(false),
        }
        let (len, digest) = self.s3.get_with(key, bytes, |_| Ok(())).await?;
        Ok(len == bytes && digest == sha256)
    }

    async fn upload(&self, key: &str, path: &Path, bytes: u64) -> Result<(), BackupError> {
        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|e| BackupError::io(path.display(), e))?;
        if bytes <= self.part_bytes {
            let mut body = Vec::with_capacity(bytes as usize);
            file.read_to_end(&mut body)
                .await
                .map_err(|e| BackupError::io(path.display(), e))?;
            self.s3.put(key, body, false).await?;
            return Ok(());
        }
        let upload = self.s3.create_multipart(key).await?;
        let mut parts = Vec::new();
        let result = async {
            let mut number = 1u32;
            loop {
                let mut part = Vec::with_capacity(self.part_bytes as usize);
                (&mut file)
                    .take(self.part_bytes)
                    .read_to_end(&mut part)
                    .await
                    .map_err(|e| BackupError::io(path.display(), e))?;
                if part.is_empty() {
                    break;
                }
                let etag = self.s3.upload_part(key, &upload, number, part).await?;
                parts.push((number, etag));
                if crate::faults::hit("replication.after_part") {
                    // A simulated crash: the multipart upload stays open.
                    return Err(BackupError::Interrupted);
                }
                number += 1;
            }
            self.s3.complete_multipart(key, &upload, &parts).await?;
            Ok(())
        }
        .await;
        if let Err(err) = &result
            && !matches!(err, BackupError::Interrupted)
        {
            let _ = self.s3.abort_multipart(key, &upload).await;
        }
        result
    }

    async fn replicate_set(
        &self,
        set_dir: &Path,
        key: &SecretKey,
        stats: &mut Stats,
    ) -> Result<(), BackupError> {
        let set_id = set_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_owned();
        let local_complete =
            super::read_regular(&set_dir.join(COMPLETE), super::MAX_COMPLETE_BYTES)?;
        let complete_key = self.set_key(&set_id, COMPLETE);
        // Already published: identical is done, different is a conflict.
        if self.s3.head(&complete_key).await?.is_some() {
            let remote = self
                .s3
                .get_bytes(&complete_key, super::MAX_COMPLETE_BYTES)
                .await?;
            return if remote == local_complete {
                Ok(())
            } else {
                Err(BackupError::Refused(format!(
                    "the remote set {set_id} has a different completion record; it is never replaced"
                )))
            };
        }
        // Validates the set and lists its objects.
        let (completion, manifest, _) = super::restore::open_set(set_dir, key)?;
        let mut objects: Vec<(String, u64, String)> = vec![(
            MANIFEST.into(),
            completion.manifest.ciphertext_bytes,
            completion.manifest.ciphertext_sha256.clone(),
        )];
        objects.extend(manifest.artifacts.iter().map(|a| {
            (
                a.object.clone(),
                a.ciphertext_bytes,
                a.ciphertext_sha256.clone(),
            )
        }));
        // A crashed earlier attempt may have left multipart uploads.
        for (upload_key, upload_id) in self.s3.list_multipart(&self.set_key(&set_id, "")).await? {
            self.s3.abort_multipart(&upload_key, &upload_id).await?;
            stats.aborted += 1;
        }
        for (object, bytes, sha256) in &objects {
            let remote = self.set_key(&set_id, object);
            if self.verify(&remote, *bytes, sha256).await? {
                stats.skipped += 1;
                continue;
            }
            self.upload(&remote, &set_dir.join(object), *bytes).await?;
            stats.uploaded += 1;
        }
        if let Some(hook) = &self.before_publish {
            hook(set_id.clone()).await;
        }
        // Every byte is checked again right before publication.
        for (object, bytes, sha256) in &objects {
            if !self
                .verify(&self.set_key(&set_id, object), *bytes, sha256)
                .await?
            {
                return Err(BackupError::Invalid(format!(
                    "the remote copy of {object} does not match its SHA-256; the set is not complete"
                )));
            }
        }
        match self
            .s3
            .put(&complete_key, local_complete.clone(), true)
            .await?
        {
            Put::Created => {}
            Put::Exists => {
                let remote = self
                    .s3
                    .get_bytes(&complete_key, super::MAX_COMPLETE_BYTES)
                    .await?;
                if remote != local_complete {
                    return Err(BackupError::Refused(format!(
                        "the remote set {set_id} was completed with other contents"
                    )));
                }
            }
        }
        let published = self
            .s3
            .get_bytes(&complete_key, super::MAX_COMPLETE_BYTES)
            .await?;
        if published != local_complete {
            return Err(BackupError::Invalid(
                "the published completion record differs".into(),
            ));
        }
        Ok(())
    }

    /// Remote sets: (set id, has completion, every key).
    async fn remote_sets(&self) -> Result<Vec<(String, bool, Vec<String>)>, BackupError> {
        let root = self.s3.key("sets/");
        let mut sets: std::collections::BTreeMap<String, (bool, Vec<String>)> = Default::default();
        for (key, _) in self.s3.list(&root).await? {
            let Some(rest) = key.strip_prefix(&root) else {
                continue;
            };
            let Some((set, object)) = rest.split_once('/') else {
                continue;
            };
            if !super::valid_set_id(set) {
                continue;
            }
            let entry = sets.entry(set.to_owned()).or_default();
            entry.0 |= object == COMPLETE;
            entry.1.push(key);
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

    /// Keep the newest `keep` complete remote sets (never fewer than one);
    /// remove older ones completion record first. Incomplete remote sets are
    /// removed only when no local set awaits them.
    async fn apply_remote_retention(&self, dir: &Path) -> Result<Vec<String>, BackupError> {
        let sets = self.remote_sets().await?;
        let complete: Vec<&String> = sets.iter().filter(|s| s.1).map(|s| &s.0).collect();
        let keep = self.keep.max(1);
        let old: Vec<String> = if complete.len() > keep {
            complete[..complete.len() - keep]
                .iter()
                .map(|s| (*s).clone())
                .collect()
        } else {
            Vec::new()
        };
        let local = super::completed_sets(dir)?;
        let mut removed = Vec::new();
        for (id, has_complete, keys) in &sets {
            let abandoned = !has_complete && !local.contains(id);
            if !old.contains(id) && !abandoned {
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
            removed.push(id.clone());
        }
        Ok(removed)
    }

    /// Download a completed remote set into a new private directory under
    /// `parent`, fetching only the objects its decrypted manifest names.
    /// The result is checked again by the restore.
    pub async fn download_set(
        &self,
        set_id: &str,
        parent: &Path,
        key: &SecretKey,
    ) -> Result<PathBuf, BackupError> {
        if !super::valid_set_id(set_id) {
            return Err(BackupError::Invalid("not a set ID".into()));
        }
        super::ensure_private_dir(parent)?;
        let dir = parent.join(set_id);
        super::private_dir(&dir)?;
        let result = async {
            let complete = self
                .s3
                .get_bytes(&self.set_key(set_id, COMPLETE), super::MAX_COMPLETE_BYTES)
                .await
                .map_err(|e| match e {
                    S3Error::Status { status: 404, .. } => {
                        BackupError::Invalid(format!("the remote set {set_id} is not complete"))
                    }
                    other => other.into(),
                })?;
            let completion: Completion = serde_json::from_slice(&complete)
                .map_err(|e| BackupError::Invalid(format!("remote completion record: {e}")))?;
            if completion.set_id != set_id || completion.manifest.object != MANIFEST {
                return Err(BackupError::Invalid(
                    "the remote completion record names another set".into(),
                ));
            }
            write_new(&dir.join(COMPLETE), &complete)?;
            self.fetch(set_id, MANIFEST, completion.manifest.ciphertext_bytes, &dir)
                .await?;
            let (_, manifest, _) = super::restore::open_set(&dir, key)?;
            for artifact in &manifest.artifacts {
                self.fetch(set_id, &artifact.object, artifact.ciphertext_bytes, &dir)
                    .await?;
            }
            Ok(())
        }
        .await;
        if let Err(err) = result {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(err);
        }
        Ok(dir)
    }

    async fn fetch(
        &self,
        set_id: &str,
        object: &str,
        bytes: u64,
        dir: &Path,
    ) -> Result<(), BackupError> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = dir.join(object);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| BackupError::io(path.display(), e))?;
        self.s3
            .get_with(&self.set_key(set_id, object), bytes, |piece| {
                file.write_all(piece)
                    .map_err(|e| S3Error::Protocol(e.to_string()))
            })
            .await?;
        file.sync_all()
            .map_err(|e| BackupError::io(path.display(), e))
    }
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
