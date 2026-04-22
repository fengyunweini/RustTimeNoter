//! Resolve data / config / log directories.
//! HKCU mode: `%LOCALAPPDATA%\RustTimeNoter\...`
//! Service mode: `%PROGRAMDATA%\RustTimeNoter\...`

use std::path::{Path, PathBuf};

use crate::APP_NAME;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallScope {
    /// Per-user (HKCU autostart).
    User,
    /// Machine-wide (Windows service running as LocalSystem).
    Machine,
}

impl InstallScope {
    pub fn detect() -> Self {
        // Honor explicit env var first (set by service entry).
        if std::env::var_os("RUSTTIMENOTER_SCOPE")
            .map(|v| v.eq_ignore_ascii_case("machine"))
            .unwrap_or(false)
        {
            return InstallScope::Machine;
        }
        InstallScope::User
    }
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub rules_file: PathBuf,
    pub data_dir: PathBuf,
    pub apps_dict: PathBuf,
    pub titles_dict: PathBuf,
    pub key_file: PathBuf,
    pub crash_log: PathBuf,
    pub bin_dir: PathBuf,
}

impl AppPaths {
    pub fn for_scope(scope: InstallScope) -> std::io::Result<Self> {
        let root = scope_root(scope)?;
        Ok(Self::from_root(&root))
    }

    pub fn from_root(root: &Path) -> Self {
        let data_dir = root.join("data");
        AppPaths {
            config_file: root.join("config.toml"),
            rules_file: root.join("rules.toml"),
            apps_dict: root.join("apps.dict"),
            titles_dict: root.join("titles.dict"),
            key_file: root.join("key.bin"),
            crash_log: root.join("crash.log"),
            bin_dir: root.join("bin"),
            data_dir,
            root: root.to_path_buf(),
        }
    }

    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(&self.bin_dir)?;
        Ok(())
    }

    /// `data_dir/YYYY/MM/YYYY-MM-DD.log`
    pub fn log_file_for_day(&self, year: i32, month: u32, day: u32) -> PathBuf {
        self.data_dir
            .join(format!("{year:04}"))
            .join(format!("{month:02}"))
            .join(format!("{year:04}-{month:02}-{day:02}.log"))
    }
}

fn scope_root(scope: InstallScope) -> std::io::Result<PathBuf> {
    let env_var = match scope {
        InstallScope::User => "LOCALAPPDATA",
        InstallScope::Machine => "PROGRAMDATA",
    };
    let base = std::env::var_os(env_var).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("environment variable {env_var} not set"),
        )
    })?;
    Ok(PathBuf::from(base).join(APP_NAME))
}
