//! What you actually use in your AI coding tool: tools, commands, settings, hooks,
//! plugins, custom agents and so on. calvin doesn't keep a list of features that exist;
//! the advisor compares this inventory against its own tool's current feature set.
//!
//! Only names and counts are collected, never setting values that could be secrets.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;

use crate::insights::Since;

#[derive(Debug, Default)]
pub struct Inventory {
    pub harness_version: Option<String>,
    pub builtin_tools: Vec<(String, i64)>,
    pub mcp_servers_used: Vec<(String, i64)>,
    pub commands_typed: Vec<(String, i64)>,
    pub settings_keys: Vec<String>,
    pub env_vars: Vec<String>,
    pub hook_events: BTreeMap<String, Vec<String>>,
    pub permission_rules: BTreeMap<String, usize>,
    pub plugins: Vec<(String, Option<i64>)>,
    pub custom_agents: usize,
    pub custom_commands: usize,
    pub output_styles: usize,
    pub keybindings: bool,
    pub personal_memory: bool,
}

pub fn collect(conn: &Connection, since: &Since, claude_dir: &Path) -> Result<Inventory> {
    let mut inv = Inventory {
        harness_version: conn
            .query_row(
                "SELECT cli_version FROM sessions WHERE cli_version IS NOT NULL ORDER BY ended_at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .ok(),
        ..Default::default()
    };

    let mut stmt = conn.prepare(
        "SELECT tool, COUNT(*) FROM tool_calls WHERE ts >= ?1 GROUP BY tool ORDER BY 2 DESC, 1",
    )?;
    let mut mcp: BTreeMap<String, i64> = BTreeMap::new();
    for row in stmt.query_map([&since.0], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })? {
        let (tool, n) = row?;
        match tool
            .strip_prefix("mcp__")
            .and_then(|t| t.split("__").next())
        {
            Some(server) => *mcp.entry(server.to_string()).or_default() += n,
            None => inv.builtin_tools.push((tool, n)),
        }
    }
    inv.mcp_servers_used = mcp.into_iter().collect();
    inv.mcp_servers_used
        .sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut commands: BTreeMap<String, i64> = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT text FROM prompts WHERE kind = 'command' AND is_sidechain = 0 AND ts >= ?1",
    )?;
    for row in stmt.query_map([&since.0], |r| r.get::<_, String>(0))? {
        let text = row?;
        let name = text.split_whitespace().next().unwrap_or(&text).to_string();
        *commands.entry(name).or_default() += 1;
    }
    inv.commands_typed = commands.into_iter().collect();
    inv.commands_typed
        .sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // Settings: user, then local. Key names only.
    let mut keys = BTreeSet::new();
    let mut env = BTreeSet::new();
    for file in ["settings.json", "settings.local.json"] {
        let v = read_json(&claude_dir.join(file));
        if let Some(obj) = v.as_object() {
            keys.extend(obj.keys().cloned());
            if let Some(e) = obj.get("env").and_then(Value::as_object) {
                env.extend(e.keys().cloned());
            }
            if let Some(h) = obj.get("hooks").and_then(Value::as_object) {
                inv.hook_events
                    .insert(format!("~/.claude/{file}"), h.keys().cloned().collect());
            }
            if let Some(p) = obj.get("permissions").and_then(Value::as_object) {
                for (kind, rules) in p {
                    if let Some(list) = rules.as_array() {
                        *inv.permission_rules.entry(kind.clone()).or_default() += list.len();
                    }
                }
            }
        }
    }
    inv.settings_keys = keys.into_iter().collect();
    inv.env_vars = env.into_iter().collect();

    // Hooks configured in projects you worked in.
    let mut stmt = conn
        .prepare("SELECT DISTINCT cwd FROM sessions WHERE cwd IS NOT NULL AND started_at >= ?1")?;
    for row in stmt.query_map([&since.0], |r| r.get::<_, String>(0))? {
        let dir = std::path::PathBuf::from(row?);
        for file in ["settings.json", "settings.local.json"] {
            let v = read_json(&dir.join(".claude").join(file));
            if let Some(h) = v.get("hooks").and_then(Value::as_object) {
                let label = format!(
                    "{}/.claude/{file}",
                    dir.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                );
                inv.hook_events.insert(label, h.keys().cloned().collect());
            }
        }
    }

    // Plugins enabled, with Claude Code's own usage counter.
    let settings = read_json(&claude_dir.join("settings.json"));
    let usage = read_json(
        &claude_dir
            .parent()
            .unwrap_or(claude_dir)
            .join(".claude.json"),
    );
    if let Some(enabled) = settings.get("enabledPlugins").and_then(Value::as_object) {
        for name in enabled.keys() {
            let count = usage
                .pointer(&format!(
                    "/pluginUsage/{}/usageCount",
                    name.replace('/', "~1")
                ))
                .and_then(Value::as_i64);
            inv.plugins.push((name.clone(), count));
        }
    }

    let count_files = |dir: &Path| {
        std::fs::read_dir(dir)
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter(|e| e.path().is_file())
                    .count()
            })
            .unwrap_or(0)
    };
    inv.custom_agents = count_files(&claude_dir.join("agents"));
    inv.custom_commands = count_files(&claude_dir.join("commands"));
    inv.output_styles = count_files(&claude_dir.join("output-styles"));
    inv.keybindings = claude_dir.join("keybindings.json").is_file();
    inv.personal_memory = claude_dir.join("CLAUDE.md").is_file();
    Ok(inv)
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn list(items: &[(String, i64)]) -> String {
    if items.is_empty() {
        return "none".into();
    }
    items
        .iter()
        .map(|(n, c)| format!("{n} ({c})"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Inventory {
    pub fn to_markdown(&self, harness: &str) -> String {
        let mut s = format!("## How you use {harness} today\n");
        if let Some(v) = &self.harness_version {
            s.push_str(&format!("- {harness} version: {v}\n"));
        }
        s.push_str(&format!(
            "- Built-in tools the agent used (calls): {}\n",
            list(&self.builtin_tools)
        ));
        s.push_str(&format!(
            "- MCP servers used (calls): {}\n",
            list(&self.mcp_servers_used)
        ));
        s.push_str(&format!(
            "- Slash commands you typed: {}\n",
            list(&self.commands_typed)
        ));
        s.push_str(&format!(
            "- Settings in use (keys only): {}\n",
            self.settings_keys.join(", ")
        ));
        s.push_str(&format!(
            "- Environment variables set in settings: {}\n",
            if self.env_vars.is_empty() {
                "none".into()
            } else {
                self.env_vars.join(", ")
            }
        ));
        if self.hook_events.is_empty() {
            s.push_str("- Hooks: none configured\n");
        } else {
            for (file, events) in &self.hook_events {
                s.push_str(&format!("- Hooks in {file}: {}\n", events.join(", ")));
            }
        }
        let rules: Vec<String> = self
            .permission_rules
            .iter()
            .map(|(k, n)| format!("{k} {n}"))
            .collect();
        s.push_str(&format!(
            "- Permission rules: {}\n",
            if rules.is_empty() {
                "none".into()
            } else {
                rules.join(", ")
            }
        ));
        let plugins: Vec<String> = self
            .plugins
            .iter()
            .map(|(n, c)| match c {
                Some(c) => format!("{n} (used {c} times)"),
                None => n.clone(),
            })
            .collect();
        s.push_str(&format!(
            "- Plugins enabled: {}\n",
            if plugins.is_empty() {
                "none".into()
            } else {
                plugins.join(", ")
            }
        ));
        s.push_str(&format!(
            "- Custom subagents: {} · custom slash commands: {} · output styles: {} · custom keybindings: {} · personal CLAUDE.md: {}\n",
            self.custom_agents,
            self.custom_commands,
            self.output_styles,
            if self.keybindings { "yes" } else { "no" },
            if self.personal_memory { "yes" } else { "no" }
        ));
        s
    }
}
