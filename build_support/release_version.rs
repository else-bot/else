// Shared by the API and desktop build scripts: the version a build reports.
//
// Releases bump the workspace version only on their own release commit, so
// main keeps an older one. The release workflow also tags the main commit
// each release was cut from as `main/vX.Y.Z`, which `git pull` fetches. A
// build reports the nearest release tag in its history (`main/vX.Y.Z`, or a
// `vX.Y.Z` tag on main itself), or the workspace version when that is newer
// or Git and the tags are unavailable.

use std::{path::Path, process::Command};

pub fn version(manifest_dir: &Path, workspace: &str) -> String {
    watch(manifest_dir);
    let tagged = git(
        manifest_dir,
        &[
            "describe",
            "--tags",
            "--abbrev=0",
            "--match",
            "main/v*",
            "--match",
            "v*",
            "HEAD",
        ],
    );
    let base = parse(workspace);
    tagged
        .as_deref()
        .map(|tag| {
            tag.trim()
                .trim_start_matches("main/")
                .trim_start_matches('v')
        })
        .filter(|found| base.is_some() && parse(found) > base)
        .unwrap_or(workspace)
        .to_owned()
}

fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(|part| {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
    });
    let parsed = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(parsed)
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
}

// Rerun when the checkout switches branches or the tags change. A pull that
// brings in a release fetches its tag too. Plain commits never change the
// nearest release tag, so they do not rebuild both binaries.
fn watch(dir: &Path) {
    let Some(dirs) = git(
        dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ],
    ) else {
        return;
    };
    let mut dirs = dirs.lines();
    let (Some(git_dir), Some(common)) = (dirs.next(), dirs.next()) else {
        return;
    };
    for path in [
        format!("{git_dir}/HEAD"),
        format!("{common}/packed-refs"),
        format!("{common}/refs/tags"),
    ] {
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
