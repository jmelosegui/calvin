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
use serde::{Deserialize, Serialize};

use crate::prices::{ModelPrice, PriceTable};

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub paths: PathsConfig,
    pub prices: PricesConfig,
    pub skills: SkillsConfig,
    pub updates: UpdatesConfig,
    pub advisor: AdvisorConfig,
}

/// "Ask for a plan" on the Opportunities page: which AI tool writes it, and how.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AdvisorConfig {
    /// `claude-code` or `command`.
    pub provider: String,
    #[serde(rename = "claude-code")]
    pub claude_code: ClaudeCodeAdvisor,
    pub command: CommandAdvisor,
}

impl Default for AdvisorConfig {
    fn default() -> Self {
        Self {
            provider: "claude-code".into(),
            claude_code: ClaudeCodeAdvisor::default(),
            command: CommandAdvisor::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ClaudeCodeAdvisor {
    /// The Claude Code executable.
    pub program: String,
    pub model: String,
    /// Spending cap per run, in USD.
    pub max_budget_usd: f64,
    /// Let the advisor read the official documentation (web fetch and search only), so
    /// its advice on features reflects the current version rather than its memory.
    pub research: bool,
    /// Where the tool's documentation index lives, for the advisor to read.
    pub docs_index: String,
}

impl Default for ClaudeCodeAdvisor {
    fn default() -> Self {
        Self {
            program: "claude".into(),
            model: "claude-sonnet-5".into(),
            max_budget_usd: 1.0,
            research: true,
            docs_index: "https://code.claude.com/docs/llms.txt".into(),
        }
    }
}

/// Any tool that reads a prompt on stdin and writes its answer to stdout.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct CommandAdvisor {
    /// Shown on the button, e.g. "Codex".
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    /// Optional: the tool's documentation index, passed to it to read.
    pub docs_index: String,
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
    /// GitHub Copilot CLI data directory (defaults to `~/.copilot`).
    pub copilot_dir: Option<PathBuf>,
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

    pub fn skill_folders(&self) -> crate::server::SkillFolders {
        crate::server::SkillFolders {
            extra: self.extra_skill_paths(),
        }
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

    pub fn copilot_dir(&self) -> Result<PathBuf> {
        Ok(self
            .paths
            .copilot_dir
            .as_deref()
            .map(expand_home)
            .unwrap_or(home()?.join(".copilot")))
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

/// Replace the `[advisor]` section of config.toml, keeping every other setting.
pub fn save_advisor(advisor: &AdvisorConfig) -> Result<()> {
    let path = config_file()?;
    let mut doc: toml::Table = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
        Err(_) => toml::Table::new(),
    };
    doc.insert("advisor".into(), toml::Value::try_from(advisor)?);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, toml::to_string_pretty(&doc)?)
        .with_context(|| format!("writing {}", path.display()))
}

/// Find a program on PATH, the way a shell would (including `.exe` / `.cmd` on Windows).
pub fn find_program(program: &str) -> Option<PathBuf> {
    let direct = PathBuf::from(program);
    if direct.components().count() > 1 {
        return direct.is_file().then_some(direct);
    }
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| {
        exts.iter()
            .map(|ext| dir.join(format!("{program}{ext}")))
            .find(|p| p.is_file())
    })
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
