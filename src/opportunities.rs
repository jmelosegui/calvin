//! Opportunities: Claude Code features and habits you could be using and aren't, each
//! with what calvin found, why it matters, how to fix it, and the evidence.
//!
//! Every check reads only local data: calvin's database, your Claude Code settings,
//! `~/.claude.json`, and files in the projects you've worked in. Nothing is changed.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;

use crate::event::Usage;
use crate::insights::{self, Since};
use crate::prices::PriceTable;
use crate::skills::InstalledSkill;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Worth doing.
    Action,
    /// Worth a look; depends on how you work.
    Consider,
    /// Already in place.
    Good,
}

#[derive(Debug, Clone, Serialize)]
pub struct Evidence {
    pub label: String,
    pub detail: Option<String>,
    /// A calvin page to open, e.g. `/sessions#<id>` or `/skills#<name>`.
    pub link: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Opportunity {
    pub id: &'static str,
    pub area: &'static str,
    pub status: Status,
    pub title: String,
    pub finding: String,
    pub why: String,
    pub fix: String,
    /// Something to paste, if the fix is a snippet or command.
    pub snippet: Option<String>,
    /// Estimated saving in USD over the period, when it can be worked out.
    pub saving_usd: Option<f64>,
    pub evidence: Vec<Evidence>,
}

/// Everything the checks read besides the database.
pub struct Context<'a> {
    pub claude_dir: &'a Path,
    pub prices: &'a PriceTable,
    pub skills: &'a [InstalledSkill],
}

pub fn run(conn: &Connection, since: &Since, ctx: &Context) -> Result<Vec<Opportunity>> {
    let settings = Settings::load(ctx.claude_dir);
    let user_config = read_json(&home_config(ctx.claude_dir));
    let projects = project_stats(conn, since)?;

    let mut out = vec![
        claude_md(&projects),
        personal_memory(ctx.claude_dir),
        subagent_model(conn, since, ctx, &settings)?,
        custom_agents(conn, since, ctx.claude_dir, &projects)?,
        effort(conn, since, &settings)?,
        mcp_servers(conn, since, &user_config, &projects)?,
        unused_skills(conn, since, ctx.skills)?,
        skills_model_skips(conn, since, ctx.skills)?,
        repeated_prompts(conn, since)?,
        permission_friction(conn, since)?,
        shared_settings(&projects),
        hooks(&settings, &projects),
        plan_mode(conn, since)?,
        long_sessions(conn, since)?,
        log_retention(&settings),
        status_line(&settings),
    ];
    out.retain(|o| !o.title.is_empty());
    // Actions first, biggest saving first, then the rest in a stable order.
    out.sort_by(|a, b| {
        a.status.cmp(&b.status).then(
            b.saving_usd
                .unwrap_or(0.0)
                .partial_cmp(&a.saving_usd.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    Ok(out)
}

fn skip() -> Opportunity {
    Opportunity {
        id: "",
        area: "",
        status: Status::Good,
        title: String::new(),
        finding: String::new(),
        why: String::new(),
        fix: String::new(),
        snippet: None,
        saving_usd: None,
        evidence: Vec::new(),
    }
}

fn money(v: f64) -> String {
    if v >= 100.0 {
        format!("${v:.0}")
    } else {
        format!("${v:.2}")
    }
}

fn pct(part: i64, total: i64) -> i64 {
    if total == 0 {
        0
    } else {
        (part * 100 + total / 2) / total
    }
}

// ---------------------------------------------------------------------------------------
// Inputs

/// `~/.claude/settings.json` merged with `settings.local.json` (local wins).
struct Settings(Value);

impl Settings {
    fn load(claude_dir: &Path) -> Self {
        let mut merged = read_json(&claude_dir.join("settings.json"));
        if let (Value::Object(base), Value::Object(local)) = (
            &mut merged,
            read_json(&claude_dir.join("settings.local.json")),
        ) {
            for (k, v) in local {
                base.insert(k, v);
            }
        }
        Settings(merged)
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    fn env(&self, key: &str) -> Option<&str> {
        self.0.get("env")?.get(key)?.as_str()
    }
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

/// `~/.claude.json` sits next to the `.claude` folder.
fn home_config(claude_dir: &Path) -> PathBuf {
    claude_dir
        .parent()
        .unwrap_or(claude_dir)
        .join(".claude.json")
}

struct Project {
    name: String,
    root: PathBuf,
    sessions: i64,
    cost: f64,
}

/// Projects worked in during the period, by repository root, most expensive first.
fn project_stats(conn: &Connection, since: &Since) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare(
        "SELECT s.cwd, COUNT(DISTINCT s.id), COALESCE(SUM(r.cost_usd), 0)
         FROM sessions s LEFT JOIN requests r ON r.session_id = s.id
         WHERE s.cwd IS NOT NULL AND s.started_at >= ?1 GROUP BY s.cwd",
    )?;
    let mut by_root: BTreeMap<PathBuf, Project> = BTreeMap::new();
    let rows = stmt.query_map([&since.0], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, f64>(2)?,
        ))
    })?;
    for row in rows {
        let (cwd, sessions, cost) = row?;
        let dir = PathBuf::from(&cwd);
        if !dir.is_dir() {
            continue;
        }
        let root = repo_root(&dir);
        let e = by_root.entry(root.clone()).or_insert_with(|| Project {
            name: root
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| root.display().to_string()),
            root,
            sessions: 0,
            cost: 0.0,
        });
        e.sessions += sessions;
        e.cost += cost;
    }
    let mut out: Vec<Project> = by_root.into_values().collect();
    out.sort_by(|a, b| {
        b.cost
            .partial_cmp(&a.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}

/// The first parent (or the folder itself) with `.git`, else the folder.
fn repo_root(dir: &Path) -> PathBuf {
    let mut d = Some(dir);
    while let Some(p) = d {
        if p.join(".git").exists() {
            return p.to_path_buf();
        }
        d = p.parent();
    }
    dir.to_path_buf()
}

fn project_evidence(p: &Project) -> Evidence {
    Evidence {
        label: p.name.clone(),
        detail: Some(format!(
            "{} · {} session{} · {}",
            money(p.cost),
            p.sessions,
            if p.sessions == 1 { "" } else { "s" },
            p.root.display()
        )),
        link: None,
    }
}

// ---------------------------------------------------------------------------------------
// Checks

fn claude_md(projects: &[Project]) -> Opportunity {
    let has = |p: &Project| {
        ["CLAUDE.md", "CLAUDE.local.md", ".claude/CLAUDE.md"]
            .iter()
            .any(|f| p.root.join(f).is_file())
    };
    let missing: Vec<&Project> = projects.iter().filter(|p| !has(p)).collect();
    if projects.is_empty() {
        return skip();
    }
    let with = projects.len() - missing.len();
    let missing_cost: f64 = missing.iter().map(|p| p.cost).sum();
    let total_cost: f64 = projects.iter().map(|p| p.cost).sum();
    Opportunity {
        id: "claude-md",
        area: "Memory",
        status: if missing.is_empty() { Status::Good } else { Status::Action },
        title: "Give your projects a CLAUDE.md".into(),
        finding: if missing.is_empty() {
            format!("All {} projects you worked in have a CLAUDE.md.", projects.len())
        } else {
            format!(
                "{with} of {} projects have a CLAUDE.md. The {} without one account for {} of your {} spend.",
                projects.len(),
                missing.len(),
                money(missing_cost),
                money(total_cost)
            )
        },
        why: "Without it, every session starts by rediscovering how the project builds, tests and is laid out: more tool calls, more tokens, and more guessing.".into(),
        fix: "Open Claude Code in the project and run /init. It writes a CLAUDE.md from the codebase; edit it and commit it so your team gets it too.".into(),
        snippet: Some("/init".into()),
        saving_usd: None,
        evidence: missing.iter().take(12).map(|p| project_evidence(p)).collect(),
    }
}

fn personal_memory(claude_dir: &Path) -> Opportunity {
    let exists = claude_dir.join("CLAUDE.md").is_file();
    Opportunity {
        id: "personal-memory",
        area: "Memory",
        status: if exists { Status::Good } else { Status::Consider },
        title: "Keep your personal preferences in one place".into(),
        finding: if exists {
            "You have a personal ~/.claude/CLAUDE.md.".into()
        } else {
            "You have no personal ~/.claude/CLAUDE.md.".into()
        },
        why: "Instructions you repeat in every project (tone, tools you prefer, how you like commits) belong in your personal memory, which applies to every session.".into(),
        fix: "Create ~/.claude/CLAUDE.md, or run /memory in Claude Code and pick user memory.".into(),
        snippet: Some("/memory".into()),
        saving_usd: None,
        evidence: Vec::new(),
    }
}

fn is_premium(model: &str) -> bool {
    model.contains("opus") || model.contains("fable") || model.contains("mythos")
}

fn subagent_model(
    conn: &Connection,
    since: &Since,
    ctx: &Context,
    settings: &Settings,
) -> Result<Opportunity> {
    let configured = settings
        .env("CLAUDE_CODE_SUBAGENT_MODEL")
        .map(str::to_string);
    let mut stmt = conn.prepare(
        "SELECT model, COUNT(*), SUM(input_tokens), SUM(output_tokens), SUM(cache_read),
                SUM(cache_write_5m), SUM(cache_write_1h), COALESCE(SUM(cost_usd), 0)
         FROM requests WHERE is_sidechain = 1 AND ts >= ?1 AND model IS NOT NULL GROUP BY model",
    )?;
    let (mut premium_requests, mut all_requests, mut premium_cost, mut as_sonnet) =
        (0i64, 0i64, 0.0, 0.0);
    let mut evidence = Vec::new();
    let rows = stmt.query_map([&since.0], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            Usage {
                input: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                output: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                cache_read: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                cache_write_5m: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                cache_write_1h: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
            },
            r.get::<_, f64>(7)?,
        ))
    })?;
    for row in rows {
        let (model, n, usage, cost) = row?;
        all_requests += n;
        if is_premium(&model) {
            premium_requests += n;
            premium_cost += cost;
            as_sonnet += ctx.prices.cost("claude-sonnet-5", &usage).unwrap_or(cost);
            evidence.push(Evidence {
                label: model,
                detail: Some(format!("{n} subagent requests · {}", money(cost))),
                link: None,
            });
        }
    }
    if all_requests == 0 {
        return Ok(skip());
    }
    let saving = (premium_cost - as_sonnet).max(0.0);
    let status = if configured.is_some() || premium_requests == 0 {
        Status::Good
    } else if saving >= 5.0 {
        Status::Action
    } else {
        Status::Consider
    };
    Ok(Opportunity {
        id: "subagent-model",
        area: "Cost",
        status,
        title: "Run subagents on a cheaper model".into(),
        finding: match &configured {
            Some(m) => format!("Subagents are set to {m} (CLAUDE_CODE_SUBAGENT_MODEL)."),
            None => format!(
                "{premium_requests} of your {all_requests} subagent requests ran on a premium model, costing {}. On Sonnet 5 they would have cost about {}.",
                money(premium_cost),
                money(as_sonnet)
            ),
        },
        why: "Subagents mostly search, read and summarise. That rarely needs your most capable model, and they can make many requests per task.".into(),
        fix: "Set a default model for subagents in ~/.claude/settings.json. Agents that need more can still name their own model in their definition.".into(),
        snippet: Some("\"env\": { \"CLAUDE_CODE_SUBAGENT_MODEL\": \"claude-sonnet-5\" }".into()),
        saving_usd: (configured.is_none() && saving > 0.0).then_some(saving),
        evidence,
    })
}

fn custom_agents(
    conn: &Connection,
    since: &Since,
    claude_dir: &Path,
    projects: &[Project],
) -> Result<Opportunity> {
    let count_md = |dir: PathBuf| {
        std::fs::read_dir(dir)
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter(|e| e.path().extension().is_some_and(|x| x == "md"))
                    .count()
            })
            .unwrap_or(0)
    };
    let user = count_md(claude_dir.join("agents"));
    let project: usize = projects
        .iter()
        .map(|p| count_md(p.root.join(".claude").join("agents")))
        .sum();
    let agent_calls: i64 = conn.query_row(
        "SELECT COUNT(*) FROM tool_calls WHERE tool IN ('Agent', 'Task') AND ts >= ?1",
        [&since.0],
        |r| r.get(0),
    )?;
    let defined = user + project;
    Ok(Opportunity {
        id: "custom-agents",
        area: "Agents",
        status: if defined > 0 { Status::Good } else if agent_calls >= 10 { Status::Consider } else { Status::Good },
        title: "Define your own subagents".into(),
        finding: if defined > 0 {
            format!("You have {defined} custom subagent{} ({user} personal, {project} in projects).", if defined == 1 { "" } else { "s" })
        } else {
            format!("You have no custom subagents, and Claude started {agent_calls} built-in ones in this period.")
        },
        why: "A subagent you define has its own instructions, tool list and model: a reviewer that only reads, a test runner on a cheap model. Built-in ones start from scratch each time.".into(),
        fix: "Run /agents in Claude Code to create one for a job you delegate often. Save it in the project's .claude/agents to share it.".into(),
        snippet: Some("/agents".into()),
        saving_usd: None,
        evidence: Vec::new(),
    })
}

fn effort(conn: &Connection, since: &Since, settings: &Settings) -> Result<Opportunity> {
    let (total, high, high_cost): (i64, i64, f64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(effort IN ('xhigh', 'max')), 0),
                COALESCE(SUM(CASE WHEN effort IN ('xhigh', 'max') THEN cost_usd END), 0)
         FROM requests WHERE ts >= ?1 AND effort IS NOT NULL AND is_sidechain = 0",
        [&since.0],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if total == 0 {
        return Ok(skip());
    }
    let default = settings
        .get("effortLevel")
        .and_then(Value::as_str)
        .map(str::to_string);
    let share = pct(high, total);
    Ok(Opportunity {
        id: "effort",
        area: "Cost",
        status: if share >= 50 { Status::Consider } else { Status::Good },
        title: "Match effort to the task".into(),
        finding: format!(
            "{share}% of your requests ran at xhigh or max effort ({}){}.",
            money(high_cost),
            default.map(|d| format!("; your default effort is {d}")).unwrap_or_default()
        ),
        why: "Higher effort means more thinking tokens and slower answers. It pays off on hard problems; routine edits, questions and small fixes usually do as well at high or medium.".into(),
        fix: "Keep a lower default and raise it when a task is hard: /effort in Claude Code, or effortLevel in settings.json.".into(),
        snippet: Some("/effort".into()),
        saving_usd: None,
        evidence: Vec::new(),
    })
}

fn mcp_servers(
    conn: &Connection,
    since: &Since,
    user_config: &Value,
    projects: &[Project],
) -> Result<Opportunity> {
    // Configured: user scope, per-project scope in ~/.claude.json, and each project's .mcp.json.
    let mut configured: BTreeMap<String, String> = BTreeMap::new();
    if let Some(servers) = user_config.get("mcpServers").and_then(Value::as_object) {
        for name in servers.keys() {
            configured.insert(name.clone(), "all projects (~/.claude.json)".into());
        }
    }
    let roots: HashSet<String> = projects
        .iter()
        .map(|p| p.root.to_string_lossy().to_lowercase())
        .collect();
    if let Some(per_project) = user_config.get("projects").and_then(Value::as_object) {
        for (path, p) in per_project {
            if !roots.contains(&path.replace('/', "\\").to_lowercase())
                && !roots.contains(&path.to_lowercase())
            {
                continue;
            }
            if let Some(servers) = p.get("mcpServers").and_then(Value::as_object) {
                for name in servers.keys() {
                    configured
                        .entry(name.clone())
                        .or_insert_with(|| format!("project {path}"));
                }
            }
        }
    }
    for p in projects {
        if let Some(servers) = read_json(&p.root.join(".mcp.json"))
            .get("mcpServers")
            .and_then(Value::as_object)
        {
            for name in servers.keys() {
                configured
                    .entry(name.clone())
                    .or_insert_with(|| format!("{}/.mcp.json", p.name));
            }
        }
    }
    if configured.is_empty() {
        return Ok(skip());
    }
    let mut used: HashMap<String, i64> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT tool, COUNT(*) FROM tool_calls WHERE tool LIKE 'mcp__%' AND ts >= ?1 GROUP BY tool",
    )?;
    for row in stmt.query_map([&since.0], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })? {
        let (tool, n) = row?;
        if let Some(server) = tool
            .strip_prefix("mcp__")
            .and_then(|t| t.split("__").next())
        {
            *used.entry(server.to_string()).or_default() += n;
        }
    }
    let unused: Vec<(&String, &String)> = configured
        .iter()
        .filter(|(name, _)| !used.contains_key(name.as_str()))
        .collect();
    Ok(Opportunity {
        id: "mcp-unused",
        area: "Context",
        status: if unused.is_empty() { Status::Good } else { Status::Action },
        title: "Remove MCP servers you don't use".into(),
        finding: if unused.is_empty() {
            format!("All {} MCP servers you have configured were used.", configured.len())
        } else {
            format!("{} of {} configured MCP servers were never used in this period.", unused.len(), configured.len())
        },
        why: "Every configured server starts with Claude Code and adds its tools and instructions to the context of your sessions, used or not. Unused servers cost context and startup time, and some prompt for sign-in.".into(),
        fix: "Remove the ones you don't need, or keep them only in the projects that use them.".into(),
        snippet: unused.first().map(|(name, _)| format!("claude mcp remove {name}")),
        saving_usd: None,
        evidence: unused
            .iter()
            .map(|(name, scope)| Evidence { label: (*name).clone(), detail: Some(format!("configured for {scope}")), link: None })
            .collect(),
    })
}

fn unused_skills(
    conn: &Connection,
    since: &Since,
    installed: &[InstalledSkill],
) -> Result<Opportunity> {
    let usage = insights::skill_usage(conn, since, installed)?;
    let never: Vec<_> = usage
        .iter()
        .filter(|s| s.installed && s.model_runs + s.command_runs == 0)
        .collect();
    if usage.iter().all(|s| !s.installed) {
        return Ok(skip());
    }
    Ok(Opportunity {
        id: "skills-unused",
        area: "Skills",
        status: if never.is_empty() { Status::Good } else { Status::Consider },
        title: "Prune or fix skills that never run".into(),
        finding: format!(
            "{} of {} installed skills never ran in this period.",
            never.len(),
            usage.iter().filter(|s| s.installed).count()
        ),
        why: "Each skill's name and description are listed to the model in every session. Skills that never run add to that list for nothing, or have a description that doesn't match how you ask.".into(),
        fix: "Remove skills you don't need. For ones you do, rewrite the description with the words you actually use when you want it.".into(),
        snippet: None,
        saving_usd: None,
        evidence: never
            .iter()
            .take(20)
            .map(|s| Evidence {
                label: s.name.clone(),
                detail: s.kind.clone(),
                link: Some(format!("/skills#{}", s.name)),
            })
            .collect(),
    })
}

fn skills_model_skips(
    conn: &Connection,
    since: &Since,
    installed: &[InstalledSkill],
) -> Result<Opportunity> {
    let usage = insights::skill_usage(conn, since, installed)?;
    let skipped: Vec<_> = usage
        .iter()
        .filter(|s| s.command_runs >= 3 && s.model_runs * 3 < s.command_runs)
        .collect();
    if skipped.is_empty() {
        return Ok(skip());
    }
    Ok(Opportunity {
        id: "skills-model-skips",
        area: "Skills",
        status: Status::Action,
        title: "Help Claude pick your skills by itself".into(),
        finding: format!(
            "You start {} skill{} by hand far more often than Claude chooses {} on its own.",
            skipped.len(),
            if skipped.len() == 1 { "" } else { "s" },
            if skipped.len() == 1 { "it" } else { "them" }
        ),
        why: "Claude decides to use a skill from its description. If you always have to type /name, the description doesn't match the requests where you want it.".into(),
        fix: "Add the phrases you use to the skill's description (\"Triggers on: …\"), then check that the model starts picking it.".into(),
        snippet: None,
        saving_usd: None,
        evidence: skipped
            .iter()
            .map(|s| Evidence {
                label: s.name.clone(),
                detail: Some(format!("you ran it {}× · Claude chose it {}×", s.command_runs, s.model_runs)),
                link: Some(format!("/skills#{}", s.name)),
            })
            .collect(),
    })
}

fn repeated_prompts(conn: &Connection, since: &Since) -> Result<Opportunity> {
    // Short acknowledgements repeat by nature; they're not worth automating.
    const TRIVIAL: &[&str] = &[
        "yes",
        "no",
        "ok",
        "continue",
        "go",
        "thanks",
        "try again",
        "yes please",
        "clear",
        "commit",
        "push",
        "go ahead",
    ];
    let rows: Vec<_> = insights::repeated_prompts(conn, since, 4)?
        .into_iter()
        .filter(|p| {
            let key = insights::normalise_prompt(&p.example);
            key.split_whitespace().count() >= 3 && !TRIVIAL.contains(&key.as_str())
        })
        .take(10)
        .collect();
    if rows.is_empty() {
        return Ok(skip());
    }
    Ok(Opportunity {
        id: "repeated-prompts",
        area: "Automation",
        status: Status::Action,
        title: "Turn prompts you repeat into skills or commands".into(),
        finding: format!(
            "{} instruction{} typed 4 or more times.",
            rows.len(),
            if rows.len() == 1 { " was" } else { "s were" }
        ),
        why: "A skill or a CLAUDE.md rule does it the same way every time, and you stop retyping and re-explaining it.".into(),
        fix: "For a workflow, make a skill (/skill-creator can help). For a rule Claude should always follow, add it to CLAUDE.md.".into(),
        snippet: None,
        saving_usd: None,
        evidence: rows
            .iter()
            .map(|p| Evidence {
                label: p.example.chars().take(140).collect(),
                detail: Some(format!("{}× in {} session{}", p.count, p.sessions, if p.sessions == 1 { "" } else { "s" })),
                link: None,
            })
            .collect(),
    })
}

fn permission_friction(conn: &Connection, since: &Since) -> Result<Opportunity> {
    let mut stmt = conn.prepare(
        "SELECT f.kind, t.tool, t.input_json FROM friction f JOIN tool_calls t ON t.id = f.id
         WHERE f.kind IN ('rejected', 'denied') AND f.ts >= ?1",
    )?;
    let mut groups: HashMap<(String, String), i64> = HashMap::new();
    for row in stmt.query_map([&since.0], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })? {
        let (kind, tool, input) = row?;
        if tool == "AskUserQuestion" || tool == "ExitPlanMode" {
            continue; // Declining a question or a plan is conversation, not a permission.
        }
        let what = if tool == "Bash" || tool == "PowerShell" {
            let cmd = input
                .and_then(|i| serde_json::from_str::<Value>(&i).ok())
                .and_then(|v| v.get("command").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            let words: Vec<&str> = cmd
                .split_whitespace()
                .filter(|w| !w.starts_with("cd") && *w != "&&")
                .take(2)
                .collect();
            format!("{tool}({})", words.join(" "))
        } else {
            tool
        };
        *groups.entry((what, kind)).or_default() += 1;
    }
    let mut repeated: Vec<_> = groups.into_iter().filter(|(_, n)| *n >= 2).collect();
    repeated.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    if repeated.is_empty() {
        return Ok(skip());
    }
    Ok(Opportunity {
        id: "permission-rules",
        area: "Permissions",
        status: Status::Action,
        title: "Turn repeated rejections into permission rules".into(),
        finding: format!("{} kinds of tool call were rejected or blocked more than once.", repeated.len()),
        why: "If you always say no to the same command, a deny rule stops Claude trying it, which saves a turn and your attention. If a block keeps getting in your way, an allow rule removes it.".into(),
        fix: "Add rules under permissions in ~/.claude/settings.json (or the project's .claude/settings.json), or manage them with /permissions.".into(),
        snippet: Some("\"permissions\": { \"deny\": [\"Bash(git push --force:*)\"] }".into()),
        saving_usd: None,
        evidence: repeated
            .iter()
            .take(12)
            .map(|((what, kind), n)| Evidence { label: what.clone(), detail: Some(format!("{kind} {n}×")), link: None })
            .collect(),
    })
}

fn shared_settings(projects: &[Project]) -> Opportunity {
    let local_only: Vec<&Project> = projects
        .iter()
        .filter(|p| {
            p.root.join(".claude/settings.local.json").is_file()
                && !p.root.join(".claude/settings.json").is_file()
        })
        .collect();
    if local_only.is_empty() {
        return skip();
    }
    Opportunity {
        id: "shared-settings",
        area: "Team",
        status: Status::Consider,
        title: "Share project permissions with your team".into(),
        finding: format!(
            "{} project{} only personal settings (.claude/settings.local.json) and no shared .claude/settings.json.",
            local_only.len(),
            if local_only.len() == 1 { " has" } else { "s have" }
        ),
        why: "Rules everyone needs (allowed build and test commands, blocked destructive ones) belong in the shared file, committed to the repo. The local file is ignored by git and stays on your machine.".into(),
        fix: "Move the rules that apply to everyone into .claude/settings.json and commit it.".into(),
        snippet: None,
        saving_usd: None,
        evidence: local_only.iter().take(12).map(|p| project_evidence(p)).collect(),
    }
}

fn hooks(settings: &Settings, projects: &[Project]) -> Opportunity {
    let events = |v: &Value| -> Vec<String> {
        v.get("hooks")
            .and_then(Value::as_object)
            .map(|h| h.keys().cloned().collect())
            .unwrap_or_default()
    };
    let user = events(&settings.0);
    let project_hooks: Vec<(String, Vec<String>)> = projects
        .iter()
        .filter_map(|p| {
            let mut e = events(&read_json(&p.root.join(".claude/settings.json")));
            e.extend(events(&read_json(
                &p.root.join(".claude/settings.local.json"),
            )));
            (!e.is_empty()).then(|| (p.name.clone(), e))
        })
        .collect();
    let any = !user.is_empty() || !project_hooks.is_empty();
    let mut evidence: Vec<Evidence> = Vec::new();
    if !user.is_empty() {
        evidence.push(Evidence {
            label: "Personal settings".into(),
            detail: Some(user.join(", ")),
            link: None,
        });
    }
    for (name, e) in &project_hooks {
        evidence.push(Evidence {
            label: name.clone(),
            detail: Some(e.join(", ")),
            link: None,
        });
    }
    Opportunity {
        id: "hooks",
        area: "Automation",
        status: if any { Status::Good } else { Status::Consider },
        title: "Automate the routine with hooks".into(),
        finding: if any {
            format!(
                "Hooks are set up: {} in your personal settings, {} project{} with their own.",
                if user.is_empty() { "none".to_string() } else { user.join(", ") },
                project_hooks.len(),
                if project_hooks.len() == 1 { "" } else { "s" }
            )
        } else {
            "No hooks are configured.".into()
        },
        why: "Hooks run your own commands at fixed points: format a file after every edit, run a linter before a commit, get notified when Claude stops and needs you. They're reliable where instructions are only followed most of the time.".into(),
        fix: "Run /hooks in Claude Code to add one. Hooks that every contributor needs belong in the project's .claude/settings.json.".into(),
        snippet: Some("/hooks".into()),
        saving_usd: None,
        evidence,
    }
}

fn plan_mode(conn: &Connection, since: &Since) -> Result<Opportunity> {
    let mut stmt = conn.prepare(
        "SELECT s.id, COALESCE(s.title, ''), s.project,
                (SELECT COUNT(*) FROM prompts p WHERE p.session_id = s.id AND p.is_sidechain = 0) AS n,
                (SELECT COALESCE(SUM(cost_usd), 0) FROM requests r WHERE r.session_id = s.id) AS cost,
                EXISTS (SELECT 1 FROM tool_calls t WHERE t.session_id = s.id AND t.tool = 'ExitPlanMode') AS planned
         FROM sessions s WHERE s.started_at >= ?1",
    )?;
    let mut big = Vec::new();
    let mut planned_big = 0;
    for row in stmt.query_map([&since.0], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, f64>(4)?,
            r.get::<_, bool>(5)?,
        ))
    })? {
        let (id, title, project, prompts, cost, planned) = row?;
        if cost >= 25.0 || prompts >= 20 {
            if planned {
                planned_big += 1;
            } else {
                big.push((id, title, project, prompts, cost));
            }
        }
    }
    if big.is_empty() && planned_big == 0 {
        return Ok(skip());
    }
    big.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal));
    let total = big.len() + planned_big;
    Ok(Opportunity {
        id: "plan-mode",
        area: "Workflow",
        status: if big.len() * 2 > total { Status::Consider } else { Status::Good },
        title: "Plan big tasks before building".into(),
        finding: format!("{} of your {total} largest sessions (20+ prompts or $25+) never used plan mode.", big.len()),
        why: "In plan mode Claude reads and proposes before it changes anything, so you agree on the approach first. Big tasks started without a plan tend to drift and need more corrections.".into(),
        fix: "Press Shift+Tab until plan mode is on before describing a large task, or start with /plan.".into(),
        snippet: None,
        saving_usd: None,
        evidence: big
            .iter()
            .take(10)
            .map(|(id, title, project, prompts, cost)| Evidence {
                label: if title.is_empty() { "(untitled session)".into() } else { title.clone() },
                detail: Some(format!("{} · {prompts} prompts · {}", project.clone().unwrap_or_default(), money(*cost))),
                link: Some(format!("/sessions#{id}")),
            })
            .collect(),
    })
}

fn long_sessions(conn: &Connection, since: &Since) -> Result<Opportunity> {
    let rows = insights::sessions(conn, since, 5000)?;
    let mut heavy: Vec<_> = rows.iter().filter(|s| s.cost_usd >= 75.0).collect();
    if heavy.is_empty() {
        return Ok(skip());
    }
    heavy.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let heavy_cost: f64 = heavy.iter().map(|s| s.cost_usd).sum();
    Ok(Opportunity {
        id: "long-sessions",
        area: "Context",
        status: Status::Consider,
        title: "Start fresh sessions for new tasks".into(),
        finding: format!(
            "{} session{} cost over $75 each ({} in total).",
            heavy.len(),
            if heavy.len() == 1 { "" } else { "s" },
            money(heavy_cost)
        ),
        why: "Each turn re-sends the whole conversation. Long sessions that move from task to task carry old context that no longer helps, which costs tokens and can confuse the model.".into(),
        fix: "When you switch tasks, use /clear or start a new session. For long work, ask for a summary and continue from it.".into(),
        snippet: Some("/clear".into()),
        saving_usd: None,
        evidence: heavy
            .iter()
            .take(10)
            .map(|s| Evidence {
                label: s.title.clone().or_else(|| s.first_prompt.clone()).unwrap_or_else(|| "(untitled)".into()),
                detail: Some(format!("{} · {} prompts · {}", s.project.clone().unwrap_or_default(), s.prompts, money(s.cost_usd))),
                link: Some(format!("/sessions#{}", s.id)),
            })
            .collect(),
    })
}

fn log_retention(settings: &Settings) -> Opportunity {
    let days = settings.get("cleanupPeriodDays").and_then(Value::as_i64);
    Opportunity {
        id: "log-retention",
        area: "Data",
        status: match days {
            Some(d) if d >= 90 => Status::Good,
            _ => Status::Consider,
        },
        title: "Keep your session history longer".into(),
        finding: match days {
            Some(d) => format!("Claude Code keeps session logs for {d} days (cleanupPeriodDays)."),
            None => "Claude Code deletes session logs after 30 days (the default).".into(),
        },
        why: "calvin keeps its own copy, but only of what it has imported while running. Longer retention also lets you resume and search older sessions in Claude Code itself.".into(),
        fix: "Raise cleanupPeriodDays in ~/.claude/settings.json.".into(),
        snippet: Some("\"cleanupPeriodDays\": 365".into()),
        saving_usd: None,
        evidence: Vec::new(),
    }
}

fn status_line(settings: &Settings) -> Opportunity {
    let has = settings.get("statusLine").is_some();
    Opportunity {
        id: "status-line",
        area: "Workflow",
        status: if has { Status::Good } else { Status::Consider },
        title: "Show context and cost in the status line".into(),
        finding: if has { "You have a custom status line.".into() } else { "You use the default status line.".into() },
        why: "A status line that shows the model, context used and session cost makes it obvious when to /clear or switch models.".into(),
        fix: "Run /statusline in Claude Code and describe what you want to see.".into(),
        snippet: Some("/statusline".into()),
        saving_usd: None,
        evidence: Vec::new(),
    }
}
