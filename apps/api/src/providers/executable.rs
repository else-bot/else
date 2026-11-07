use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub fn executable(configured: Option<&Path>, name: &str) -> Result<Option<PathBuf>, String> {
    resolve(configured, name, std::env::var_os("PATH").as_deref())
}

fn resolve(
    configured: Option<&Path>,
    name: &str,
    path: Option<&OsStr>,
) -> Result<Option<PathBuf>, String> {
    Ok(match configured {
        Some(p) => Some(resolve_native(p, name).unwrap_or_else(|| p.to_path_buf())),
        None => std::env::split_paths(path.unwrap_or_default())
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
            .and_then(|p| resolve_native(&p, name)),
    })
}

fn resolve_native(path: &Path, name: &str) -> Option<PathBuf> {
    let found = std::fs::canonicalize(path).ok()?;
    if found.file_name().is_some_and(|f| f == "mise") {
        let out = std::process::Command::new(&found)
            .args(["which", name])
            .output()
            .ok()
            .filter(|out| out.status.success())?;
        let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        return std::fs::canonicalize(path).ok().filter(|p| p.is_file());
    }
    Some(found)
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

        /// A version manager whose `which <name>` prints `body`'s output.
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

    /// Every shim that does not lead to an executable is reported with the
    /// setting that bypasses it, configured or found on `PATH`.
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
