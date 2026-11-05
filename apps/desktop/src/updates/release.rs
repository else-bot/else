use std::ffi::OsString;
use std::fmt;
use std::path::Path;
use std::process::Stdio;
use std::str::FromStr;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt as _};

/// The repository the installer updates from when `ELSE_REPO` is unset,
/// which it always is for an update started by the app.
pub const REPO: &str = "https://github.com/else-bot/else.git";

const MAX_LISTING: u64 = 1 << 20;
const MAX_ERROR: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl Version {
    /// `X.Y.Z`, as the installer's `--version` takes it.
    pub fn plain(&self) -> String {
        format!("{}.{}.{}", self.0, self.1, self.2)
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        tag.strip_prefix('v')?.parse().ok()
    }
}

/// `vX.Y.Z`, as releases are shown.
impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}.{}.{}", self.0, self.1, self.2)
    }
}

impl FromStr for Version {
    type Err = ();

    /// Only `X.Y.Z` with decimal numbers: no prerelease or build suffix.
    fn from_str(text: &str) -> Result<Self, ()> {
        let mut parts = text.split('.').map(|part| {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(());
            }
            part.parse::<u64>().map_err(|_| ())
        });
        let version = Version(
            parts.next().ok_or(())??,
            parts.next().ok_or(())??,
            parts.next().ok_or(())??,
        );
        if parts.next().is_some() {
            return Err(());
        }
        Ok(version)
    }
}

/// The newest release in `git ls-remote --tags` output.
pub fn newest(listing: &str) -> Option<Version> {
    listing
        .lines()
        .filter_map(|line| {
            let (_, name) = line.split_once('\t')?;
            Version::from_tag(name.strip_prefix("refs/tags/")?)
        })
        .max()
}

/// The release `current` points at (`versions/X.Y.Z`).
pub fn installed(home: &Path) -> Result<Version, String> {
    let link = home.join("current");
    let target = std::fs::read_link(&link).map_err(|err| {
        format!(
            "Cannot tell which release is installed ({}: {err}).",
            link.display()
        )
    })?;
    target
        .file_name()
        .and_then(|name| name.to_str()?.parse().ok())
        .ok_or_else(|| {
            format!(
                "Cannot tell which release is installed ({} points at {}).",
                link.display(),
                target.display()
            )
        })
}

/// The newest release at `repo`. Git never prompts: it has no terminal and
/// no stdin, and it is stopped after `limit`.
pub async fn latest(
    git: &Path,
    repo: &str,
    env: Vec<(OsString, OsString)>,
    limit: Duration,
) -> Result<Version, String> {
    let mut child = tokio::process::Command::new(git)
        .args(["ls-remote", "--tags", "--refs", repo, "v*"])
        .current_dir("/")
        .env_clear()
        .envs(env)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0)
        .spawn()
        .map_err(|err| format!("Cannot run git to check for updates: {err}."))?;
    let group = child.id().map(|pid| pid as libc::pid_t);
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let run = async {
        let (out, err, status) = tokio::join!(
            capped(stdout, MAX_LISTING),
            capped(stderr, MAX_ERROR),
            child.wait()
        );
        (out, err, status)
    };
    let Ok((out, err, status)) = tokio::time::timeout(limit, run).await else {
        if let Some(group) = group {
            // SAFETY: git was spawned as the leader of this process group;
            // its transport helpers are in it too.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        return Err(format!(
            "The release list at {repo} did not answer within {} s.",
            limit.as_secs()
        ));
    };
    let status = status.map_err(|err| format!("git failed: {err}."))?;
    if out.len() as u64 > MAX_LISTING {
        return Err(format!("The release list at {repo} is too large."));
    }
    if !status.success() {
        let why = String::from_utf8_lossy(&err);
        let why = why
            .lines()
            .rfind(|line| !line.trim().is_empty())
            .unwrap_or("");
        return Err(format!("Cannot reach {repo} ({}).", why.trim()));
    }
    newest(&String::from_utf8_lossy(&out))
        .ok_or_else(|| format!("No else.bot release (vX.Y.Z tag) was found at {repo}."))
}

async fn capped(pipe: Option<impl AsyncRead + Unpin>, max: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    if let Some(pipe) = pipe {
        let _ = pipe.take(max + 1).read_to_end(&mut bytes).await;
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        text.parse().unwrap()
    }

    #[test]
    fn releases_compare_as_numbers() {
        assert!(v("0.0.20") > v("0.0.9"));
        assert!(v("0.10.0") > v("0.9.99"));
        assert!(v("1.0.0") > v("0.99.99"));
        assert_eq!(v("0.0.21").to_string(), "v0.0.21");
        assert_eq!(v("0.0.21").plain(), "0.0.21");
        for bad in ["0.0", "0.0.1.2", "0.0.1-rc1", "0.0.x", "+1.0.0", "", "0..1"] {
            assert!(bad.parse::<Version>().is_err(), "{bad}");
        }
    }

    #[test]
    fn the_newest_stable_tag_wins() {
        let listing = "\
a1\trefs/tags/v0.0.9
a2\trefs/tags/v0.0.20
a3\trefs/tags/v0.0.21-rc.1
a4\trefs/tags/v0.1.0^{}
a5\trefs/tags/v1.0.0-beta
a6\trefs/tags/v0.0.19
a7\trefs/heads/v9.9.9
a8\trefs/tags/release-9.9.9
";
        assert_eq!(newest(listing), Some(v("0.0.20")));
        assert_eq!(newest("a\trefs/tags/v1.0.0-rc1\n"), None);
    }

    #[test]
    fn the_installed_release_is_what_current_points_at() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("versions/0.0.20", dir.path().join("current")).unwrap();
        assert_eq!(installed(dir.path()), Ok(v("0.0.20")));
        let other = tempfile::tempdir().unwrap();
        assert!(installed(other.path()).unwrap_err().contains("Cannot tell"));
    }
}
