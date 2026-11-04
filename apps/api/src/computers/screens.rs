//! Desktop screenshots for the model (POC-028).
//!
//! A screenshot is the whole guest framebuffer (Xvfb display `:99`, the one
//! the owner sees through VNC), captured inside the guest by
//! `else-screenshot` as a PNG within an image budget separate from text
//! limits, copied out under the computer's lease and operation lock,
//! verified (PNG signature, header size, SHA-256), and handed to the tool
//! layer as bytes for an MCP image content block. Nothing about it is
//! logged but its metadata. When the bot asks to keep it, it is stored in
//! the bot's artifact store (`guest_path` `screen:<id>`), which encrypted
//! backups already cover; otherwise no copy remains on the host or guest.

use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{ComputerError, ComputerManager};

/// The largest PNG handed to a model (about 2 MB as base64, well inside the
/// providers' 8 MiB event lines).
pub const MAX_IMAGE_BYTES: u64 = 1_500_000;
/// Where the desktop user writes a capture.
const GUEST_DIR: &str = "/var/lib/elsebot/screens";
/// Where root keeps the checked copy that is copied out (root only).
const ROOT_DIR: &str = "/var/lib/elsebot-screens";

#[derive(Debug, Clone, Serialize)]
pub struct ScreenMeta {
    pub capture_id: Uuid,
    pub screen_width: u32,
    pub screen_height: u32,
    pub image_width: u32,
    pub image_height: u32,
    /// Screen pixels per image pixel (1 unless reduced to fit the budget).
    pub scale: u32,
    /// Pointer position in screen pixels.
    pub pointer: [i32; 2],
    pub bytes: u64,
    pub sha256: String,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub artifact_id: Option<Uuid>,
}

pub struct Screenshot {
    pub meta: ScreenMeta,
    pub png: Vec<u8>,
}

/// Why no screenshot came back (an explicit outcome for the tool).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenRefusal {
    /// The turn that asked is no longer running.
    Stopped,
    DisplayUnavailable(String),
    TooLarge(String),
    Invalid(String),
    NoSuchArtifact,
}

/// The width and height a PNG's header declares, if it is a PNG.
pub fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || &png[..8] != b"\x89PNG\r\n\x1a\n" || &png[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(png[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(png[20..24].try_into().ok()?);
    Some((w, h))
}

impl ComputerManager {
    /// Capture the bot's desktop under `lease` (its own computer: a lease of
    /// another bot is `LeaseEnded`) for `turn_id`, which must still run once
    /// the operation lock is held.
    ///
    /// The guest user writes the PNG; root then moves it, bounded by the
    /// budget and refusing links and non-files, into a directory only root
    /// can write, so the file copied out is exactly what was checked. Each
    /// capture first clears what an interrupted one left (guest and host),
    /// and every exit removes its own files.
    pub async fn capture_screen(
        &self,
        lease: Uuid,
        bot_id: Uuid,
        turn_id: Option<Uuid>,
        save: bool,
    ) -> Result<Result<Screenshot, ScreenRefusal>, ComputerError> {
        if !self.config.desktop {
            return Ok(Err(ScreenRefusal::DisplayUnavailable(
                "this installation's computers have no desktop".into(),
            )));
        }
        let (lease_bot, name, op) = self.checked(lease).await?;
        if lease_bot != bot_id {
            return Err(ComputerError::LeaseEnded);
        }
        if let Some(turn) = turn_id {
            let live: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM turns WHERE id = ?1
                                AND status IN ('preparing', 'sending', 'running'))",
            )
            .bind(turn)
            .fetch_one(&self.pool)
            .await?;
            if !live {
                return Ok(Err(ScreenRefusal::Stopped));
            }
        }
        let staging = self
            .config
            .state_dir
            .join("screens")
            .join(bot_id.to_string());
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&staging)
                .map_err(|e| ComputerError::Refused(format!("screenshot staging: {e}")))?;
        }
        // What an interrupted capture left on the host.
        if let Ok(entries) = std::fs::read_dir(&staging) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let capture_id = Uuid::new_v4();
        let user_path = format!("{GUEST_DIR}/{capture_id}.png");
        let root_path = format!("{ROOT_DIR}/{capture_id}.png");
        let result = self
            .capture_in_guest(&name, &user_path, &root_path, &staging, capture_id)
            .await;
        // Always: nothing of this capture stays in the guest.
        let _ = self
            .runtime
            .exec(
                &name,
                "/bin/rm",
                &["-f", "--", &user_path, &root_path],
                Duration::from_secs(30),
            )
            .await;
        drop(op);
        let temp = staging.join(format!(".capture-{capture_id}"));
        let (reported, png) = match result {
            Ok(Ok(found)) => found,
            Ok(Err(refusal)) => {
                let _ = std::fs::remove_file(&temp);
                let _ = std::fs::remove_dir_all(&temp);
                return Ok(Err(refusal));
            }
            Err(err) => {
                let _ = std::fs::remove_file(&temp);
                let _ = std::fs::remove_dir_all(&temp);
                return Err(err);
            }
        };
        let field = |k: &str| reported[k].as_u64().unwrap_or(0) as u32;
        let captured_at = reported["captured_at"]
            .as_f64()
            .and_then(|t| chrono::DateTime::from_timestamp_millis((t * 1000.0) as i64))
            .unwrap_or_else(chrono::Utc::now);
        let pointer = &reported["pointer"];
        let mut meta = ScreenMeta {
            capture_id,
            screen_width: field("screen_width"),
            screen_height: field("screen_height"),
            image_width: field("image_width"),
            image_height: field("image_height"),
            scale: field("scale").max(1),
            pointer: [
                pointer[0].as_i64().unwrap_or(0) as i32,
                pointer[1].as_i64().unwrap_or(0) as i32,
            ],
            bytes: png.len() as u64,
            sha256: hex::encode(Sha256::digest(&png)),
            captured_at,
            artifact_id: None,
        };
        if save {
            match self
                .save_screen(bot_id, turn_id, &temp, &png, &mut meta)
                .await
            {
                Ok(()) => {}
                Err(err) => {
                    let _ = std::fs::remove_file(&temp);
                    return Err(err);
                }
            }
        } else {
            let _ = std::fs::remove_file(&temp);
        }
        Ok(Ok(Screenshot { meta, png }))
    }

    /// The guest steps of a capture, under the caller's operation lock.
    async fn capture_in_guest(
        &self,
        name: &str,
        user_path: &str,
        root_path: &str,
        staging: &std::path::Path,
        capture_id: Uuid,
    ) -> Result<Result<(serde_json::Value, Vec<u8>), ScreenRefusal>, ComputerError> {
        let (program, mut args) = self.as_guest_user(
            r#"d=$(dirname "?1"); mkdir -p "$d" && find "$d" -mindepth 1 -delete 2>/dev/null; DISPLAY=:99 exec /usr/local/bin/else-screenshot "?1" "?2""#,
        );
        args.push(user_path.to_owned());
        args.push(MAX_IMAGE_BYTES.to_string());
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self
            .runtime
            .exec(name, &program, &argv, Duration::from_secs(60))
            .await?;
        let reported: serde_json::Value =
            serde_json::from_str(out.stdout.trim()).unwrap_or(serde_json::Value::Null);
        if let Some(error) = reported["error"].as_str() {
            return Ok(Err(if error.starts_with("IMAGE_TOO_LARGE") {
                ScreenRefusal::TooLarge(error.to_owned())
            } else {
                ScreenRefusal::DisplayUnavailable(error.to_owned())
            }));
        }
        if out.status != Some(0) || reported.is_null() {
            return Ok(Err(ScreenRefusal::DisplayUnavailable(format!(
                "DISPLAY_UNAVAILABLE: the capture failed: {}",
                super::bounded(out.stderr.trim(), 300)
            ))));
        }
        // Root takes a bounded copy of a regular file (never through a link)
        // into a directory only root writes; that copy is what leaves.
        let moved = self
            .runtime
            .exec(
                name,
                "/bin/sh",
                &[
                    "-c",
                    r#"set -e; d=$(dirname "?2"); mkdir -p "$d"; chmod 0700 "$d"; find "$d" -mindepth 1 -delete
[ -f "?1" ] && [ ! -L "?1" ]
dd if="?1" of="?2" iflag=nofollow,count_bytes count="?3" status=none
[ "$(stat -c %s "?2")" -lt "?3" ]"#,
                    "sh",
                    user_path,
                    root_path,
                    &(MAX_IMAGE_BYTES + 1).to_string(),
                ],
                Duration::from_secs(30),
            )
            .await?;
        if moved.status != Some(0) {
            return Ok(Err(ScreenRefusal::Invalid(
                "the captured file was not a regular file within the budget".into(),
            )));
        }
        let temp = staging.join(format!(".capture-{capture_id}"));
        self.runtime.copy_out(name, root_path, &temp).await?;
        let meta = std::fs::symlink_metadata(&temp)
            .map_err(|e| ComputerError::Refused(format!("screenshot copy: {e}")))?;
        if !meta.is_file() || meta.len() > MAX_IMAGE_BYTES {
            return Ok(Err(ScreenRefusal::Invalid(
                "the copied screenshot was not a file within the budget".into(),
            )));
        }
        let png = std::fs::read(&temp)
            .map_err(|e| ComputerError::Refused(format!("screenshot copy: {e}")))?;
        let size = (
            reported["image_width"].as_u64().unwrap_or(0) as u32,
            reported["image_height"].as_u64().unwrap_or(0) as u32,
        );
        if png_size(&png) != Some(size)
            || reported["sha256"].as_str() != Some(hex::encode(Sha256::digest(&png)).as_str())
        {
            return Ok(Err(ScreenRefusal::Invalid(
                "the copied screenshot does not match the capture".into(),
            )));
        }
        Ok(Ok((reported, png)))
    }

    /// Keep a captured screenshot as an artifact with its metadata. A failed
    /// registration removes the stored file.
    async fn save_screen(
        &self,
        bot_id: Uuid,
        turn_id: Option<Uuid>,
        temp: &std::path::Path,
        png: &[u8],
        meta: &mut ScreenMeta,
    ) -> Result<(), ComputerError> {
        let id = Uuid::new_v4();
        let store = self
            .config
            .state_dir
            .join("artifacts")
            .join(bot_id.to_string());
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&store)
                .map_err(|e| ComputerError::Refused(format!("artifact store: {e}")))?;
        }
        let path = store.join(id.to_string());
        std::fs::rename(temp, &path)
            .map_err(|e| ComputerError::Refused(format!("artifact store: {e}")))?;
        meta.artifact_id = Some(id);
        let inserted = sqlx::query(
            "INSERT INTO artifacts (id, bot_id, turn_id, guest_path, size_bytes, sha256, meta)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(id)
        .bind(bot_id)
        .bind(turn_id)
        .bind(format!("screen:{}", meta.capture_id))
        .bind(png.len() as i64)
        .bind(Sha256::digest(png).to_vec())
        .bind(sqlx::types::Json(&*meta))
        .execute(&self.pool)
        .await;
        if let Err(err) = inserted {
            meta.artifact_id = None;
            let _ = std::fs::remove_file(&path);
            return Err(err.into());
        }
        Ok(())
    }

    /// A screenshot this bot saved earlier, from its artifact store. Another
    /// bot's artifact, or an artifact that is not a screenshot, looks the
    /// same as none. The stored bytes must match the recorded SHA-256.
    pub async fn saved_screen(
        &self,
        bot_id: Uuid,
        artifact_id: Uuid,
    ) -> Result<Result<(Vec<u8>, serde_json::Value), ScreenRefusal>, ComputerError> {
        let row: Option<(Vec<u8>, Option<serde_json::Value>)> = sqlx::query_as(
            "SELECT sha256, meta FROM artifacts
             WHERE id = ?1 AND bot_id = ?2 AND guest_path LIKE 'screen:%'",
        )
        .bind(artifact_id)
        .bind(bot_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((digest, meta)) = row else {
            return Ok(Err(ScreenRefusal::NoSuchArtifact));
        };
        let path = self
            .config
            .state_dir
            .join("artifacts")
            .join(bot_id.to_string())
            .join(artifact_id.to_string());
        let Ok(png) = std::fs::read(&path) else {
            return Ok(Err(ScreenRefusal::NoSuchArtifact));
        };
        if png.len() as u64 > MAX_IMAGE_BYTES
            || Sha256::digest(&png).as_slice() != digest.as_slice()
            || png_size(&png).is_none()
        {
            return Ok(Err(ScreenRefusal::Invalid(
                "the saved screenshot does not match its record".into(),
            )));
        }
        Ok(Ok((png, meta.unwrap_or(serde_json::Value::Null))))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn png_headers_are_read_and_others_refused() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&1280u32.to_be_bytes());
        png.extend_from_slice(&800u32.to_be_bytes());
        assert_eq!(super::png_size(&png), Some((1280, 800)));
        assert_eq!(super::png_size(b"GIF89a..."), None);
    }
}
