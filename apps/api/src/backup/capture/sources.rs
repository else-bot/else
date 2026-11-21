//! The checkpoint sources a capture seals besides the database: bot
//! computers, files, projects, and shared MCP packages.

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use crate::backup::{Artifact, ArtifactKind, BackupError, ProjectState, VmState};
use crate::db::bots::BotStatus;

async fn seal_file(
    plain: PathBuf,
    target: PathBuf,
    identity: age::x25519::Identity,
    limit: u64,
) -> Result<crate::backup::Sealed, BackupError> {
    crate::backup::blocking(move || {
        let result = std::fs::File::open(&plain)
            .map_err(|e| BackupError::io(plain.display(), e))
            .and_then(|mut file| crate::backup::seal(&target, &identity, &mut file, limit));
        let _ = std::fs::remove_file(&plain);
        result
    })
    .await
}

pub(super) async fn capture_computers(
    computers: &crate::computers::ComputerManager,
    bots: &[(uuid::Uuid, String, BotStatus)],
    staging: &Path,
    identity: &age::x25519::Identity,
    limit: u64,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Result<
    (
        Vec<Artifact>,
        std::collections::HashMap<uuid::Uuid, (VmState, Option<crate::backup::ComputerEntry>)>,
    ),
    BackupError,
> {
    use crate::computers::{ComputerError, DiskCapture};
    let mut artifacts = Vec::new();
    let mut entries = std::collections::HashMap::new();
    let mut carried = std::collections::HashSet::new();
    let total = bots
        .iter()
        .filter(|(_, _, status)| *status == BotStatus::Active)
        .count();
    let mut done = 0;
    progress(done, total);
    for (bot, name, status) in bots {
        if *status != BotStatus::Active {
            entries.insert(*bot, (VmState::OmittedDeletedBot, None));
            continue;
        }
        let object = format!("vm-{bot}.msb.age");
        let plain = staging.join(format!("vm-{bot}.msb"));
        let captured = computers.capture_disk(*bot, &plain, &carried).await;
        let captured = match captured {
            Ok(captured) => captured,
            Err(ComputerError::Busy(why)) => {
                let _ = std::fs::remove_file(&plain);
                return Err(BackupError::Deferred(format!(
                    "bot {name}'s computer is {why}"
                )));
            }
            Err(err) => {
                let _ = std::fs::remove_file(&plain);
                return Err(BackupError::Producer(format!(
                    "bot {name}'s computer: {err}"
                )));
            }
        };
        match captured {
            DiskCapture::NoComputer => {
                entries.insert(*bot, (VmState::None, None));
            }
            DiskCapture::Missing => {
                entries.insert(*bot, (VmState::Missing, None));
            }
            DiskCapture::Exported {
                runtime_name,
                image,
                cpus,
                memory_mib,
                desired_state,
                desktop_port,
                was_running,
                browser_checkpoint,
                project_mount,
                carries_image,
            } => {
                let sealed =
                    seal_file(plain, staging.join(&object), identity.clone(), limit).await?;
                if carries_image {
                    carried.insert(image.clone());
                }
                let password_file = computers.desktop_password_file(*bot);
                let desktop_password = std::fs::read_to_string(&password_file)
                    .ok()
                    .map(|p| p.trim().to_owned())
                    .filter(|p| !p.is_empty() && p.len() <= 64);
                if desktop_port.is_some() && desktop_password.is_none() {
                    return Err(BackupError::Producer(format!(
                        "bot {name}'s desktop password ({}) is missing or invalid",
                        password_file.display()
                    )));
                }
                artifacts.push(Artifact {
                    kind: ArtifactKind::Vm,
                    object: object.clone(),
                    bot_id: Some(*bot),
                    ciphertext_bytes: sealed.ciphertext_bytes,
                    ciphertext_sha256: sealed.ciphertext_sha256,
                    plaintext_bytes: sealed.plaintext_bytes,
                    plaintext_sha256: sealed.plaintext_sha256,
                });
                entries.insert(
                    *bot,
                    (
                        VmState::Included,
                        Some(crate::backup::ComputerEntry {
                            object,
                            runtime_name,
                            runtime_version: crate::computers::runtime::VERIFIED_VERSION.into(),
                            image,
                            cpus,
                            memory_mib,
                            desired_state,
                            network: computers.config().network.clone(),
                            desktop_port,
                            desktop_password,
                            was_running,
                            browser_checkpoint,
                            project_mount: project_mount.map(|m| crate::backup::ProjectMount {
                                guest: m.guest,
                                uid: m.uid,
                            }),
                            carries_image,
                        }),
                    ),
                );
            }
        }
        done += 1;
        progress(done, total);
    }
    Ok((artifacts, entries))
}

pub(super) async fn capture_files(
    pool: &SqlitePool,
    state_dir: &Path,
    staging: &Path,
    identity: &age::x25519::Identity,
    limit: u64,
) -> Result<(Vec<Artifact>, Vec<uuid::Uuid>), BackupError> {
    let rows: Vec<(uuid::Uuid, uuid::Uuid, i64, Vec<u8>)> =
        sqlx::query_as("SELECT id, bot_id, size_bytes, sha256 FROM artifacts ORDER BY id")
            .fetch_all(pool)
            .await?;
    let mut artifacts = Vec::new();
    let mut missing = Vec::new();
    for (id, bot, size, sha) in rows {
        let source = state_dir
            .join("artifacts")
            .join(bot.to_string())
            .join(id.to_string());
        let regular =
            std::fs::symlink_metadata(&source).is_ok_and(|m| m.is_file() && m.len() == size as u64);
        if !regular {
            missing.push(id);
            continue;
        }
        let object = format!("file-{id}.age");
        let target = staging.join(&object);
        let identity = identity.clone();
        let sealed = crate::backup::blocking(move || {
            let mut file =
                std::fs::File::open(&source).map_err(|e| BackupError::io(source.display(), e))?;
            crate::backup::seal(&target, &identity, &mut file, limit)
        })
        .await?;
        if sealed.plaintext_sha256 != hex::encode(&sha) {
            let _ = std::fs::remove_file(staging.join(&object));
            missing.push(id);
            continue;
        }
        artifacts.push(Artifact {
            kind: ArtifactKind::File,
            object,
            bot_id: Some(bot),
            ciphertext_bytes: sealed.ciphertext_bytes,
            ciphertext_sha256: sealed.ciphertext_sha256,
            plaintext_bytes: sealed.plaintext_bytes,
            plaintext_sha256: sealed.plaintext_sha256,
        });
    }
    Ok((artifacts, missing))
}

pub(super) async fn capture_projects(
    bots: &[(uuid::Uuid, String, BotStatus)],
    projects_dir: &Path,
    staging: &Path,
    identity: &age::x25519::Identity,
    limit: u64,
) -> Result<
    (
        Vec<Artifact>,
        std::collections::HashMap<uuid::Uuid, ProjectState>,
    ),
    BackupError,
> {
    let mut artifacts = Vec::new();
    let mut states = std::collections::HashMap::new();
    for (bot, name, status) in bots {
        if *status != BotStatus::Active {
            states.insert(*bot, ProjectState::OmittedDeletedBot);
            continue;
        }
        let dir = projects_dir.join(bot.to_string());
        if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
            states.insert(*bot, ProjectState::None);
            continue;
        }
        let object = crate::backup::projects::Format::TarZstd.object(*bot);
        let plain = staging.join(format!("project-{bot}.tar.zst"));
        let archive_target = plain.clone();
        let unreadable = crate::backup::blocking(move || {
            crate::backup::projects::archive(&dir, &archive_target)
        })
        .await
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&plain);
        })?;
        if !unreadable.is_empty() {
            tracing::warn!(bot = %name, entries = unreadable.len(), "project entries could not be read and are not in the set");
        }
        let sealed = seal_file(plain, staging.join(&object), identity.clone(), limit).await?;
        artifacts.push(Artifact {
            kind: ArtifactKind::Project,
            object,
            bot_id: Some(*bot),
            ciphertext_bytes: sealed.ciphertext_bytes,
            ciphertext_sha256: sealed.ciphertext_sha256,
            plaintext_bytes: sealed.plaintext_bytes,
            plaintext_sha256: sealed.plaintext_sha256,
        });
        states.insert(
            *bot,
            if unreadable.is_empty() {
                ProjectState::Included
            } else {
                ProjectState::Partial
            },
        );
    }
    Ok((artifacts, states))
}

pub(super) async fn capture_mcp(
    dir: PathBuf,
    staging: &Path,
    identity: &age::x25519::Identity,
    limit: u64,
) -> Result<Artifact, BackupError> {
    let plain = staging.join("mcp.tar");
    let archive_target = plain.clone();
    crate::backup::blocking(move || crate::backup::mcp::archive(&dir, &archive_target, limit))
        .await?;
    let sealed = seal_file(
        plain,
        staging.join(crate::backup::mcp::OBJECT),
        identity.clone(),
        limit,
    )
    .await?;
    Ok(Artifact {
        kind: ArtifactKind::Mcp,
        object: crate::backup::mcp::OBJECT.into(),
        bot_id: None,
        ciphertext_bytes: sealed.ciphertext_bytes,
        ciphertext_sha256: sealed.ciphertext_sha256,
        plaintext_bytes: sealed.plaintext_bytes,
        plaintext_sha256: sealed.plaintext_sha256,
    })
}
