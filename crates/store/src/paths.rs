use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use directories::ProjectDirs;

/// Where SnapIt keeps its files. `SNAPIT_HOME` overrides both roots.
#[derive(Clone, Debug)]
pub struct AppDirs {
    config_dir: PathBuf,
    data_dir: PathBuf,
}

impl AppDirs {
    pub fn discover() -> Result<Self> {
        if let Some(home) = std::env::var_os("SNAPIT_HOME") {
            return Ok(Self::at(home));
        }
        let dirs = ProjectDirs::from("dev", "SnapIt", "SnapIt")
            .ok_or_else(|| anyhow!("no home directory found"))?;
        Ok(Self {
            config_dir: dirs.config_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
        })
    }

    /// Keep everything under one root.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            config_dir: root.clone(),
            data_dir: root,
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn history_dir(&self) -> PathBuf {
        self.data_dir.join("history")
    }

    /// Persistent diagnostic log. Kept beside history so `SNAPIT_HOME` and the
    /// per-platform data root both contain everything needed to file a report.
    pub fn log_file(&self) -> PathBuf {
        self.data_dir.join("logs").join("snapit.log")
    }
}
