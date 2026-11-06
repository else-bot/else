use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail, ensure};
use else_api::computers::runtime::{ComputerRuntime, Msb, RuntimeState};
use else_api::config::Config;
use else_api::credentials::SecretKeySource;
use else_api::providers::claude_home;

pub struct ResetPlan {
    home: PathBuf,
    data: PathBuf,
    bots: PathBuf,
    database_lock: PathBuf,
    claude_projects: Vec<PathBuf>,
}

impl ResetPlan {
    pub fn new(home: &Path, config: &Config) -> anyhow::Result<Self> {
        let configured_home = home;
        let home = fs::canonicalize(home).context("finding the installation directory")?;
        ensure!(
            home.parent().is_some()
                && Some(&home)
                    != std::env::var_os("HOME")
                        .as_ref()
                        .map(PathBuf::from)
                        .as_ref(),
            "the installation directory must not be / or HOME"
        );
        let data = home.join("data");
        ensure!(
            (config.data_dir == data || config.data_dir == configured_home.join("data"))
                && resolved(&config.data_dir)? == data,
            "reset requires ELSE_DATA_DIR to be {}",
            data.display()
        );
        let bots = resolved(&config.bots_dir)?;
        ensure!(
            (bots == home.join("bots") || bots == data.join("projects"))
                && [
                    home.join("bots"),
                    configured_home.join("bots"),
                    config.data_dir.join("projects")
                ]
                .contains(&config.bots_dir),
            "reset requires ELSE_BOTS_DIR to be inside this installation at bots or data/projects"
        );
        let database = else_api::db::options(config.require_database()?.expose())?;
        let filename = database.get_filename();
        ensure!(
            [
                data.join("else.sqlite3"),
                config.data_dir.join("else.sqlite3")
            ]
            .contains(&filename.to_path_buf())
                && resolved(filename)? == data.join("else.sqlite3"),
            "reset requires the database to be data/else.sqlite3 inside this installation"
        );
        for path in [
            &config.owner_token_file,
            &config.backup.dir,
            &data.join("microsandbox"),
        ] {
            contained(path, &data)?;
        }
        match &config.secret_key {
            SecretKeySource::File(path) => contained(path, &data)?,
            SecretKeySource::Env(_) => bail!(
                "unset SECRET_KEY and use a secret-key file inside the data directory before resetting"
            ),
        }
        for name in ["config", "logs", "run"] {
            let path = home.join(name);
            ensure!(
                resolved(&path)? == path,
                "reset refuses a symlinked {name} directory"
            );
        }
        let mut claude_projects = Vec::new();
        if let Some(host) = claude_home::host_config(
            std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        ) {
            match fs::read_dir(&bots) {
                Ok(entries) => {
                    for entry in entries {
                        let entry = entry?;
                        if uuid::Uuid::parse_str(&entry.file_name().to_string_lossy()).is_ok()
                            && entry.file_type()?.is_dir()
                        {
                            claude_projects
                                .push(claude_home::project_sessions(&host.dir, &entry.path()));
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("listing bot project directories"),
            }
        }
        Ok(Self {
            home,
            data: data.clone(),
            bots,
            database_lock: data.join("else.sqlite3.api.lock"),
            claude_projects,
        })
    }

    pub async fn stop_computers(&self, config: &Config) -> anyhow::Result<()> {
        if !self.data.join("microsandbox").exists() {
            return Ok(());
        }
        let msb = Msb::detect(&self.data, config.msb_bin.clone())
            .context("the bot computer runtime is needed to stop computers before reset")?;
        for (name, state) in msb
            .list()
            .await
            .context("listing bot computers before reset")?
        {
            if matches!(state, RuntimeState::Running | RuntimeState::Paused) {
                msb.stop(&name)
                    .await
                    .with_context(|| format!("stopping computer {name} before reset"))?;
            }
        }
        Ok(())
    }

    pub fn erase(&self) -> anyhow::Result<()> {
        for path in &self.claude_projects {
            remove(path)?;
        }
        clear_directory(
            &self.data,
            &[self.data.join("api.lock"), self.database_lock.clone()],
        )?;
        clear_directory(&self.bots, &[])?;
        clear_directory(&self.home.join("logs"), &[])?;
        clear_directory(&self.home.join("run"), &[])?;
        for entry in fs::read_dir(self.home.join("config"))? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(name.as_ref(), "desktop.json" | "desktop.layout.json")
                || (name.starts_with(".desktop.") && name.ends_with(".tmp"))
            {
                remove(&entry.path())?;
            }
        }
        Ok(())
    }
}

fn resolved(path: &Path) -> anyhow::Result<PathBuf> {
    ensure!(path.is_absolute(), "reset requires absolute storage paths");
    ensure!(
        !path.components().any(|part| part == Component::ParentDir),
        "reset refuses storage paths containing '..'"
    );
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().context("storage path has no parent")?;
            Ok(resolved(parent)?.join(path.file_name().context("storage path has no filename")?))
        }
        Err(error) => Err(error).with_context(|| format!("resolving {}", path.display())),
    }
}

fn contained(path: &Path, data: &Path) -> anyhow::Result<()> {
    let path = resolved(path)?;
    ensure!(
        path.starts_with(data) && path != data,
        "reset refuses storage outside the data directory at {}",
        path.display()
    );
    Ok(())
}

fn clear_directory(path: &Path, keep: &[PathBuf]) -> anyhow::Result<()> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    for entry in entries {
        let path = entry?.path();
        if !keep.contains(&path) {
            remove(&path)?;
        }
    }
    Ok(())
}

fn remove(path: &Path) -> anyhow::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("removing {}", path.display()))
}
