//! Where things live, and the optional user config file.
//!
//! Environment overrides (mainly for tests and unusual setups):
//! - `CALVIN_DATA_DIR`: directory for the database and state files
//! - `CALVIN_CONFIG`: path to config.toml
//! - `CLAUDE_CONFIG_DIR`: Claude Code's own override for `~/.claude`

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use directories::{BaseDirs, ProjectDirs};
use serde::Deserialize;

use crate::prices::{ModelPrice, PriceTable};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub paths: PathsConfig,
    pub prices: PricesConfig,
    pub skills: SkillsConfig,
    pub updates: UpdatesConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct UpdatesConfig {
    /// Once a day, ask GitHub whether a newer calvin exists. Sends no usage data.
    pub check: bool,
}

impl Default for UpdatesConfig {
    fn default() -> Self {
        Self { check: true }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Extra folders containing skills (each skill is a folder with a SKILL.md).
    pub extra_paths: Vec<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// Claude Code config directory (defaults to `~/.claude`).
    pub claude_dir: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PricesConfig {
    pub models: BTreeMap<String, ModelPrice>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_file()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn price_table(&self) -> PriceTable {
        PriceTable::with_overrides(&self.prices.models)
    }

    pub fn extra_skill_paths(&self) -> Vec<PathBuf> {
        self.skills
            .extra_paths
            .iter()
            .map(|p| expand_home(p))
            .collect()
    }

    pub fn claude_dir(&self) -> Result<PathBuf> {
        if let Some(dir) = &self.paths.claude_dir {
            return Ok(expand_home(dir));
        }
        if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
            return Ok(PathBuf::from(dir));
        }
        Ok(home()?.join(".claude"))
    }
}

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("", "", "calvin")
        .ok_or_else(|| anyhow!("could not determine a home directory"))
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CALVIN_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(project_dirs()?.data_local_dir().to_path_buf())
}

pub fn config_file() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CALVIN_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    Ok(project_dirs()?.config_dir().join("config.toml"))
}

pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("calvin.db"))
}

fn home() -> Result<PathBuf> {
    BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .ok_or_else(|| anyhow!("could not determine a home directory"))
}

fn expand_home(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home()
            .map(|h| h.join(rest))
            .unwrap_or_else(|_| p.to_path_buf()),
        Err(_) => p.to_path_buf(),
    }
}
