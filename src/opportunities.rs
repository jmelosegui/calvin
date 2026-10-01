//! Opportunities: Claude Code features and habits you could be using and aren't, each
//! with what calvin found, why it matters, how to fix it, and the evidence.
//!
//! Every check reads only local data: calvin's database, your Claude Code settings,
//! `~/.claude.json`, and files in the projects you've worked in. Nothing is changed.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
    /// One number to track over time, so a fix shows up as a trend.
    pub metric: Option<Metric>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Metric {
    pub value: f64,
    pub label: String,
    pub lower_is_better: bool,
}

impl Metric {
    fn lower(value: impl Into<f64>, label: &str) -> Option<Metric> {
        Some(Metric {
            value: value.into(),
            label: label.into(),
            lower_is_better: true,
        })
    }
    fn higher(value: impl Into<f64>, label: &str) -> Option<Metric> {
        Some(Metric {
            value: value.into(),
            label: label.into(),
            lower_is_better: false,
        })
    }
}

/// Everything the checks read besides the database.
pub struct Context<'a> {
    pub claude_dir: &'a Path,
    pub copilot_dir: &'a Path,
    pub prices: &'a PriceTable,
    pub skills: &'a [InstalledSkill],
}

pub fn run(conn: &Connection, since: &Since, ctx: &Context) -> Result<Vec<Opportunity>> {
    let mut out = vec![
        shared_agents_md(conn, since)?,
        copilot_project_instructions(conn, since)?,
        copilot_plan_mode(conn, since)?,
        copilot_compact(conn, since)?,
        copilot_review(conn, since)?,
        copilot_autopilot(conn, since)?,
    ];
    out.extend(copilot_feature_catalog(conn, since, ctx.copilot_dir)?);
    conn.execute_batch(
        "CREATE TEMP VIEW sessions AS SELECT * FROM main.sessions WHERE harness = 'claude-code';
         CREATE TEMP VIEW prompts AS SELECT p.* FROM main.prompts p
            JOIN main.sessions s ON s.id = p.session_id WHERE s.harness = 'claude-code';
         CREATE TEMP VIEW requests AS SELECT r.* FROM main.requests r
            JOIN main.sessions s ON s.id = r.session_id WHERE s.harness = 'claude-code';
         CREATE TEMP VIEW tool_calls AS SELECT t.* FROM main.tool_calls t
            JOIN main.sessions s ON s.id = t.session_id WHERE s.harness = 'claude-code';
         CREATE TEMP VIEW friction AS SELECT f.* FROM main.friction f
            JOIN main.sessions s ON s.id = f.session_id WHERE s.harness = 'claude-code';",
    )?;
    let settings = Settings::load(ctx.claude_dir);
    let user_config = read_json(&home_config(ctx.claude_dir));
    let provider = (|| -> Result<Vec<Opportunity>> {
        let projects = project_stats(conn, since)?;
        Ok(vec![
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
        ])
    })();
    conn.execute_batch(
        "DROP VIEW temp.friction;
         DROP VIEW temp.tool_calls;
         DROP VIEW temp.requests;
         DROP VIEW temp.prompts;
         DROP VIEW temp.sessions;",
    )?;
    out.extend(provider?);
    out.retain(|o| !o.title.is_empty());
    // Actions first, biggest saving first, then by id: the same inputs always give the
    // same order.
    out.sort_by(|a, b| {
        a.status
            .cmp(&b.status)
            .then(
                b.saving_usd
                    .unwrap_or(0.0)
                    .total_cmp(&a.saving_usd.unwrap_or(0.0)),
            )
            .then(a.id.cmp(b.id))
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
        metric: None,
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

fn shared_agents_md(conn: &Connection, since: &Since) -> Result<Opportunity> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT s.cwd, s.harness FROM sessions s
         WHERE s.cwd IS NOT NULL AND s.started_at >= ?1
           AND s.harness IN ('claude-code', 'copilot-cli')",
    )?;
    let mut roots: BTreeMap<PathBuf, HashSet<String>> = BTreeMap::new();
    for row in stmt.query_map([&since.0], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (cwd, harness) = row?;
        let path = PathBuf::from(cwd);
        if path.is_dir() {
            roots.entry(repo_root(&path)).or_default().insert(harness);
        }
    }
    let shared: Vec<PathBuf> = roots
        .into_iter()
        .filter(|(_, harnesses)| harnesses.len() == 2)
        .map(|(root, _)| root)
        .collect();
    if shared.is_empty() {
        return Ok(skip());
    }

    let mut needs_work = Vec::new();
    let mut consolidated = 0usize;
    for root in &shared {
        let agents = root.join("AGENTS.md");
        let claude = root.join("CLAUDE.md");
        let imports_agents = std::fs::read_to_string(&claude)
            .is_ok_and(|text| text.lines().any(|line| line.trim() == "@AGENTS.md"));
        if agents.is_file() && (!claude.is_file() || imports_agents) {
            consolidated += 1;
        } else {
            needs_work.push(root);
        }
    }
    let missing = shared.len() - consolidated;
    Ok(Opportunity {
        id: "shared-agents-md",
        area: "Cross-tool",
        status: if missing == 0 {
            Status::Good
        } else {
            Status::Action
        },
        title: "Keep shared project instructions in AGENTS.md".into(),
        finding: if missing == 0 {
            format!(
                "All {} projects used with both Claude Code and Copilot CLI have one shared instruction source.",
                shared.len()
            )
        } else {
            format!(
                "{missing} of {} projects used with both tools duplicate instructions or do not have a canonical AGENTS.md.",
                shared.len()
            )
        },
        why: "Both CLIs understand AGENTS.md. Keeping build, test, architecture and coding conventions there prevents the two tools from receiving different project guidance.".into(),
        fix: "Move shared instructions into AGENTS.md. If Claude-specific guidance remains, keep a small CLAUDE.md that starts with @AGENTS.md and contains only the Claude-specific section; otherwise remove CLAUDE.md.".into(),
        snippet: Some("@AGENTS.md\n\n## Claude Code\n\n<!-- Claude-specific guidance only -->".into()),
        saving_usd: None,
        evidence: needs_work
            .iter()
            .take(12)
            .map(|root| Evidence {
                label: root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| root.display().to_string()),
                detail: Some(root.display().to_string()),
                link: None,
            })
            .collect(),
        metric: Metric::lower(missing as f64, "cross-tool projects needing consolidation"),
    })
}

fn copilot_project_instructions(conn: &Connection, since: &Since) -> Result<Opportunity> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT cwd FROM sessions
         WHERE harness = 'copilot-cli' AND cwd IS NOT NULL AND started_at >= ?1",
    )?;
    let mut roots = BTreeMap::<PathBuf, ()>::new();
    for row in stmt.query_map([&since.0], |r| r.get::<_, String>(0))? {
        let path = PathBuf::from(row?);
        if path.is_dir() {
            roots.insert(repo_root(&path), ());
        }
    }
    copilot_project_instructions_from_roots(roots)
}

fn copilot_plan_mode(conn: &Connection, since: &Since) -> Result<Opportunity> {
    let mut stmt = conn.prepare(
                "SELECT s.id, COALESCE(s.title, ''), s.project,
                        (SELECT COUNT(*) FROM prompts p WHERE p.session_id = s.id AND p.kind = 'prompt'),
                        (SELECT COUNT(*) FROM tool_calls t WHERE t.session_id = s.id),
                        (SELECT COUNT(*) FROM session_files f WHERE f.session_id = s.id),
                        EXISTS (SELECT 1 FROM session_modes m
                                WHERE m.session_id = s.id AND m.mode = 'plan')
                          OR EXISTS (SELECT 1 FROM prompts p
                                     WHERE p.session_id = s.id AND p.kind = 'command'
                                       AND p.text LIKE '/plan%')
                 FROM sessions s
                 WHERE s.harness = 'copilot-cli' AND s.started_at >= ?1",
            )?;
    let mut large = 0usize;
    let mut missed = Vec::new();
    for row in stmt.query_map([&since.0], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, bool>(6)?,
        ))
    })? {
        let (id, title, project, prompts, tools, files, planned) = row?;
        if prompts >= 8 || tools >= 15 || files >= 4 {
            large += 1;
            if !planned {
                missed.push((id, title, project, prompts, tools, files));
            }
        }
    }
    if large == 0 {
        return Ok(skip());
    }
    Ok(Opportunity {
                id: "copilot-plan-mode",
                area: "Workflow",
                status: if missed.is_empty() {
                    Status::Good
                } else {
                    Status::Consider
                },
                title: "Plan substantial Copilot tasks before editing".into(),
                finding: format!(
                    "{} of {large} substantial Copilot session{} did not use plan mode.",
                    missed.len(),
                    if large == 1 { "" } else { "s" }
                ),
                why: "Plan mode lets you inspect and adjust Copilot's approach before it changes files, which is most useful when a task spans many tools, files or turns.".into(),
                fix: "Start substantial tasks with /plan, review the proposed approach, then switch to implementation.".into(),
                snippet: Some("/plan".into()),
                saving_usd: None,
                evidence: missed
                    .iter()
                    .take(10)
                    .map(|(id, title, project, prompts, tools, files)| Evidence {
                        label: if title.is_empty() {
                            "(untitled session)".into()
                        } else {
                            title.clone()
                        },
                        detail: Some(format!(
                            "{} · {prompts} prompts · {tools} tool calls · {files} files",
                            project.clone().unwrap_or_default()
                        )),
                        link: Some(format!("/sessions#{id}")),
                    })
                    .collect(),
                metric: Metric::lower(missed.len() as f64, "substantial Copilot sessions without plan mode"),
            })
}

fn copilot_compact(conn: &Connection, since: &Since) -> Result<Opportunity> {
    let mut stmt = conn.prepare(
                "SELECT s.id, COALESCE(s.title, ''), s.project,
                        (SELECT COUNT(*) FROM prompts p WHERE p.session_id = s.id AND p.kind = 'prompt'),
                        (SELECT COALESCE(MAX(input_tokens), 0) FROM requests r WHERE r.session_id = s.id),
                        EXISTS (SELECT 1 FROM prompts p WHERE p.session_id = s.id
                                AND p.kind = 'command' AND p.text LIKE '/compact%')
                 FROM sessions s
                 WHERE s.harness = 'copilot-cli' AND s.started_at >= ?1",
            )?;
    let mut long = 0usize;
    let mut missed = Vec::new();
    for row in stmt.query_map([&since.0], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, bool>(5)?,
        ))
    })? {
        let (id, title, project, prompts, max_input, compacted) = row?;
        if prompts >= 20 || max_input >= 100_000 {
            long += 1;
            if !compacted {
                missed.push((id, title, project, prompts, max_input));
            }
        }
    }
    if long == 0 {
        return Ok(skip());
    }
    Ok(Opportunity {
                id: "copilot-compact",
                area: "Context",
                status: if missed.is_empty() {
                    Status::Good
                } else {
                    Status::Consider
                },
                title: "Compact long Copilot sessions before context gets crowded".into(),
                finding: format!(
                    "{} of {long} long or high-context Copilot session{} did not use /compact.",
                    missed.len(),
                    if long == 1 { "" } else { "s" }
                ),
                why: "/compact summarizes conversation history so useful context remains while old turn-by-turn detail stops consuming the context window.".into(),
                fix: "Run /context when a session grows; use /compact with optional focus instructions before continuing a long task.".into(),
                snippet: Some("/compact Keep the current implementation decisions and remaining test failures.".into()),
                saving_usd: None,
                evidence: missed
                    .iter()
                    .take(10)
                    .map(|(id, title, project, prompts, max_input)| Evidence {
                        label: if title.is_empty() {
                            "(untitled session)".into()
                        } else {
                            title.clone()
                        },
                        detail: Some(format!(
                            "{} · {prompts} prompts · peak recorded input {} tokens",
                            project.clone().unwrap_or_default(),
                            max_input
                        )),
                        link: Some(format!("/sessions#{id}")),
                    })
                    .collect(),
                metric: Metric::lower(missed.len() as f64, "long Copilot sessions without compact"),
            })
}

fn copilot_review(conn: &Connection, since: &Since) -> Result<Opportunity> {
    let mut stmt = conn.prepare(
        "SELECT s.id, COALESCE(s.title, ''), s.project, COUNT(DISTINCT f.file_path),
                        EXISTS (SELECT 1 FROM prompts p WHERE p.session_id = s.id
                                AND p.kind = 'command'
                                AND (p.text LIKE '/review%' OR p.text LIKE '/security-review%'
                                     OR p.text LIKE '/rubber-duck%'))
                 FROM sessions s JOIN session_files f ON f.session_id = s.id
                 WHERE s.harness = 'copilot-cli' AND s.started_at >= ?1
                 GROUP BY s.id, s.title, s.project",
    )?;
    let mut substantial = 0usize;
    let mut missed = Vec::new();
    for row in stmt.query_map([&since.0], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, bool>(4)?,
        ))
    })? {
        let (id, title, project, files, reviewed) = row?;
        if files >= 5 {
            substantial += 1;
            if !reviewed {
                missed.push((id, title, project, files));
            }
        }
    }
    if substantial == 0 {
        return Ok(skip());
    }
    Ok(Opportunity {
                id: "copilot-review",
                area: "Quality",
                status: if missed.is_empty() {
                    Status::Good
                } else {
                    Status::Consider
                },
                title: "Add an independent review after broad Copilot changes".into(),
                finding: format!(
                    "{} of {substantial} Copilot session{} changing 5 or more files did not run a review agent.",
                    missed.len(),
                    if substantial == 1 { "" } else { "s" }
                ),
                why: "A fresh review agent examines the resulting diff rather than continuing the implementation's assumptions, making it useful after broad changes.".into(),
                fix: "Run /review after substantial changes. Use /security-review for staged or unstaged security-sensitive changes, or /rubber-duck for an independent critique of the approach.".into(),
                snippet: Some("/review".into()),
                saving_usd: None,
                evidence: missed
                    .iter()
                    .take(10)
                    .map(|(id, title, project, files)| Evidence {
                        label: if title.is_empty() {
                            "(untitled session)".into()
                        } else {
                            title.clone()
                        },
                        detail: Some(format!(
                            "{} · {files} changed files",
                            project.clone().unwrap_or_default()
                        )),
                        link: Some(format!("/sessions#{id}")),
                    })
                    .collect(),
                metric: Metric::lower(missed.len() as f64, "broad Copilot sessions without review"),
            })
}

fn copilot_autopilot(conn: &Connection, since: &Since) -> Result<Opportunity> {
    let mut stmt = conn.prepare(
        "SELECT p.text, p.session_id FROM prompts p
                 JOIN sessions s ON s.id = p.session_id
                 WHERE s.harness = 'copilot-cli' AND p.kind = 'prompt'
                   AND p.is_sidechain = 0 AND p.ts >= ?1",
    )?;
    let mut repeated: BTreeMap<String, (String, i64, HashSet<String>)> = BTreeMap::new();
    for row in stmt.query_map([&since.0], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (text, session) = row?;
        let key = insights::normalise_prompt(&text);
        if key.split_whitespace().count() < 3 {
            continue;
        }
        let entry = repeated
            .entry(key)
            .or_insert_with(|| (text.clone(), 0, HashSet::new()));
        entry.1 += 1;
        entry.2.insert(session);
    }
    let mut rows: Vec<_> = repeated
        .into_values()
        .filter(|(_, count, sessions)| *count >= 4 && sessions.len() >= 2)
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if rows.is_empty() {
        return Ok(skip());
    }
    Ok(Opportunity {
                id: "copilot-autopilot",
                area: "Automation",
                status: Status::Consider,
                title: "Use bounded autopilot for repeated end-to-end tasks".into(),
                finding: format!(
                    "{} repeated Copilot task{} appeared 4 or more times across multiple sessions.",
                    rows.len(),
                    if rows.len() == 1 { "" } else { "s" }
                ),
                why: "Autopilot can plan and execute a well-understood objective with fewer handoffs. It is best for bounded, repeatable work rather than ambiguous changes.".into(),
                fix: "For a repeated task with clear success criteria, start /autopilot with an explicit objective and an AI-credit limit. Keep destructive operations and final review outside the objective.".into(),
                snippet: Some("/autopilot <objective> --max-ai-credits <limit>".into()),
                saving_usd: None,
                evidence: rows
                    .iter()
                    .take(10)
                    .map(|(example, count, sessions)| Evidence {
                        label: example.chars().take(140).collect(),
                        detail: Some(format!("{count}× across {} sessions", sessions.len())),
                        link: None,
                    })
                    .collect(),
                metric: Metric::lower(rows.len() as f64, "repeatable Copilot tasks not automated"),
            })
}

fn copilot_feature_catalog(
    conn: &Connection,
    since: &Since,
    copilot_dir: &Path,
) -> Result<Vec<Opportunity>> {
    let sessions: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sessions
         WHERE harness = 'copilot-cli' AND started_at >= ?1",
        [&since.0],
        |r| r.get(0),
    )?;
    if sessions == 0 {
        return Ok(Vec::new());
    }

    let mut commands = BTreeSet::new();
    let mut stmt = conn.prepare(
        "SELECT p.text FROM prompts p JOIN sessions s ON s.id = p.session_id
         WHERE s.harness = 'copilot-cli' AND p.kind = 'command' AND p.ts >= ?1",
    )?;
    for row in stmt.query_map([&since.0], |r| r.get::<_, String>(0))? {
        commands.insert(row?);
    }
    let used = |prefixes: &[&str]| {
        commands
            .iter()
            .any(|command| prefixes.iter().any(|prefix| command.starts_with(prefix)))
    };

    let config = read_json(&copilot_dir.join("config.json"));
    let settings = read_json(&copilot_dir.join("settings.json"));
    let setting = |key: &str| settings.get(key).or_else(|| config.get(key));
    let mut roots = BTreeSet::new();
    let mut stmt = conn.prepare(
        "SELECT DISTINCT cwd FROM sessions
         WHERE harness = 'copilot-cli' AND cwd IS NOT NULL AND started_at >= ?1",
    )?;
    for row in stmt.query_map([&since.0], |r| r.get::<_, String>(0))? {
        let path = PathBuf::from(row?);
        if path.is_dir() {
            roots.insert(repo_root(&path));
        }
    }

    let count_entries = |dir: &Path| {
        std::fs::read_dir(dir)
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or(0)
    };
    let project_hooks: usize = roots
        .iter()
        .map(|root| count_entries(&root.join(".github").join("hooks")))
        .sum();
    let inline_hooks = setting("hooks")
        .and_then(Value::as_object)
        .is_some_and(|hooks| !hooks.is_empty());
    let hooks_configured = inline_hooks || project_hooks > 0;

    let status_line = setting("statusLine").is_some();
    let history_size = setting("commandHistoryMaxSize")
        .and_then(Value::as_i64)
        .unwrap_or(50);
    let custom_agents = count_entries(&copilot_dir.join("agents"))
        + roots
            .iter()
            .map(|root| count_entries(&root.join(".github").join("agents")))
            .sum::<usize>();
    let subagents_used = custom_agents > 0 || used(&["/fleet", "/subagents", "/tasks", "/agent"]);
    let worktrees_used = used(&["/worktree", "/fork worktree", "/move"]);
    let notifications = setting("notifications").and_then(Value::as_bool) == Some(true);
    let keep_alive = setting("keepAlive")
        .and_then(Value::as_str)
        .is_some_and(|mode| mode != "off")
        || used(&["/keep-alive"]);
    let memory_enabled = setting("memory").and_then(Value::as_bool) != Some(false);
    let handoff_used = used(&["/remote", "/delegate", "/share"]);
    let context_controls_used = used(&["/context", "/limits", "/rewind"]);
    let prompt_tools_used = used(&["/refine", "/ask", "/research"]);
    let development_integrations_used = used(&["/ide", "/lsp"]);
    let session_navigation_used = used(&["/resume", "/session", "/search"]);

    let mcp_servers = read_json(&copilot_dir.join("mcp-config.json"))
        .get("mcpServers")
        .and_then(Value::as_object)
        .map_or(0, serde_json::Map::len);
    let plugins = count_entries(&copilot_dir.join("installed-plugins"));
    let skills = count_entries(&copilot_dir.join("skills"))
        + roots
            .iter()
            .map(|root| count_entries(&root.join(".github").join("skills")))
            .sum::<usize>();
    let extensions_configured = mcp_servers + plugins + skills + custom_agents > 0;

    Ok(vec![
        copilot_feature(
            "copilot-hooks",
            hooks_configured,
            "Automate checks and guardrails with Copilot hooks",
            if hooks_configured {
                format!(
                    "Hooks are configured{}.",
                    if project_hooks > 0 {
                        format!(" in {project_hooks} project hook file(s)")
                    } else {
                        String::new()
                    }
                )
            } else {
                "No user-level or repository Copilot hooks were discovered.".into()
            },
            "Hooks run deterministic commands around tool and session events, which is useful for formatting, validation, audit logging and policy checks.",
            "Add user-level hooks under the hooks setting, or commit repository hooks under .github/hooks. Use copilot help config for the current event schema.",
            Some("copilot help config"),
        ),
        copilot_feature(
            "copilot-status-line",
            status_line,
            "Add a Copilot status line",
            if status_line {
                "A custom status line is configured.".into()
            } else {
                "No statusLine configuration was found.".into()
            },
            "A status line can keep the model, context usage, repository state or current task visible without interrupting the conversation.",
            "Run /statusline to configure a command-backed status line.",
            Some("/statusline"),
        ),
        Opportunity {
            id: "copilot-command-history",
            area: "Copilot CLI",
            status: if history_size >= 200 {
                Status::Good
            } else {
                Status::Consider
            },
            title: "Keep more prompt history available".into(),
            finding: format!(
                "Copilot keeps up to {history_size} recent prompt{} for Ctrl+R and history navigation.",
                if history_size == 1 { "" } else { "s" }
            ),
            why: "A larger command history makes recurring prompts easier to recover without searching old sessions. This is separate from resumable session history, which Copilot stores automatically.".into(),
            fix: "Increase commandHistoryMaxSize if you often reuse older prompts; Copilot supports values from 1 to 1000.".into(),
            snippet: Some("/settings commandHistoryMaxSize 200".into()),
            saving_usd: None,
            evidence: Vec::new(),
            metric: Metric::higher(history_size as f64, "prompts retained in command history"),
        },
        copilot_feature(
            "copilot-subagents",
            subagents_used,
            "Use Copilot subagents and fleet mode",
            if subagents_used {
                format!(
                    "Subagent features or custom agents were found{}.",
                    if custom_agents > 0 {
                        format!(" ({custom_agents} custom agent(s))")
                    } else {
                        String::new()
                    }
                )
            } else {
                "No /fleet, /subagents, /tasks or custom-agent usage was found in this period."
                    .into()
            },
            "Subagents isolate investigation and parallelize independent work instead of growing one conversation with every intermediate detail.",
            "Use /subagents to choose subagent models and /fleet for a task with genuinely independent workstreams.",
            Some("/subagents"),
        ),
        copilot_feature(
            "copilot-worktrees",
            worktrees_used,
            "Isolate parallel work with Copilot worktrees",
            if worktrees_used {
                "Worktree commands were used in this period.".into()
            } else {
                "No /worktree, /fork worktree or /move usage was recorded.".into()
            },
            "A separate worktree keeps experimental or parallel changes away from the current dirty working tree while preserving a normal Git workflow.",
            "Use /fork worktree to preserve the current conversation, or /move to move uncommitted changes into a new worktree.",
            Some("/fork worktree"),
        ),
        copilot_feature(
            "copilot-notifications",
            notifications,
            "Enable attention notifications",
            if notifications {
                "Copilot desktop notifications are enabled.".into()
            } else {
                "The notifications setting is not enabled.".into()
            },
            "Notifications reduce polling when a long-running turn finishes or Copilot needs a decision.",
            "Enable notifications in Copilot settings. Supported terminals can also use terminalNotifications.",
            Some("/settings notifications on"),
        ),
        copilot_feature(
            "copilot-keep-alive",
            keep_alive,
            "Prevent sleep during long Copilot tasks",
            if keep_alive {
                "Keep-alive mode is configured or was used in this period.".into()
            } else {
                "Keep-alive is off and /keep-alive was not used.".into()
            },
            "Long builds, test suites and autopilot tasks can be interrupted when the machine sleeps.",
            "Use busy mode so sleep is prevented only while Copilot is actively working.",
            Some("/keep-alive busy"),
        ),
        copilot_feature(
            "copilot-memory",
            memory_enabled,
            "Keep Copilot cross-session memory enabled",
            if memory_enabled {
                "Agentic memory is enabled (the default).".into()
            } else {
                "The memory setting explicitly disables cross-session recall.".into()
            },
            "Memory lets Copilot retain useful project facts across sessions so recurring conventions need less explanation.",
            "Enable memory unless project policy requires it to remain off.",
            Some("/memory on"),
        ),
        copilot_feature(
            "copilot-extensions",
            extensions_configured,
            "Extend Copilot with MCP, plugins, agents and skills",
            if extensions_configured {
                format!(
                    "{mcp_servers} MCP server(s), {plugins} plugin(s), {custom_agents} custom agent(s) and {skills} skill(s) were discovered."
                )
            } else {
                "No MCP servers, installed plugins, custom agents or skills were discovered."
                    .into()
            },
            "Extensions turn repeated organization and project workflows into reusable capabilities instead of prompt text.",
            "Inspect /mcp, /plugin, /agent and /skills, then add only integrations tied to work you repeat.",
            Some("/env"),
        ),
        copilot_feature(
            "copilot-handoff",
            handoff_used,
            "Use remote sessions and delegation when work can continue elsewhere",
            if handoff_used {
                "Remote, delegation or sharing commands were used in this period.".into()
            } else {
                "No /remote, /delegate or /share usage was recorded.".into()
            },
            "Remote control keeps a live session accessible from GitHub web or mobile; delegation can hand a suitable task to GitHub for a pull request.",
            "Use /remote for an active session, /share for a report, or /delegate when the task is well-scoped and PR-ready.",
            Some("/remote"),
        ),
        copilot_feature(
            "copilot-context-controls",
            context_controls_used,
            "Use context, limits and rewind controls",
            if context_controls_used {
                "Context, limit or rewind commands were used in this period.".into()
            } else {
                "No /context, /limits or /rewind usage was recorded.".into()
            },
            "These controls show context pressure, cap AI-credit use and undo a turn together with its file changes.",
            "Use /context during long sessions, /limits for bounded work and /rewind when a turn takes the implementation in the wrong direction.",
            Some("/context"),
        ),
        copilot_feature(
            "copilot-prompt-tools",
            prompt_tools_used,
            "Use side questions, prompt refinement and research",
            if prompt_tools_used {
                "Prompt-support or research commands were used in this period.".into()
            } else {
                "No /ask, /refine or /research usage was recorded.".into()
            },
            "These commands keep quick questions out of the main history, turn rough notes into a reviewable prompt and separate research from implementation.",
            "Try /ask for a side question, /refine before submitting an ambiguous task, or /research for a source-backed investigation.",
            Some("/refine"),
        ),
        copilot_feature(
            "copilot-development-integrations",
            development_integrations_used,
            "Connect Copilot to IDE and language-server context",
            if development_integrations_used {
                "IDE or language-server commands were used in this period.".into()
            } else {
                "No /ide or /lsp usage was recorded.".into()
            },
            "IDE selections, diagnostics and language-server information can give Copilot more precise code context than filesystem search alone.",
            "Use /ide to connect a workspace and /lsp to inspect or manage configured language servers.",
            Some("/ide"),
        ),
        copilot_feature(
            "copilot-session-navigation",
            session_navigation_used,
            "Return to useful Copilot sessions",
            if session_navigation_used {
                "Session resume, management or search commands were used in this period.".into()
            } else {
                "No /resume, /session or /search usage was recorded.".into()
            },
            "Copilot stores resumable sessions locally. Reusing the right session preserves task context without keeping every task in one oversized conversation.",
            "Use /session to browse, /resume to reopen a relevant session and /search to find earlier conversation content when available.",
            Some("/session"),
        ),
    ])
}

fn copilot_feature(
    id: &'static str,
    configured: bool,
    title: &str,
    finding: String,
    why: &str,
    fix: &str,
    snippet: Option<&str>,
) -> Opportunity {
    Opportunity {
        id,
        area: match id {
            "copilot-hooks" => "Automation",
            "copilot-status-line" | "copilot-notifications" | "copilot-keep-alive" => "Interface",
            "copilot-command-history"
            | "copilot-memory"
            | "copilot-context-controls"
            | "copilot-session-navigation" => "Context",
            "copilot-subagents" | "copilot-extensions" => "Agents",
            "copilot-worktrees" | "copilot-handoff" => "Workflow",
            "copilot-prompt-tools" | "copilot-development-integrations" => "Quality",
            _ => "Copilot CLI",
        },
        status: if configured {
            Status::Good
        } else {
            Status::Consider
        },
        title: title.into(),
        finding,
        why: why.into(),
        fix: fix.into(),
        snippet: snippet.map(str::to_string),
        saving_usd: None,
        evidence: Vec::new(),
        metric: None,
    }
}

fn copilot_project_instructions_from_roots(roots: BTreeMap<PathBuf, ()>) -> Result<Opportunity> {
    if roots.is_empty() {
        return Ok(skip());
    }
    let has_instructions = |root: &Path| {
        [
            "AGENTS.md",
            "CLAUDE.md",
            "GEMINI.md",
            ".github/copilot-instructions.md",
        ]
        .iter()
        .any(|file| root.join(file).is_file())
    };
    let missing: Vec<&PathBuf> = roots
        .keys()
        .filter(|root| !has_instructions(root))
        .collect();
    Ok(Opportunity {
        id: "copilot-project-instructions",
        area: "Instructions",
        status: if missing.is_empty() {
            Status::Good
        } else {
            Status::Action
        },
        title: "Give Copilot CLI persistent project instructions".into(),
        finding: if missing.is_empty() {
            format!(
                "All {} projects used with Copilot CLI have an instruction file.",
                roots.len()
            )
        } else {
            format!(
                "{} of {} projects used with Copilot CLI have no recognized project instruction file.",
                missing.len(),
                roots.len()
            )
        },
        why: "Without project instructions, Copilot has to rediscover build commands, conventions and architecture in each session, increasing tool use and inconsistent changes.".into(),
        fix: "Run /init in Copilot CLI, then keep shared guidance in AGENTS.md. Use Copilot-specific instruction files only for guidance that should not apply to Claude Code.".into(),
        snippet: Some("/init".into()),
        saving_usd: None,
        evidence: missing
            .iter()
            .take(12)
            .map(|root| Evidence {
                label: root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| root.display().to_string()),
                detail: Some(root.display().to_string()),
                link: None,
            })
            .collect(),
        metric: Metric::lower(missing.len() as f64, "Copilot projects without instructions"),
    })
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
        metric: Metric::lower(missing.len() as f64, "projects without a CLAUDE.md"),
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
        ..skip()
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
         FROM requests WHERE is_sidechain = 1 AND ts >= ?1 AND model IS NOT NULL
         GROUP BY model ORDER BY model",
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
        metric: Metric::lower(premium_cost, "USD of subagent work on premium models"),
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
        metric: Metric::higher(defined as f64, "custom subagents"),
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
        metric: Metric::lower(share as f64, "% of requests at xhigh or max"),
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
        metric: Metric::lower(unused.len() as f64, "unused MCP servers"),
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
        metric: Metric::lower(never.len() as f64, "skills that never ran"),
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
        metric: Metric::lower(skipped.len() as f64, "skills Claude doesn't pick"),
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
        metric: Metric::lower(rows.len() as f64, "prompts typed 4+ times"),
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
    // Most frequent first; ties by name, so the order never depends on hash-map order.
    repeated.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if repeated.is_empty() {
        return Ok(skip());
    }
    Ok(Opportunity {
        id: "permission-rules",
        metric: Metric::lower(repeated.len() as f64, "repeated rejections"),
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
        metric: Metric::lower(local_only.len() as f64, "projects without shared settings"),
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
        ..skip()
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
    big.sort_by(|a, b| b.4.total_cmp(&a.4).then_with(|| a.0.cmp(&b.0)));
    let total = big.len() + planned_big;
    Ok(Opportunity {
        id: "plan-mode",
        metric: Metric::lower(big.len() as f64, "large sessions without a plan"),
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
            .total_cmp(&a.cost_usd)
            .then_with(|| a.id.cmp(&b.id))
    });
    let heavy_cost: f64 = heavy.iter().map(|s| s.cost_usd).sum();
    Ok(Opportunity {
        id: "long-sessions",
        metric: Metric::lower(heavy.len() as f64, "sessions over $75"),
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
        metric: Metric::higher(days.unwrap_or(30) as f64, "days of logs kept"),
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
        ..skip()
    }
}
