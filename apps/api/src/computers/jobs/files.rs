use super::{ComputerError, ComputerManager, ExecOutput};
use std::time::Duration;
use uuid::Uuid;

const WRITE_FILE: &str = r#"cd "$HOME/work" 2>/dev/null || cd "$HOME"; mkdir -p -- "$(dirname -- "$1")" && printf %s "$2" | base64 -d > "$1""#;

const READ_FILE: &str = r#"cd "$HOME/work" 2>/dev/null || cd "$HOME"; [ -f "$1" ] || { echo "not a regular file" >&2; exit 1; }
stat -c %s -- "$1" && tail -c +"$(($2 + 1))" -- "$1" | head -c "$3" | base64 -w0"#;

struct CheckedArtifact {
    bytes: u64,
    digest: Vec<u8>,
    preview: Vec<u8>,
}

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
            self.as_guest_user(r#"[ -f "$1" ] && [ ! -L "$1" ] && stat -c %s -- "$1""#);
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
        let checked = (|| -> std::io::Result<Result<CheckedArtifact, String>> {
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
            Ok(Ok(CheckedArtifact {
                bytes: total,
                digest: hasher.finalize().to_vec(),
                preview,
            }))
        })();
        let CheckedArtifact {
            bytes: total,
            digest,
            preview,
        } = match checked {
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
        let bot: Option<Uuid> =
            sqlx::query_scalar("SELECT bot_id FROM computer_leases WHERE id = ?1")
                .bind(lease)
                .fetch_optional(&self.pool)
                .await?;
        if let Some(bot) = bot
            && self.owner_controls(bot, false).await?
        {
            return Err(ComputerError::Refused(
                "OWNER_USING_DESKTOP: the owner took over your desktop; nothing was done. \
                 Wait until they hand it back."
                    .into(),
            ));
        }
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
            WRITE_FILE,
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
                READ_FILE,
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

#[cfg(test)]
mod tests {
    use super::{READ_FILE, WRITE_FILE};
    use base64::Engine;
    use std::path::Path;
    use std::process::Command;

    fn run(home: &Path, script: &str, args: &[&str]) -> (bool, String, String) {
        let out = Command::new("/bin/sh")
            .args(["-c", script, "sh"])
            .args(args)
            .env("HOME", home)
            .output()
            .expect("sh runs");
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn write(home: &Path, path: &str, content: &str) -> (bool, String) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(content);
        let (ok, _, err) = run(home, WRITE_FILE, &[path, &encoded]);
        (ok, err)
    }

    fn read(home: &Path, path: &str) -> Result<String, String> {
        let (ok, out, err) = run(home, READ_FILE, &[path, "0", "4096"]);
        if !ok {
            return Err(err.trim().to_owned());
        }
        let (_, encoded) = out.split_once('\n').expect("size line");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.trim())
            .expect("base64");
        Ok(String::from_utf8(bytes).expect("utf-8"))
    }

    #[test]
    fn file_paths_resolve_relative_absolute_and_home_forms() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        std::fs::create_dir(home.join("work")).unwrap();
        let absolute = home.join("abs.txt");
        let absolute = absolute.to_str().unwrap();
        for (path, on_disk) in [
            ("notes/rel.txt", "work/notes/rel.txt"),
            (absolute, "abs.txt"),
            ("~/work/tilde.txt", "work/tilde.txt"),
            ("$HOME/work/var.txt", "work/var.txt"),
        ] {
            assert_eq!(
                write(home, path, path),
                (true, String::new()),
                "write {path}"
            );
            assert_eq!(
                std::fs::read_to_string(home.join(on_disk)).ok().as_deref(),
                Some(path),
                "{path} lands at {on_disk}"
            );
            assert_eq!(read(home, path).as_deref(), Ok(path), "read {path}");
        }
        assert!(!home.join("work/~").exists(), "no literal ~ directory");
    }

    #[test]
    fn read_tells_a_missing_file_from_a_directory() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        std::fs::create_dir_all(home.join("work/dir")).unwrap();
        assert_eq!(
            read(home, "~/work/missing.txt"),
            Err("no such file: ~/work/missing.txt".to_owned())
        );
        assert_eq!(read(home, "dir"), Err("not a regular file: dir".to_owned()));
    }
}
