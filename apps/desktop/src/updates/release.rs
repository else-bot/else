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

/// Which builds this installation follows, saved by `install.sh --channel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Channel {
    /// Tagged `vX.Y.Z` releases.
    #[default]
    Stable,
    /// Every commit of main, built on this machine.
    Dev,
}

const CHANNEL_FILE: &str = "config/update-channel";

impl Channel {
    pub fn saved(home: &Path) -> Result<Self, String> {
        let path = home.join(CHANNEL_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => match text.trim() {
                "stable" => Ok(Self::Stable),
                "dev" => Ok(Self::Dev),
                other => Err(format!(
                    "{} names an unknown update channel ({other:?}).",
                    path.display()
                )),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::Stable),
            Err(err) => Err(format!("Cannot read {}: {err}.", path.display())),
        }
    }
}

/// An installable build: what `versions/<id>` holds and `install.sh
/// --version <id>` installs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Release {
    Stable(Version),
    Dev(Commit),
}

impl Release {
    pub fn id(&self) -> String {
        match self {
            Self::Stable(version) => version.plain(),
            Self::Dev(commit) => format!("dev-{}", commit.0),
        }
    }

    /// Whether installing `self` changes an installation running `installed`.
    /// Stable never offers an older release, but always leaves a dev build.
    pub fn replaces(&self, installed: &Release) -> bool {
        match (self, installed) {
            (Self::Stable(latest), Self::Stable(installed)) => latest > installed,
            (Self::Stable(_), Self::Dev(_)) => true,
            (Self::Dev(_), _) => self != installed,
        }
    }
}

/// `vX.Y.Z`, or `main@abcdef1` for a dev build.
impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stable(version) => version.fmt(f),
            Self::Dev(commit) => write!(f, "main@{}", &commit.0[..7]),
        }
    }
}

impl FromStr for Release {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, ()> {
        match text.strip_prefix("dev-") {
            Some(commit) => commit.parse().map(Self::Dev),
            None => text.parse().map(Self::Stable),
        }
    }
}

/// A full commit ID: 40 lowercase hex digits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit(String);

impl FromStr for Commit {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, ()> {
        (text.len() == 40 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
            .then(|| Self(text.to_owned()))
            .ok_or(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl Version {
    /// `X.Y.Z`, as the installer's `--version` takes it.
    pub fn plain(&self) -> String {
        format!("{}.{}.{}", self.0, self.1, self.2)
    }

    fn from_tag(tag: &str) -> Option<Self> {
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

/// The newest commit of main in `git ls-remote <repo> refs/heads/main` output.
pub fn main_head(listing: &str) -> Option<Commit> {
    listing.lines().find_map(|line| {
        let (commit, name) = line.split_once('\t')?;
        (name == "refs/heads/main").then(|| commit.parse().ok())?
    })
}

/// The component's activated release, falling back to an unmigrated installation.
pub fn installed(home: &Path, component: super::Component) -> Result<Release, String> {
    let link = crate::launch::component_directory(home, component.pointer());
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

/// The newest stable release at `repo`.
pub async fn latest(
    git: &Path,
    repo: &str,
    env: Vec<(OsString, OsString)>,
    limit: Duration,
) -> Result<Version, String> {
    let listing = ls_remote(git, repo, &["--tags", "--refs", repo, "v*"], env, limit).await?;
    newest(&listing).ok_or_else(|| format!("No else.bot release (vX.Y.Z tag) was found at {repo}."))
}

/// The newest build on `channel` at `repo`.
pub async fn latest_on(
    git: &Path,
    repo: &str,
    channel: Channel,
    env: Vec<(OsString, OsString)>,
    limit: Duration,
) -> Result<Release, String> {
    match channel {
        Channel::Stable => latest(git, repo, env, limit).await.map(Release::Stable),
        Channel::Dev => {
            let listing = ls_remote(git, repo, &[repo, "refs/heads/main"], env, limit).await?;
            main_head(&listing)
                .map(Release::Dev)
                .ok_or_else(|| format!("{repo} has no main branch."))
        }
    }
}

/// Git never prompts: it has no terminal and no stdin, and it is stopped
/// after `limit`.
async fn ls_remote(
    git: &Path,
    repo: &str,
    args: &[&str],
    env: Vec<(OsString, OsString)>,
    limit: Duration,
) -> Result<String, String> {
    let mut child = tokio::process::Command::new(git)
        .arg("ls-remote")
        .args(args)
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
    Ok(String::from_utf8_lossy(&out).into_owned())
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

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn releases_name_their_version_directories() {
        let stable: Release = "0.0.21".parse().unwrap();
        assert_eq!(stable, Release::Stable(v("0.0.21")));
        assert_eq!(
            (stable.id(), stable.to_string()),
            ("0.0.21".into(), "v0.0.21".into())
        );
        let dev: Release = format!("dev-{COMMIT}").parse().unwrap();
        assert_eq!(dev.id(), format!("dev-{COMMIT}"));
        assert_eq!(dev.to_string(), "main@0123456");
        for bad in [
            "dev-0123",
            "dev-0123456789ABCDEF0123456789abcdef01234567",
            "dev-",
            "v0.0.1",
        ] {
            assert!(bad.parse::<Release>().is_err(), "{bad}");
        }
    }

    #[test]
    fn stable_never_downgrades_but_dev_follows_any_other_commit() {
        let stable = |text: &str| Release::Stable(v(text));
        let dev = |commit: &str| Release::Dev(commit.parse().unwrap());
        let other = "1123456789abcdef0123456789abcdef01234567";
        assert!(stable("0.0.21").replaces(&stable("0.0.20")));
        assert!(!stable("0.0.20").replaces(&stable("0.0.20")));
        assert!(!stable("0.0.19").replaces(&stable("0.0.20")));
        assert!(stable("0.0.19").replaces(&dev(COMMIT)), "leaving dev");
        assert!(dev(COMMIT).replaces(&stable("0.0.20")), "joining dev");
        assert!(dev(other).replaces(&dev(COMMIT)));
        assert!(!dev(COMMIT).replaces(&dev(COMMIT)));
    }

    #[test]
    fn the_dev_channel_reads_the_head_of_main() {
        let listing = format!("{COMMIT}\trefs/heads/main\n");
        assert_eq!(main_head(&listing), COMMIT.parse().ok());
        assert_eq!(main_head(&format!("{COMMIT}\trefs/heads/mainline\n")), None);
        assert_eq!(main_head("abc\trefs/heads/main\n"), None);
    }

    #[test]
    fn the_saved_channel_defaults_to_stable() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Channel::saved(dir.path()), Ok(Channel::Stable));
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        let file = dir.path().join(CHANNEL_FILE);
        std::fs::write(&file, "dev\n").unwrap();
        assert_eq!(Channel::saved(dir.path()), Ok(Channel::Dev));
        std::fs::write(&file, "nightly\n").unwrap();
        assert!(
            Channel::saved(dir.path())
                .unwrap_err()
                .contains("unknown update channel")
        );
    }

    #[test]
    fn a_dev_build_is_installed_by_its_commit() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            format!("versions/dev-{COMMIT}"),
            dir.path().join("app-current"),
        )
        .unwrap();
        assert_eq!(
            installed(dir.path(), super::super::Component::App),
            Ok(Release::Dev(COMMIT.parse().unwrap()))
        );
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
        assert_eq!(
            installed(dir.path(), super::super::Component::App),
            Ok(Release::Stable(v("0.0.20")))
        );
        let other = tempfile::tempdir().unwrap();
        assert!(
            installed(other.path(), super::super::Component::App)
                .unwrap_err()
                .contains("Cannot tell")
        );
    }

    #[test]
    fn component_pointers_override_legacy_and_each_other() {
        use super::super::Component::{App, Server};
        let dir = tempfile::tempdir().unwrap();
        for (pointer, target) in [
            ("current", "versions/0.0.9"),
            ("app-current", "versions/0.0.20"),
            ("server-current", "versions/0.0.10"),
        ] {
            std::os::unix::fs::symlink(target, dir.path().join(pointer)).unwrap();
        }
        assert_eq!(installed(dir.path(), App), Ok(Release::Stable(v("0.0.20"))));
        assert_eq!(
            installed(dir.path(), Server),
            Ok(Release::Stable(v("0.0.10")))
        );
        std::fs::remove_file(dir.path().join("app-current")).unwrap();
        std::os::unix::fs::symlink("missing", dir.path().join("app-current")).unwrap();
        assert!(
            installed(dir.path(), App).is_err(),
            "a broken component pointer must never use legacy current"
        );
    }
}
