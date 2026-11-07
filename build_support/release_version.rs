// Shared by the API and desktop build scripts: the version a build reports.
//
// Releases bump the workspace version only on their own release commit, so
// main keeps an older one. The release workflow also tags the main commit
// each release was cut from as `main/vX.Y.Z`, which `git pull` fetches. A
// build reports the highest release tagged in its history (`main/vX.Y.Z`,
// or a `vX.Y.Z` tag in that history), or the workspace version when that is
// newer or Git and the tags are unavailable.

use std::{path::Path, process::Command};

pub fn version(manifest_dir: &Path, workspace: &str) -> String {
    watch(manifest_dir);
    let base = parse(workspace);
    let tags = git(
        manifest_dir,
        &["tag", "--merged", "HEAD", "--list", "main/v*", "v*"],
    )
    .unwrap_or_default();
    tags.lines()
        .map(|tag| {
            tag.trim()
                .trim_start_matches("main/")
                .trim_start_matches('v')
        })
        .filter_map(|found| Some((parse(found)?, found)))
        .filter(|(found, _)| base.is_some_and(|base| *found > base))
        .max_by_key(|(found, _)| *found)
        .map_or(workspace, |(_, found)| found)
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

// Rerun when the checkout switches branches, the tags change, or the branch
// moves to other history: a pull, merge, rebase, or reset can bring in a
// release whose tag was fetched earlier, and each of them writes ORIG_HEAD.
// Plain commits do not, so they do not rebuild both binaries. A checkout
// that never wrote ORIG_HEAD watches its branch ref instead.
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
    let mut paths = vec![
        format!("{git_dir}/HEAD"),
        format!("{common}/packed-refs"),
        format!("{common}/refs/tags"),
    ];
    let moved = format!("{git_dir}/ORIG_HEAD");
    if Path::new(&moved).exists() {
        paths.push(moved);
    } else if let Some(branch) = git(dir, &["symbolic-ref", "-q", "HEAD"]) {
        paths.push(format!("{common}/{}", branch.trim()));
    }
    for path in paths {
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
