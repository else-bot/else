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
const GUEST_DIR: &str = "/var/lib/elsebot/screens";

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
    /// another bot is `LeaseEnded`).
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
        let (lease_bot, name, _op) = self.checked(lease).await?;
        if lease_bot != bot_id {
            return Err(ComputerError::LeaseEnded);
        }
        let capture_id = Uuid::new_v4();
        let guest_path = format!("{GUEST_DIR}/{capture_id}.png");
        let (program, mut args) = self.as_guest_user(
            r#"mkdir -p "$(dirname "$1")" && DISPLAY=:99 exec /usr/local/bin/else-screenshot "$1" "$2""#,
        );
        args.push(guest_path.clone());
        args.push(MAX_IMAGE_BYTES.to_string());
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = self
            .runtime
            .exec(&name, &program, &argv, Duration::from_secs(60))
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
        let temp = staging.join(format!(".capture-{capture_id}"));
        let copied = self.runtime.copy_out(&name, &guest_path, &temp).await;
        let (rm, mut rm_args) = self.as_guest_user(r#"rm -f -- "$1""#);
        rm_args.push(guest_path.clone());
        let rm_argv: Vec<&str> = rm_args.iter().map(String::as_str).collect();
        let _ = self
            .runtime
            .exec(&name, &rm, &rm_argv, Duration::from_secs(30))
            .await;
        drop(_op);
        let read = copied.map_err(ComputerError::from).and_then(|()| {
            let meta = std::fs::symlink_metadata(&temp)
                .map_err(|e| ComputerError::Refused(format!("screenshot copy: {e}")))?;
            if !meta.is_file() || meta.len() > MAX_IMAGE_BYTES {
                return Ok(None);
            }
            std::fs::read(&temp)
                .map(Some)
                .map_err(|e| ComputerError::Refused(format!("screenshot copy: {e}")))
        });
        let png = match read {
            Ok(Some(png)) => png,
            Ok(None) => {
                let _ = std::fs::remove_file(&temp);
                return Ok(Err(ScreenRefusal::Invalid(
                    "the copied screenshot was not a file within the budget".into(),
                )));
            }
            Err(err) => {
                let _ = std::fs::remove_file(&temp);
                return Err(err);
            }
        };
        let sha256 = hex::encode(Sha256::digest(&png));
        let field = |k: &str| reported[k].as_u64().unwrap_or(0) as u32;
        let (image_width, image_height) = (field("image_width"), field("image_height"));
        if png_size(&png) != Some((image_width, image_height))
            || reported["sha256"].as_str() != Some(sha256.as_str())
        {
            let _ = std::fs::remove_file(&temp);
            return Ok(Err(ScreenRefusal::Invalid(
                "the copied screenshot does not match the capture".into(),
            )));
        }
        let artifact_id = if save {
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
            std::fs::rename(&temp, store.join(id.to_string()))
                .map_err(|e| ComputerError::Refused(format!("artifact store: {e}")))?;
            sqlx::query(
                "INSERT INTO artifacts (id, bot_id, turn_id, guest_path, size_bytes, sha256)
                 VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(id)
            .bind(bot_id)
            .bind(turn_id)
            .bind(format!("screen:{capture_id}"))
            .bind(png.len() as i64)
            .bind(Sha256::digest(&png).to_vec())
            .execute(&self.pool)
            .await?;
            Some(id)
        } else {
            let _ = std::fs::remove_file(&temp);
            None
        };
        let captured_at = reported["captured_at"]
            .as_f64()
            .and_then(|t| chrono::DateTime::from_timestamp_millis((t * 1000.0) as i64))
            .unwrap_or_else(chrono::Utc::now);
        let pointer = &reported["pointer"];
        Ok(Ok(Screenshot {
            meta: ScreenMeta {
                capture_id,
                screen_width: field("screen_width"),
                screen_height: field("screen_height"),
                image_width,
                image_height,
                scale: field("scale").max(1),
                pointer: [
                    pointer[0].as_i64().unwrap_or(0) as i32,
                    pointer[1].as_i64().unwrap_or(0) as i32,
                ],
                bytes: png.len() as u64,
                sha256,
                captured_at,
                artifact_id,
            },
            png,
        }))
    }

    /// A screenshot this bot saved earlier, from its artifact store. Another
    /// bot's artifact, or an artifact that is not a screenshot, looks the
    /// same as none. The stored bytes must match the recorded SHA-256.
    pub async fn saved_screen(
        &self,
        bot_id: Uuid,
        artifact_id: Uuid,
    ) -> Result<Result<(Vec<u8>, chrono::DateTime<chrono::Utc>), ScreenRefusal>, ComputerError>
    {
        let row: Option<(Vec<u8>, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
            "SELECT sha256, created_at FROM artifacts
             WHERE id = $1 AND bot_id = $2 AND guest_path LIKE 'screen:%'",
        )
        .bind(artifact_id)
        .bind(bot_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((digest, created_at)) = row else {
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
        Ok(Ok((png, created_at)))
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
