//! What you actually use in your AI coding tool: tools, commands, settings, hooks,
//! plugins, custom agents and so on. calvin doesn't keep a list of features that exist;
//! the advisor compares this inventory against its own tool's current feature set.
//!
//! Only names and counts are collected, never setting values that could be secrets.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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

pub fn copilot_markdown(conn: &Connection, since: &Since, copilot_dir: &Path) -> Result<String> {
    let version: Option<String> = conn
        .query_row(
            "SELECT cli_version FROM sessions WHERE harness = 'copilot-cli'
             AND cli_version IS NOT NULL ORDER BY ended_at DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    let mut tools = Vec::new();
    let mut mcp_used = BTreeMap::<String, i64>::new();
    let mut stmt = conn.prepare(
        "SELECT t.tool, COUNT(*) FROM tool_calls t JOIN sessions s ON s.id = t.session_id
         WHERE s.harness = 'copilot-cli' AND t.ts >= ?1 GROUP BY t.tool ORDER BY 2 DESC, 1",
    )?;
    for row in stmt.query_map([&since.0], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })? {
        let (tool, count) = row?;
        if let Some(server) = tool
            .strip_prefix("mcp__")
            .and_then(|name| name.split("__").next())
        {
            *mcp_used.entry(server.to_string()).or_default() += count;
        } else {
            tools.push((tool, count));
        }
    }
    let mut models = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT r.model, COUNT(*) FROM requests r JOIN sessions s ON s.id = r.session_id
         WHERE s.harness = 'copilot-cli' AND r.ts >= ?1 AND r.model IS NOT NULL
         GROUP BY r.model ORDER BY 2 DESC, 1",
    )?;
    for row in stmt.query_map([&since.0], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })? {
        models.push(row?);
    }
    let mut modes = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT m.mode, COUNT(*) FROM session_modes m JOIN sessions s ON s.id = m.session_id
         WHERE s.harness = 'copilot-cli' AND m.ts >= ?1
         GROUP BY m.mode ORDER BY 2 DESC, 1",
    )?;
    for row in stmt.query_map([&since.0], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })? {
        modes.push(row?);
    }
    let mut commands = BTreeMap::<String, i64>::new();
    let mut stmt = conn.prepare(
        "SELECT p.text FROM prompts p JOIN sessions s ON s.id = p.session_id
         WHERE s.harness = 'copilot-cli' AND p.kind = 'command' AND p.ts >= ?1",
    )?;
    for row in stmt.query_map([&since.0], |r| r.get::<_, String>(0))? {
        let text = row?;
        let command = text.split_whitespace().next().unwrap_or(&text).to_string();
        *commands.entry(command).or_default() += 1;
    }
    let settings = read_json(&copilot_dir.join("settings.json"));
    let mut setting_keys = settings
        .as_object()
        .map(|o| o.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    setting_keys.sort();
    let mcp = read_json(&copilot_dir.join("mcp-config.json"));
    let mut mcp_servers = mcp
        .get("mcpServers")
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    mcp_servers.sort();
    let permission_locations = read_json(&copilot_dir.join("permissions-config.json"))
        .get("locations")
        .and_then(Value::as_object)
        .map(|o| o.len())
        .unwrap_or(0);
    let mut plugins = entry_names(&copilot_dir.join("installed-plugins"));
    plugins.sort();
    let mut agents = entry_names(&copilot_dir.join("agents"));
    let mut skills = entry_names(&copilot_dir.join("skills"));
    let mut stmt = conn.prepare(
        "SELECT DISTINCT cwd FROM sessions
         WHERE harness = 'copilot-cli' AND cwd IS NOT NULL AND started_at >= ?1",
    )?;
    for row in stmt.query_map([&since.0], |r| r.get::<_, String>(0))? {
        let root = inventory_repo_root(&PathBuf::from(row?));
        agents.extend(entry_names(&root.join(".github").join("agents")));
        skills.extend(entry_names(&root.join(".github").join("skills")));
    }
    agents.sort();
    agents.dedup();
    skills.sort();
    skills.dedup();
    let mut out = String::from("## How you use Copilot CLI today\n");
    if let Some(version) = version {
        out.push_str(&format!("- Copilot CLI version: {version}\n"));
    }
    out.push_str(&format!("- Tools used (calls): {}\n", list(&tools)));
    out.push_str(&format!("- Models used (requests): {}\n", list(&models)));
    out.push_str(&format!(
        "- Modes entered: {}\n",
        if modes.is_empty() {
            "interactive only or unavailable".into()
        } else {
            list(&modes)
        }
    ));
    out.push_str(&format!(
        "- Slash commands typed: {}\n",
        list(&commands.into_iter().collect::<Vec<_>>())
    ));
    out.push_str(&format!(
        "- Settings in use (keys only): {}\n",
        if setting_keys.is_empty() {
            "none".into()
        } else {
            setting_keys.join(", ")
        }
    ));
    out.push_str(&format!(
        "- MCP servers configured (names only): {}\n",
        if mcp_servers.is_empty() {
            "none".into()
        } else {
            mcp_servers.join(", ")
        }
    ));
    out.push_str(&format!(
        "- MCP servers observed in tool calls: {}\n",
        list(&mcp_used.into_iter().collect::<Vec<_>>())
    ));
    out.push_str(&format!(
        "- Permission locations configured: {permission_locations}\n"
    ));
    out.push_str(&format!(
        "- Installed plugins: {}\n",
        if plugins.is_empty() {
            "none".into()
        } else {
            plugins.join(", ")
        }
    ));
    out.push_str(&format!(
        "- Custom agents discovered: {} · skills discovered: {}\n",
        if agents.is_empty() {
            "none".into()
        } else {
            agents.join(", ")
        },
        if skills.is_empty() {
            "none".into()
        } else {
            skills.join(", ")
        }
    ));
    out.push_str(
        "- Official documentation: https://docs.github.com/en/copilot/how-tos/copilot-cli\n",
    );
    Ok(out)
}

pub fn cursor_markdown(
    conn: &Connection,
    since: &Since,
    cursor_dir: &Path,
    cursor_state_db: &Path,
) -> Result<String> {
    let sessions: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sessions WHERE harness = 'cursor' AND started_at >= ?1",
        [&since.0],
        |row| row.get(0),
    )?;
    if sessions == 0 {
        return Ok(String::new());
    }
    let query_counts = |sql: &str| -> Result<Vec<(String, i64)>> {
        let mut statement = conn.prepare(sql)?;
        Ok(statement
            .query_map([&since.0], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?)
    };
    let tools = query_counts(
        "SELECT t.tool, COUNT(*) FROM tool_calls t JOIN sessions s ON s.id = t.session_id
         WHERE s.harness = 'cursor' AND t.ts >= ?1 GROUP BY t.tool ORDER BY 2 DESC, 1",
    )?;
    let models = query_counts(
        "SELECT r.model, COUNT(*) FROM requests r JOIN sessions s ON s.id = r.session_id
         WHERE s.harness = 'cursor' AND r.ts >= ?1 AND r.model IS NOT NULL
         GROUP BY r.model ORDER BY 2 DESC, 1",
    )?;
    let modes = query_counts(
        "SELECT lower(m.mode), COUNT(*) FROM session_modes m
         JOIN sessions s ON s.id = m.session_id
         WHERE s.harness = 'cursor' AND m.ts >= ?1
         GROUP BY lower(m.mode) ORDER BY 2 DESC, 1",
    )?;
    let mut commands = BTreeMap::<String, i64>::new();
    let mut statement = conn.prepare(
        "SELECT p.text FROM prompts p JOIN sessions s ON s.id = p.session_id
         WHERE s.harness = 'cursor' AND p.kind = 'command' AND p.ts >= ?1",
    )?;
    for row in statement.query_map([&since.0], |row| row.get::<_, String>(0))? {
        let text = row?;
        let command = text.split_whitespace().next().unwrap_or(&text).to_string();
        *commands.entry(command).or_default() += 1;
    }
    let mut roots = BTreeSet::new();
    let mut statement = conn.prepare(
        "SELECT DISTINCT cwd FROM sessions
         WHERE harness = 'cursor' AND cwd IS NOT NULL AND started_at >= ?1",
    )?;
    for row in statement.query_map([&since.0], |row| row.get::<_, String>(0))? {
        roots.insert(inventory_repo_root(&PathBuf::from(row?)));
    }
    let count_entries = |dir: &Path| {
        std::fs::read_dir(dir)
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or(0)
    };
    let settings = cursor_state_db
        .parent()
        .and_then(Path::parent)
        .map(|user| read_json(&user.join("settings.json")))
        .unwrap_or(Value::Null);
    let mut setting_keys = settings
        .as_object()
        .map(|object| object.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    setting_keys.sort();
    let cli_config = read_json(&cursor_dir.join("cli-config.json"));
    let mut cli_keys = cli_config
        .as_object()
        .map(|object| object.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    cli_keys.sort();
    let mut mcp_servers = read_json(&cursor_dir.join("mcp.json"))
        .get("mcpServers")
        .and_then(Value::as_object)
        .map(|object| object.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    mcp_servers.sort();
    let agents = count_entries(&cursor_dir.join("agents"))
        + roots
            .iter()
            .map(|root| count_entries(&root.join(".cursor").join("agents")))
            .sum::<usize>();
    let skills = count_entries(&cursor_dir.join("skills-cursor"))
        + roots
            .iter()
            .map(|root| count_entries(&root.join(".cursor").join("skills")))
            .sum::<usize>();
    let rules = count_entries(&cursor_dir.join("rules"))
        + roots
            .iter()
            .map(|root| count_entries(&root.join(".cursor").join("rules")))
            .sum::<usize>();
    let hooks = usize::from(cursor_dir.join("hooks.json").is_file())
        + roots
            .iter()
            .filter(|root| root.join(".cursor").join("hooks.json").is_file())
            .count();
    let worktree_setups = roots
        .iter()
        .filter(|root| root.join(".cursor").join("worktrees.json").is_file())
        .count();

    let mut out = String::from("## How you use Cursor today\n");
    out.push_str(&format!("- Tools used (calls): {}\n", list(&tools)));
    out.push_str(&format!("- Models used (requests): {}\n", list(&models)));
    out.push_str(&format!("- Modes entered: {}\n", list(&modes)));
    out.push_str(&format!(
        "- Slash commands typed: {}\n",
        list(&commands.into_iter().collect::<Vec<_>>())
    ));
    out.push_str(&format!(
        "- Cursor settings in use (keys only): {}\n",
        if setting_keys.is_empty() {
            "none".into()
        } else {
            setting_keys.join(", ")
        }
    ));
    out.push_str(&format!(
        "- Cursor CLI config in use (keys only): {}\n",
        if cli_keys.is_empty() {
            "none".into()
        } else {
            cli_keys.join(", ")
        }
    ));
    out.push_str(&format!(
        "- MCP servers configured (names only): {}\n",
        if mcp_servers.is_empty() {
            "none".into()
        } else {
            mcp_servers.join(", ")
        }
    ));
    out.push_str(&format!(
        "- Cursor customization: {rules} rule file(s), {hooks} hooks file(s), {agents} custom subagent(s), {skills} skill(s), {worktree_setups} worktree setup file(s)\n"
    ));
    out.push_str("- Official documentation: https://cursor.com/docs/llms.txt\n");
    Ok(out)
}

fn entry_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| {
                    let path = entry.path();
                    path.file_stem()
                        .or_else(|| path.file_name())
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn inventory_repo_root(path: &Path) -> PathBuf {
    path.ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .unwrap_or(path)
        .to_path_buf()
}
