//! Capture one database backup set and publish it locally.
//!
//! The table digests and `pg_dump` share one exported snapshot, so the rows
//! the manifest describes are exactly the rows in the dump. The dump streams
//! from `pg_dump`'s stdout through age into a private staging directory; it
//! is never written in plaintext. The set becomes visible under its final
//! name only after every object and the encrypted manifest are synced, and
//! counts as a backup only once `complete.json` is linked in (never
//! replacing one). A failed producer, encryption error, size overrun, or
//! interruption leaves no completed set.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sqlx::PgPool;

use super::{
    Application, Artifact, BackupError, BotEntry, COMPLETE, Completion, DATABASE, MANIFEST,
    Manifest, ObjectRef, PgParams, Postgres, SCHEMA_VERSION, TableDigest,
};
use crate::credentials::SecretKey;

#[derive(Debug, Clone)]
pub struct BackupConfig {
    pub dir: PathBuf,
    pub pg_dump: PathBuf,
    pub pg_restore: PathBuf,
    /// Completed local sets kept (at least one).
    pub keep: usize,
    /// Largest dump accepted, in plaintext bytes; bounds temporary storage.
    pub max_bytes: u64,
    /// Time between scheduled sets; zero disables the schedule.
    pub interval: std::time::Duration,
}

#[derive(Debug, Clone)]
pub struct SetSummary {
    pub set_id: String,
    pub path: PathBuf,
    pub manifest: Manifest,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Row counts and content digests of every application table, in the
/// connection's current snapshot.
pub async fn table_digests(conn: &mut sqlx::PgConnection) -> Result<Vec<TableDigest>, sqlx::Error> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename::text FROM pg_tables WHERE schemaname = 'public' ORDER BY tablename",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut digests = Vec::new();
    for table in tables {
        // The name comes from the catalog; quote it as an identifier.
        let quoted = format!("\"{}\"", table.replace('"', "\"\""));
        let (rows, md5): (i64, String) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT count(*), coalesce(md5(string_agg(h, '' ORDER BY h)), '')
             FROM (SELECT md5(t::text) AS h FROM public.{quoted} t) rows"
        )))
        .fetch_one(&mut *conn)
        .await?;
        digests.push(TableDigest { table, rows, md5 });
    }
    Ok(digests)
}

fn dump_version(pg_dump: &Path) -> Result<String, BackupError> {
    let out = Command::new(pg_dump)
        .arg("--version")
        .env_clear()
        .output()
        .map_err(|e| BackupError::Producer(format!("{}: {e}", pg_dump.display())))?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Run `pg_dump` in the exported snapshot and seal its output. The child is
/// killed and reaped if encryption fails; its exit status must be success.
fn dump_and_seal(
    pg_dump: &Path,
    params: &PgParams,
    snapshot: &str,
    target: &Path,
    identity: &age::x25519::Identity,
    limit: u64,
) -> Result<super::Sealed, BackupError> {
    let mut child = Command::new(pg_dump)
        .args(["--format=custom", "--no-owner", "--no-acl"])
        .arg(format!("--snapshot={snapshot}"))
        .env_clear()
        .envs(params.env())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| BackupError::Producer(format!("{}: {e}", pg_dump.display())))?;
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let errors = std::thread::spawn(move || {
        let mut tail = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stderr.read(&mut buf) {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > 16 * 1024 {
                tail.drain(..tail.len() - 8 * 1024);
            }
        }
        String::from_utf8_lossy(&tail).trim().to_owned()
    });
    let sealed = super::seal(target, identity, &mut stdout, limit);
    if sealed.is_err() {
        let _ = child.kill();
    }
    drop(stdout);
    let status = child
        .wait()
        .map_err(|e| BackupError::Producer(e.to_string()))?;
    let errors = errors.join().unwrap_or_default();
    match sealed {
        Err(err) => Err(err),
        Ok(_) if !status.success() => {
            let _ = std::fs::remove_file(target);
            Err(BackupError::Producer(format!(
                "exited with {status}: {}",
                errors.lines().last().unwrap_or_default()
            )))
        }
        Ok(sealed) => Ok(sealed),
    }
}

/// Write `complete.json` without replacing an existing one: a synced
/// temporary file hard-linked into place, then the directory synced.
fn publish_completion(set_dir: &Path, completion: &Completion) -> Result<(), BackupError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let temp = set_dir.join(".complete.tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| BackupError::io(temp.display(), e))?;
    let bytes = serde_json::to_vec_pretty(completion).expect("serializable");
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| BackupError::io(temp.display(), e))?;
    drop(file);
    let linked = std::fs::hard_link(&temp, set_dir.join(COMPLETE));
    let _ = std::fs::remove_file(&temp);
    linked.map_err(|e| BackupError::io(set_dir.join(COMPLETE).display(), e))?;
    super::sync_dir(set_dir)
}

/// Capture, encrypt, and publish one database-only set.
pub async fn create_set(
    pool: &PgPool,
    params: &PgParams,
    key: &SecretKey,
    config: &BackupConfig,
    set_id: &str,
) -> Result<SetSummary, BackupError> {
    if !super::valid_set_id(set_id) {
        return Err(BackupError::Invalid("not a set ID".into()));
    }
    super::ensure_private_dir(&config.dir)?;
    let set_id = set_id.to_owned();
    let staging = super::staging_path(&config.dir, &set_id);
    super::private_dir(&staging)?;
    let result = capture_into(pool, params, key, config, &set_id, &staging).await;
    match result {
        Ok((manifest, completion)) => {
            let final_dir = config.dir.join(&set_id);
            let published = (|| {
                super::rename_noreplace(&staging, &final_dir)?;
                super::sync_dir(&config.dir)?;
                if crate::faults::hit("backup.before_complete") {
                    return Err(BackupError::Interrupted);
                }
                publish_completion(&final_dir, &completion)
            })();
            if let Err(err) = published {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(err);
            }
            Ok(SetSummary {
                set_id,
                path: final_dir,
                manifest,
            })
        }
        Err(err) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(err)
        }
    }
}

async fn capture_into(
    pool: &PgPool,
    params: &PgParams,
    key: &SecretKey,
    config: &BackupConfig,
    set_id: &str,
    staging: &Path,
) -> Result<(Manifest, Completion), BackupError> {
    let capture_started_at = now();
    let mut salt = [0u8; 32];
    getrandom::fill(&mut salt).map_err(|e| BackupError::Crypto(e.to_string()))?;
    let encryption = super::encryption_for(key, &salt);
    let identity = super::identity(key, &salt)?;

    // One snapshot for the digests and the dump; held open until the dump
    // has finished.
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await?;
    let snapshot: String = sqlx::query_scalar("SELECT pg_export_snapshot()")
        .fetch_one(&mut *tx)
        .await?;
    let tables = table_digests(&mut tx).await?;
    let migration_version: i64 =
        sqlx::query_scalar("SELECT coalesce(max(version), 0) FROM _sqlx_migrations WHERE success")
            .fetch_one(&mut *tx)
            .await?;
    let server_version: String = sqlx::query_scalar("SHOW server_version")
        .fetch_one(&mut *tx)
        .await?;
    let extensions: Vec<(String, String)> = sqlx::query_as(
        "SELECT extname::text, extversion FROM pg_extension WHERE extname <> 'plpgsql' ORDER BY 1",
    )
    .fetch_all(&mut *tx)
    .await?;
    let bots: Vec<(uuid::Uuid, String, String)> =
        sqlx::query_as("SELECT id, name, status FROM bots ORDER BY created_at, id")
            .fetch_all(&mut *tx)
            .await?;

    let pg_dump = config.pg_dump.clone();
    let dump_params = params.clone();
    let target = staging.join(DATABASE);
    let limit = config.max_bytes;
    let dump_identity = identity.clone();
    let sealed = tokio::task::spawn_blocking(move || {
        let version = dump_version(&pg_dump)?;
        dump_and_seal(
            &pg_dump,
            &dump_params,
            &snapshot,
            &target,
            &dump_identity,
            limit,
        )
        .map(|sealed| (sealed, version))
    })
    .await
    .map_err(|e| BackupError::Producer(e.to_string()))?;
    tx.commit().await?;
    let (sealed, dump_version) = sealed?;

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        set_id: set_id.to_owned(),
        created_at: now(),
        capture_started_at,
        capture_finished_at: now(),
        encryption: encryption.clone(),
        application: Application {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            migration_version,
        },
        postgres: Postgres {
            server_version,
            dump_version,
            format: "custom".into(),
            extensions,
        },
        artifacts: vec![Artifact {
            kind: "postgres".into(),
            object: DATABASE.into(),
            ciphertext_bytes: sealed.ciphertext_bytes,
            ciphertext_sha256: sealed.ciphertext_sha256,
            plaintext_bytes: sealed.plaintext_bytes,
            plaintext_sha256: sealed.plaintext_sha256,
        }],
        tables,
        bots: bots
            .into_iter()
            .map(|(id, name, status)| BotEntry {
                id,
                name,
                status,
                vm: "omitted".into(),
                provider_state: "omitted-reauth-required".into(),
            })
            .collect(),
        coverage: super::database_only_coverage(),
    };
    let json = serde_json::to_vec(&manifest).expect("serializable");
    let sealed_manifest = super::seal(
        &staging.join(MANIFEST),
        &identity,
        &mut json.as_slice(),
        super::MAX_MANIFEST_BYTES,
    )?;
    super::sync_dir(staging)?;
    let completion = Completion {
        schema_version: SCHEMA_VERSION,
        set_id: set_id.to_owned(),
        encryption,
        manifest: ObjectRef {
            object: MANIFEST.into(),
            ciphertext_bytes: sealed_manifest.ciphertext_bytes,
            ciphertext_sha256: sealed_manifest.ciphertext_sha256,
        },
    };
    Ok((manifest, completion))
}
