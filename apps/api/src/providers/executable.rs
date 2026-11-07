//! Where a provider CLI lives. Turns run under [`super::host::Host`], which
//! replaces the environment and the working directory, so a version
//! manager's shim would pick a different version or none there. Shims are
//! resolved here, with this process's environment, to the executable they
//! select now.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The executable for the CLI `name`: the configured path, else the first
/// executable `name` on `PATH`, with a mise or asdf shim resolved through
/// its manager's `which`. `Ok(None)` means it is not on `PATH`. An error
/// says why the found or configured file cannot be used; a configured path
/// never falls back to `PATH`.
pub fn executable(configured: Option<&Path>, name: &str) -> Result<Option<PathBuf>, String> {
    resolve(configured, name, std::env::var_os("PATH").as_deref())
}

fn resolve(
    configured: Option<&Path>,
    name: &str,
    path: Option<&OsStr>,
) -> Result<Option<PathBuf>, String> {
    let setting = format!("ELSE_{}_BIN", name.to_uppercase());
    let found = match configured {
        Some(found) if is_executable(found) => found.to_path_buf(),
        Some(found) => {
            return Err(format!(
                "{setting} ({}) is not an executable file",
                found.display()
            ));
        }
        None => match search(path, name) {
            Some(found) => found,
            None => return Ok(None),
        },
    };
    native(&found, name, path).map(Some).map_err(|why| {
        format!(
            "`{name}` at {} {why}; set {setting} to the {name} executable itself",
            found.display()
        )
    })
}

#[derive(Clone, Copy)]
enum Manager {
    Mise,
    Asdf,
}

impl Manager {
    const ALL: [Self; 2] = [Self::Mise, Self::Asdf];

    fn name(self) -> &'static str {
        match self {
            Self::Mise => "mise",
            Self::Asdf => "asdf",
        }
    }
}

enum Found {
    Native(PathBuf),
    Link(Manager, PathBuf),
    Script(Manager),
}

fn classify(path: &Path) -> Result<Found, String> {
    let file = std::fs::canonicalize(path).map_err(|e| format!("cannot be read ({e})"))?;
    let named = |m: &Manager| file.file_name().is_some_and(|f| f == m.name());
    if let Some(manager) = Manager::ALL.into_iter().find(named) {
        return Ok(Found::Link(manager, file));
    }
    let mut head = Vec::new();
    std::fs::File::open(&file)
        .and_then(|f| f.take(4096).read_to_end(&mut head))
        .map_err(|e| format!("cannot be read ({e})"))?;
    if head.starts_with(b"#!") {
        let text = String::from_utf8_lossy(&head);
        let words: Vec<_> = text.split_whitespace().collect();
        let execs = |m: &Manager| {
            words.windows(2).any(|pair| {
                matches!(pair[1], "exec" | "x")
                    && Path::new(pair[0].trim_matches(['\'', '"']))
                        .file_name()
                        .is_some_and(|file| file == m.name())
            })
        };
        if let Some(manager) = Manager::ALL.into_iter().find(execs) {
            return Ok(Found::Script(manager));
        }
    }
    Ok(Found::Native(file))
}

fn native(found: &Path, name: &str, path: Option<&OsStr>) -> Result<PathBuf, String> {
    let (manager, binary) = match classify(found)? {
        Found::Native(file) => return Ok(file),
        Found::Link(manager, binary) => (manager, binary),
        Found::Script(manager) => {
            let binary = search(path, manager.name())
                .and_then(|p| std::fs::canonicalize(p).ok())
                .filter(|p| p.file_name().is_some_and(|f| f == manager.name()))
                .ok_or(format!(
                    "is a {0} shim and `{0}` is not on PATH",
                    manager.name()
                ))?;
            (manager, binary)
        }
    };
    let which = format!("`{} which {name}`", manager.name());
    let out = std::process::Command::new(&binary)
        .args(["which", name])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("is a shim and {which} did not run ({e})"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = stderr.lines().next().unwrap_or("").trim();
        return Err(format!("is a shim and {which} failed ({reason})"));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let target = match stdout.trim().lines().collect::<Vec<_>>()[..] {
        [line] if Path::new(line).is_absolute() => PathBuf::from(line),
        _ => {
            return Err(format!(
                "is a shim and {which} printed no absolute path ({:?})",
                stdout.trim()
            ));
        }
    };
    if !is_executable(&target) {
        return Err(format!(
            "is a shim and {which} printed {}, which is not an executable file",
            target.display()
        ));
    }
    match classify(&target)? {
        Found::Native(file) => Ok(file),
        Found::Link(..) | Found::Script(_) => Err(format!(
            "is a shim and {which} printed {}, another shim",
            target.display()
        )),
    }
}

fn search(path: Option<&OsStr>, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    const NAMES: [&str; 3] = ["codex", "claude", "grok"];

    struct Tree(tempfile::TempDir);

    impl Tree {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }

        fn at(&self, rel: &str) -> PathBuf {
            std::fs::canonicalize(self.0.path()).unwrap().join(rel)
        }

        fn file(&self, rel: &str, body: &str, mode: u32) -> PathBuf {
            let path = self.at(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            path
        }

        fn link(&self, rel: &str, target: &Path) -> PathBuf {
            let path = self.at(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            symlink(target, &path).unwrap();
            path
        }

        fn manager(&self, rel: &str, body: &str) -> PathBuf {
            self.file(
                rel,
                &format!("#!/bin/sh\n[ \"$1\" = which ] || exit 2\n{body}\n"),
                0o755,
            )
        }

        fn native(&self, name: &str) -> PathBuf {
            self.file(&format!("installs/{name}/1.0/bin/{name}"), "native", 0o755)
        }

        fn path(&self, dirs: &[&str]) -> std::ffi::OsString {
            std::env::join_paths(dirs.iter().map(|d| self.at(d))).unwrap()
        }
    }

    fn installs_answer(tree: &Tree) -> String {
        format!("echo \"{}/installs/$2/1.0/bin/$2\"", tree.at("").display())
    }

    #[test]
    fn a_mise_link_shim_on_path_resolves_to_each_providers_install() {
        let tree = Tree::new();
        let mise = tree.manager("usr/bin/mise", &installs_answer(&tree));
        for name in NAMES {
            let native = tree.native(name);
            tree.file(&format!("not-exec/{name}"), "data", 0o644);
            tree.link(&format!("wrappers/{name}"), &mise);
            let path = tree.path(&["missing", "not-exec", "wrappers"]);
            assert_eq!(resolve(None, name, Some(&path)), Ok(Some(native)), "{name}");
        }
    }

    #[test]
    fn script_shims_resolve_through_the_manager_on_path() {
        for manager in ["asdf", "mise"] {
            let tree = Tree::new();
            tree.manager(&format!("bin/{manager}"), &installs_answer(&tree));
            for name in NAMES {
                let native = tree.native(name);
                tree.file(
                    &format!("shims/{name}"),
                    &format!(
                        "#!/usr/bin/env bash\n# asdf-plugin: {name} 1.0\nexec {manager} exec \"{name}\" \"$@\"\n"
                    ),
                    0o755,
                );
                let path = tree.path(&["shims", "bin"]);
                assert_eq!(
                    resolve(None, name, Some(&path)),
                    Ok(Some(native)),
                    "{manager} {name}"
                );
            }
        }
    }

    #[test]
    fn quoted_manager_paths_are_resolved_and_never_returned_as_native() {
        for manager in ["mise", "asdf"] {
            let tree = Tree::new();
            let native = tree.native("codex");
            let binary = tree.manager(&format!("bin/{manager}"), &installs_answer(&tree));
            let path = tree.path(&["shims", "bin"]);
            for quote in ['\"', '\''] {
                let shim = tree.file(
                    "shims/codex",
                    &format!(
                        "#!/bin/sh\nexec {quote}/opt/tool manager/bin/{manager}{quote} exec codex \"$@\"\n"
                    ),
                    0o755,
                );
                tree.manager(&format!("bin/{manager}"), &installs_answer(&tree));
                assert_eq!(
                    resolve(None, "codex", Some(&path)),
                    Ok(Some(native.clone()))
                );
                assert_eq!(
                    resolve(Some(&shim), "codex", Some(&path)),
                    Ok(Some(native.clone()))
                );
                tree.manager(
                    &format!("bin/{manager}"),
                    &format!("echo '{}'", shim.display()),
                );
                let link = tree.link(&format!("links/{quote}/codex"), &binary);
                let err = resolve(Some(&link), "codex", Some(&path)).unwrap_err();
                assert!(err.contains("another shim"), "{err}");
            }
        }
    }

    #[test]
    fn a_link_to_a_native_binary_is_kept() {
        let tree = Tree::new();
        let native = tree.native("claude");
        let link = tree.link("local/bin/claude", &native);
        let path = tree.path(&["local/bin"]);
        assert_eq!(
            resolve(None, "claude", Some(&path)),
            Ok(Some(native.clone()))
        );
        assert_eq!(resolve(Some(&link), "claude", None), Ok(Some(native)));
    }

    #[test]
    fn nothing_on_path_is_not_an_error() {
        let tree = Tree::new();
        tree.file("bin/codex", "data", 0o644);
        assert_eq!(resolve(None, "codex", Some(&tree.path(&["bin"]))), Ok(None));
        assert_eq!(resolve(None, "codex", None), Ok(None));
    }

    #[test]
    fn a_configured_path_wins_over_path_and_never_falls_back() {
        let tree = Tree::new();
        let configured = tree.file("custom/codex", "native", 0o755);
        tree.native("codex");
        tree.link(
            "wrappers/codex",
            &tree.manager("bin/mise", &installs_answer(&tree)),
        );
        let path = tree.path(&["wrappers"]);
        assert_eq!(
            resolve(Some(&configured), "codex", Some(&path)),
            Ok(Some(configured))
        );

        let missing = tree.at("nope/codex");
        let err = resolve(Some(&missing), "codex", Some(&path)).unwrap_err();
        assert!(err.contains("ELSE_CODEX_BIN"), "{err}");
    }

    #[test]
    fn a_configured_mise_shim_is_resolved() {
        let tree = Tree::new();
        let native = tree.native("grok");
        let shim = tree.link(
            "shims/grok",
            &tree.manager("bin/mise", &installs_answer(&tree)),
        );
        assert_eq!(resolve(Some(&shim), "grok", None), Ok(Some(native)));
    }

    #[test]
    fn an_unresolved_shim_is_an_error_naming_the_setting() {
        let tree = Tree::new();
        let script = tree.file("bin/codex.sh", "#!/bin/sh\nexit 0\n", 0o755);
        let answers = [
            ("failing", "exit 1".to_owned()),
            ("silent", "true".to_owned()),
            ("relative", "echo installs/codex/1.0/bin/codex".to_owned()),
            (
                "missing",
                format!("echo {}/nowhere/codex", tree.at("").display()),
            ),
            ("two-lines", format!("echo {0}\necho {0}", script.display())),
            (
                "not-exec",
                format!("echo {}", tree.file("data/codex", "x", 0o644).display()),
            ),
            (
                "recursive",
                format!("echo {}/recursive/wrappers/codex", tree.at("").display()),
            ),
        ];
        for (case, body) in answers {
            let mise = tree.manager(&format!("{case}/mise"), &body);
            let shim = tree.link(&format!("{case}/wrappers/codex"), &mise);
            let path = tree.path(&[&format!("{case}/wrappers")]);
            for got in [
                resolve(None, "codex", Some(&path)),
                resolve(Some(&shim), "codex", None),
            ] {
                let err = got.expect_err(case);
                assert!(err.contains("ELSE_CODEX_BIN"), "{case}: {err}");
            }
        }

        let asdf_shim = tree.file(
            "lonely/shims/claude",
            "#!/usr/bin/env bash\nexec asdf exec \"claude\" \"$@\"\n",
            0o755,
        );
        let path = tree.path(&["lonely/shims"]);
        let err = resolve(None, "claude", Some(&path)).unwrap_err();
        assert!(err.contains("ELSE_CLAUDE_BIN"), "{err}");
        let err = resolve(Some(&asdf_shim), "claude", None).unwrap_err();
        assert!(err.contains("ELSE_CLAUDE_BIN"), "{err}");
    }
}
