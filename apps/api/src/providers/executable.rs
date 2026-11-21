//! Where a provider CLI lives. Turns run under [`super::host::Host`], which
//! replaces the environment and the working directory, so a version
//! manager's shim would pick a different version or none there. Shims are
//! resolved here, with this process's environment, to the executable they
//! select now.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::host::{Context, Host, Run, RunError};

/// A provider CLI to launch. One found by [`executable`] remembers how: a
/// version manager's upgrade can remove the version a shim resolved to,
/// and a reinstall can move the CLI to another `PATH` directory, while the
/// API runs. [`Program::current`] looks it up again then.
#[derive(Debug, Clone)]
pub struct Program {
    path: PathBuf,
    found: Option<Lookup>,
}

#[derive(Debug, Clone)]
struct Lookup {
    host: Host,
    given: PathBuf,
    configured: bool,
    name: String,
    setting: String,
}

impl Program {
    /// The executable as resolved when the provider was set up.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The executable to launch now, or why there is none, naming the file
    /// tried and the setting that overrides it.
    pub async fn current(&self) -> Result<PathBuf, String> {
        self.current_in(std::env::var_os("PATH").as_deref()).await
    }

    async fn current_in(&self, path: Option<&OsStr>) -> Result<PathBuf, String> {
        let Some(found) = &self.found else {
            return Ok(self.path.clone());
        };
        if is_executable(&self.path) {
            return Ok(self.path.clone());
        }
        let given = if is_executable(&found.given) || found.configured {
            Some(found.given.clone())
        } else {
            find(None, &found.name, &found.setting, path)?
        };
        if let Some(given) = given.filter(|given| is_executable(given)) {
            return locate(&found.host, &given, &found.name, &found.setting, path).await;
        }
        Err(format!(
            "`{}` was not found at {}. Install it or set {}.",
            found.name,
            found.given.display(),
            found.setting
        ))
    }
}

/// Exactly this executable, never looked up again.
impl From<PathBuf> for Program {
    fn from(path: PathBuf) -> Self {
        Self { path, found: None }
    }
}

/// The executable for the CLI `name`: the configured path, else the first
/// executable `name` on `PATH`, with a mise or asdf shim resolved through
/// its manager's `which`. A found link is kept, not resolved: a package
/// manager's upgrade repoints it and removes the version it named.
/// `Ok(None)` means it is not on `PATH`. An error says why the found or
/// configured file cannot be used; a configured path never falls back to
/// `PATH`.
pub async fn executable(
    host: &Host,
    configured: Option<&Path>,
    name: &str,
) -> Result<Option<Program>, String> {
    executable_as(
        host,
        configured,
        name,
        &format!("ELSE_{}_BIN", name.to_uppercase()),
    )
    .await
}

/// [`executable`] for a CLI configured by a `setting` not named after it.
pub async fn executable_as(
    host: &Host,
    configured: Option<&Path>,
    name: &str,
    setting: &str,
) -> Result<Option<Program>, String> {
    program(
        host,
        configured,
        name,
        setting,
        std::env::var_os("PATH").as_deref(),
    )
    .await
}

async fn program(
    host: &Host,
    configured: Option<&Path>,
    name: &str,
    setting: &str,
    path: Option<&OsStr>,
) -> Result<Option<Program>, String> {
    let Some(given) = find(configured, name, setting, path)? else {
        return Ok(None);
    };
    Ok(Some(Program {
        path: locate(host, &given, name, setting, path).await?,
        found: Some(Lookup {
            host: host.clone(),
            given,
            configured: configured.is_some(),
            name: name.to_owned(),
            setting: setting.to_owned(),
        }),
    }))
}

/// The configured file, else the first executable `name` on `path`, as an
/// absolute path: turns start in the bot's project, where a relative path
/// means another file.
fn find(
    configured: Option<&Path>,
    name: &str,
    setting: &str,
    path: Option<&OsStr>,
) -> Result<Option<PathBuf>, String> {
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
    std::path::absolute(&found)
        .map(Some)
        .map_err(|e| format!("{} cannot be read ({e})", found.display()))
}

/// The executable `found` runs: itself, or the version its shim selects.
async fn locate(
    host: &Host,
    found: &Path,
    name: &str,
    setting: &str,
    path: Option<&OsStr>,
) -> Result<PathBuf, String> {
    native(host, found, name, path).await.map_err(|why| {
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
    Native,
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
    Ok(Found::Native)
}

async fn native(
    host: &Host,
    found: &Path,
    name: &str,
    path: Option<&OsStr>,
) -> Result<PathBuf, String> {
    let (manager, binary) = match classify(found)? {
        Found::Native => return Ok(found.to_path_buf()),
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
    let run = Run {
        context: Context::Inherited,
        limit: 64 * 1024,
        ..Run::new(&binary, &["which", name])
    };
    let stdout = host.run(&run).await.map_err(|err| match err {
        RunError::Failed { .. } => format!("is a shim and {which} failed ({err})"),
        err => format!("is a shim and {which} did not run ({err})"),
    })?;
    let stdout = String::from_utf8_lossy(&stdout);
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
        Found::Native => Ok(target),
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

/// Whether `path` is a file someone may execute.
pub fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    const NAMES: [&str; 3] = ["codex", "claude", "grok"];

    async fn resolve(
        configured: Option<&Path>,
        name: &str,
        path: Option<&OsStr>,
    ) -> Result<Option<PathBuf>, String> {
        let setting = format!("ELSE_{}_BIN", name.to_uppercase());
        program(&host(), configured, name, &setting, path)
            .await
            .map(|p| p.map(|p| p.path))
    }

    /// A host whose runs leave nothing behind.
    fn host() -> Host {
        let data = std::env::temp_dir().join(format!("else-executable-{}", uuid::Uuid::new_v4()));
        Host::new(&data, Path::new("/"))
    }

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

    #[tokio::test]
    async fn a_mise_link_shim_on_path_resolves_to_each_providers_install() {
        let tree = Tree::new();
        let mise = tree.manager("usr/bin/mise", &installs_answer(&tree));
        for name in NAMES {
            let native = tree.native(name);
            tree.file(&format!("not-exec/{name}"), "data", 0o644);
            tree.link(&format!("wrappers/{name}"), &mise);
            let path = tree.path(&["missing", "not-exec", "wrappers"]);
            assert_eq!(
                resolve(None, name, Some(&path)).await,
                Ok(Some(native)),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn script_shims_resolve_through_the_manager_on_path() {
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
                    resolve(None, name, Some(&path)).await,
                    Ok(Some(native)),
                    "{manager} {name}"
                );
            }
        }
    }

    #[tokio::test]
    async fn quoted_manager_paths_are_resolved_and_never_returned_as_native() {
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
                    resolve(None, "codex", Some(&path)).await,
                    Ok(Some(native.clone()))
                );
                assert_eq!(
                    resolve(Some(&shim), "codex", Some(&path)).await,
                    Ok(Some(native.clone()))
                );
                tree.manager(
                    &format!("bin/{manager}"),
                    &format!("echo '{}'", shim.display()),
                );
                let link = tree.link(&format!("links/{quote}/codex"), &binary);
                let err = resolve(Some(&link), "codex", Some(&path))
                    .await
                    .unwrap_err();
                assert!(err.contains("another shim"), "{err}");
            }
        }
    }

    #[tokio::test]
    async fn a_link_to_a_native_binary_is_kept_across_an_upgrade() {
        let tree = Tree::new();
        let old = tree.file("Caskroom/codex/0.142.0/bin/codex", "old", 0o755);
        let link = tree.link("homebrew/bin/codex", &old);
        let path = tree.path(&["homebrew/bin"]);
        assert_eq!(
            resolve(None, "codex", Some(&path)).await,
            Ok(Some(link.clone()))
        );
        let program = executable(&host(), Some(&link), "codex")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(program.path(), link);

        let new = tree.file(
            "Caskroom/codex/0.171.0/bin/codex",
            "#!/bin/sh\necho 0.171.0\n",
            0o755,
        );
        std::fs::remove_dir_all(tree.at("Caskroom/codex/0.142.0")).unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&new, &link).unwrap();
        let ran = std::process::Command::new(program.current().await.unwrap())
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&ran.stdout), "0.171.0\n");
    }

    #[tokio::test]
    async fn a_cli_reinstalled_elsewhere_on_path_is_found_again() {
        let tree = Tree::new();
        let old = tree.file("npm/bin/codex", "old", 0o755);
        let path = tree.path(&["npm/bin", "homebrew/bin"]);
        let program = program(&host(), None, "codex", "ELSE_CODEX_BIN", Some(&path))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(program.current_in(Some(&path)).await, Ok(old.clone()));

        std::fs::remove_file(&old).unwrap();
        let new = tree.file("homebrew/bin/codex", "new", 0o755);
        assert_eq!(program.current_in(Some(&path)).await, Ok(new.clone()));
        std::fs::remove_file(&new).unwrap();
        assert_eq!(
            program.current_in(Some(&path)).await,
            Err(format!(
                "`codex` was not found at {}. Install it or set ELSE_CODEX_BIN.",
                old.display()
            ))
        );
    }

    #[tokio::test]
    async fn a_shim_is_resolved_again_once_its_version_is_removed() {
        let tree = Tree::new();
        let answer =
            |version: &str| format!("echo {}/installs/{version}/codex", tree.at("").display());
        let mise = tree.manager("bin/mise", &answer("1.0"));
        let old = tree.file("installs/1.0/codex", "old", 0o755);
        let shim = tree.link("shims/codex", &mise);
        let program = executable(&host(), Some(&shim), "codex")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(program.current().await, Ok(old));

        let new = tree.file("installs/2.0/codex", "new", 0o755);
        std::fs::remove_dir_all(tree.at("installs/1.0")).unwrap();
        tree.manager("bin/mise", &answer("2.0"));
        assert_eq!(program.current().await, Ok(new));
    }

    #[tokio::test]
    async fn a_removed_cli_names_the_file_and_the_setting() {
        let tree = Tree::new();
        let found = tree.file("bin/cursor-agent", "native", 0o755);
        let program = executable_as(&host(), Some(&found), "cursor-agent", "ELSE_CURSOR_BIN")
            .await
            .unwrap()
            .unwrap();
        std::fs::remove_file(&found).unwrap();
        assert_eq!(
            program.current().await,
            Err(format!(
                "`cursor-agent` was not found at {}. Install it or set ELSE_CURSOR_BIN.",
                found.display()
            ))
        );
    }

    #[tokio::test]
    async fn nothing_on_path_is_not_an_error() {
        let tree = Tree::new();
        tree.file("bin/codex", "data", 0o644);
        assert_eq!(
            resolve(None, "codex", Some(&tree.path(&["bin"]))).await,
            Ok(None)
        );
        assert_eq!(resolve(None, "codex", None).await, Ok(None));
    }

    #[tokio::test]
    async fn a_configured_path_wins_over_path_and_never_falls_back() {
        let tree = Tree::new();
        let configured = tree.file("custom/codex", "native", 0o755);
        tree.native("codex");
        tree.link(
            "wrappers/codex",
            &tree.manager("bin/mise", &installs_answer(&tree)),
        );
        let path = tree.path(&["wrappers"]);
        assert_eq!(
            resolve(Some(&configured), "codex", Some(&path)).await,
            Ok(Some(configured))
        );

        let missing = tree.at("nope/codex");
        let err = resolve(Some(&missing), "codex", Some(&path))
            .await
            .unwrap_err();
        assert!(err.contains("ELSE_CODEX_BIN"), "{err}");
    }

    #[tokio::test]
    async fn a_configured_mise_shim_is_resolved() {
        let tree = Tree::new();
        let native = tree.native("grok");
        let shim = tree.link(
            "shims/grok",
            &tree.manager("bin/mise", &installs_answer(&tree)),
        );
        assert_eq!(resolve(Some(&shim), "grok", None).await, Ok(Some(native)));
    }

    #[tokio::test]
    async fn an_unresolved_shim_is_an_error_naming_the_setting() {
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
                resolve(None, "codex", Some(&path)).await,
                resolve(Some(&shim), "codex", None).await,
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
        let err = resolve(None, "claude", Some(&path)).await.unwrap_err();
        assert!(err.contains("ELSE_CLAUDE_BIN"), "{err}");
        let err = resolve(Some(&asdf_shim), "claude", None).await.unwrap_err();
        assert!(err.contains("ELSE_CLAUDE_BIN"), "{err}");
    }
}
