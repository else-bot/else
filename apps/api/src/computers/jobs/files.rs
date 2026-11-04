//! Guest files, browser helper, and host artifact transfer.

use super::{ComputerError, ComputerManager, ExecOutput};
use std::time::Duration;
use uuid::Uuid;

impl ComputerManager {
    /// Copy a regular guest file into the host artifact store (at most
    /// `max` bytes) and register it. Checked and copied under the bot's
    /// operation lock with the lease revalidated, so a stop cannot slip in
    /// (copying would wake a stopped computer). Anything over the limit, not
    /// a regular file, or failing midway is removed again.
    pub async fn fetch_artifact(
        &self,
        lease: Uuid,
        bot_id: Uuid,
        turn_id: Option<Uuid>,
        guest_path: &str,
        max: u64,
    ) -> Result<Result<(Uuid, u64, String, String), String>, ComputerError> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let (lease_bot, name, _op) = self.checked(lease).await?;
        if lease_bot != bot_id {
            return Err(ComputerError::LeaseEnded);
        }
        let (program, mut args) =
            self.as_guest_user(r#"[ -f "?1" ] && [ ! -L "?1" ] && stat -c %s -- "?1""#);
        args.push(guest_path.to_owned());
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let size = self
            .runtime
            .exec(&name, &program, &argv, Duration::from_secs(30))
            .await?;
        let Ok(bytes) = size.stdout.trim().parse::<u64>() else {
            return Ok(Err("not a regular file (or not readable)".into()));
        };
        if bytes > max {
            return Ok(Err(format!(
                "the file is {bytes} bytes; the limit is {max}"
            )));
        }
        let store = self
            .config
            .state_dir
            .join("artifacts")
            .join(bot_id.to_string());
        let io = |e: std::io::Error| {
            ComputerError::Runtime(crate::computers::RuntimeError::Unavailable(e.to_string()))
        };
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&store)
                .map_err(io)?;
        }
        let id = Uuid::new_v4();
        let temp = store.join(format!(".incoming-{id}"));
        let final_path = store.join(id.to_string());
        let remove = |path: &std::path::Path| {
            let _ = std::fs::remove_dir_all(path);
            let _ = std::fs::remove_file(path);
        };
        if let Err(err) = self.runtime.copy_out(&name, guest_path, &temp).await {
            remove(&temp);
            return Err(err.into());
        }
        // Verify what arrived: a regular file within the limit, hashed in
        // a stream rather than loaded whole.
        let checked = (|| -> std::io::Result<Result<(u64, Vec<u8>, Vec<u8>), String>> {
            let meta = std::fs::symlink_metadata(&temp)?;
            if !meta.is_file() {
                return Ok(Err("the copy was not a regular file".into()));
            }
            if meta.len() > max {
                return Ok(Err("the file grew past the limit while copying".into()));
            }
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))?;
            let mut file = std::fs::File::open(&temp)?;
            let mut hasher = Sha256::new();
            let mut preview = Vec::new();
            let mut total = 0u64;
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let n = file.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                if total > max {
                    return Ok(Err("the file grew past the limit while copying".into()));
                }
                hasher.update(&chunk[..n]);
                if preview.len() < 2000 {
                    let take = (2000 - preview.len()).min(n);
                    preview.extend_from_slice(&chunk[..take]);
                }
            }
            Ok(Ok((total, hasher.finalize().to_vec(), preview)))
        })();
        let (total, digest, preview) = match checked {
            Ok(Ok(found)) => found,
            Ok(Err(reason)) => {
                remove(&temp);
                return Ok(Err(reason));
            }
            Err(err) => {
                remove(&temp);
                return Err(io(err));
            }
        };
        std::fs::rename(&temp, &final_path).map_err(|e| {
            remove(&temp);
            io(e)
        })?;
        sqlx::query(
            "INSERT INTO artifacts (id, bot_id, turn_id, guest_path, size_bytes, sha256)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(id)
        .bind(bot_id)
        .bind(turn_id)
        .bind(guest_path)
        .bind(total as i64)
        .bind(&digest)
        .execute(&self.pool)
        .await?;
        let preview = String::from_utf8_lossy(&preview).into_owned();
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Ok((id, total, hex, preview)))
    }

    /// Run the guest browser helper (`else-browser`).
    pub async fn browser(&self, lease: Uuid, args: &[&str]) -> Result<ExecOutput, ComputerError> {
        self.guest(
            lease,
            r#"DISPLAY=:99 exec /usr/local/bin/else-browser "$@""#,
            args,
            Duration::from_secs(60),
        )
        .await
    }

    /// Write a text file in the guest (via base64, no shell quoting).
    pub async fn write_file(
        &self,
        lease: Uuid,
        path: &str,
        content: &str,
    ) -> Result<ExecOutput, ComputerError> {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
        self.guest(
            lease,
            r#"cd "$HOME/work" 2>/dev/null || cd "$HOME"; mkdir -p -- "$(dirname -- "?1")" && printf %s "?2" | base64 -d > "?1""#,
            &[path, &encoded],
            Duration::from_secs(30),
        )
        .await
    }

    /// Read `length` bytes of a guest file from `offset`, exactly: returns
    /// the file size and the raw bytes (sent as base64).
    pub async fn read_file(
        &self,
        lease: Uuid,
        path: &str,
        offset: u64,
        length: u64,
    ) -> Result<Result<(u64, Vec<u8>), String>, ComputerError> {
        use base64::Engine;
        let offset_arg = offset.to_string();
        let length_arg = length.to_string();
        let out = self
            .guest(
                lease,
                r#"cd "$HOME/work" 2>/dev/null || cd "$HOME"; [ -f "?1" ] || { echo "not a regular file" >&2; exit 1; }
stat -c %s -- "?1" && tail -c +"$((?2 + 1))" -- "?1" | head -c "?3" | base64 -w0"#,
                &[path, &offset_arg, &length_arg],
                Duration::from_secs(30),
            )
            .await?;
        if out.status != Some(0) {
            return Ok(Err(out.stderr.trim().chars().take(500).collect()));
        }
        let (size, encoded) = out.stdout.split_once('\n').unwrap_or(("0", ""));
        let size = size.trim().parse().unwrap_or(0);
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .map_err(|e| {
                ComputerError::Runtime(crate::computers::RuntimeError::Unavailable(e.to_string()))
            })?;
        Ok(Ok((size, bytes)))
    }
}
