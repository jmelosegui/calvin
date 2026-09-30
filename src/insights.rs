//! The read side: every question calvin can answer, as plain functions over SQLite that
//! return serialisable structs. The terminal reports and the web API both call these, so
//! the CLI and the dashboard always agree.

use std::collections::HashMap;

use anyhow::{Result, bail};
use chrono::{Duration, SecondsFormat, Utc};
use rusqlite::{Connection, params};
use serde::Serialize;

/// A lower bound on timestamps, as an ISO-8601 UTC string (comparable to stored `ts`).
#[derive(Debug, Clone, PartialEq)]
pub struct Since(pub String);

impl Since {
    pub fn all() -> Self {
        Since(String::new())
    }

    /// Accepts `7d`, `12w`, `6m` (30-day months), `all`, or a date like `2026-09-01`.
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("all") {
            return Ok(Self::all());
        }
        if let Some((num, unit)) = s.split_at_checked(s.len().saturating_sub(1))
            && let Ok(n) = num.parse::<i64>()
        {
            let days = match unit {
                "d" => n,
                "w" => n * 7,
                "m" => n * 30,
                _ => bail!("unknown unit in '{s}' (use d, w or m)"),
            };
            let cutoff = Utc::now() - Duration::days(days);
            return Ok(Since(cutoff.to_rfc3339_opts(SecondsFormat::Millis, true)));
        }
        if chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() {
            return Ok(Since(format!("{s}T00:00:00.000Z")));
        }
        bail!("can't understand '{s}': use 7d, 4w, 3m, all, or YYYY-MM-DD")
    }
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub sessions: i64,
    pub prompts: i64,
    pub commands: i64,
    pub requests: i64,
    pub cost_usd: f64,
    pub ai_units: f64,
    pub unpriced_requests: i64,
    pub cache_hit_rate: Option<f64>,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
}

pub fn summary(conn: &Connection, since: &Since) -> Result<Summary> {
    let (prompts, commands): (i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(kind = 'prompt'), 0), COALESCE(SUM(kind = 'command'), 0)
         FROM prompts WHERE ts >= ?1 AND is_sidechain = 0",
        [&since.0],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let sessions: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sessions WHERE started_at >= ?1",
        [&since.0],
        |r| r.get(0),
    )?;
    let s = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(r.cost_usd), 0), COALESCE(SUM(r.ai_units), 0),
                SUM(r.cost_usd IS NULL AND s.harness = 'claude-code'),
                SUM(CASE WHEN s.harness = 'claude-code' THEN r.cache_read ELSE 0 END),
                SUM(CASE WHEN s.harness = 'claude-code'
                    THEN r.input_tokens + r.cache_read + r.cache_write_5m + r.cache_write_1h
                    ELSE 0 END),
                MIN(r.ts), MAX(r.ts)
         FROM requests r JOIN sessions s ON s.id = r.session_id WHERE r.ts >= ?1",
        [&since.0],
        |r| {
            let read: Option<i64> = r.get(4)?;
            let total: Option<i64> = r.get(5)?;
            Ok(Summary {
                sessions,
                prompts,
                commands,
                requests: r.get(0)?,
                cost_usd: r.get(1)?,
                ai_units: r.get(2)?,
                unpriced_requests: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                cache_hit_rate: ratio(read, total),
                first_ts: r.get(6)?,
                last_ts: r.get(7)?,
            })
        },
    )?;
    Ok(s)
}

#[derive(Debug, Serialize)]
pub struct HarnessComparison {
    pub harness: String,
    pub sessions: i64,
    pub prompts: i64,
    pub commands: i64,
    pub requests: i64,
    pub tool_calls: i64,
    pub tool_errors: i64,
    pub files_touched: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: Option<f64>,
    pub ai_units: Option<f64>,
}

/// Shared, rate-friendly measures for comparing coding harnesses without conflating their
/// provider-specific billing models.
pub fn harness_comparison(conn: &Connection, since: &Since) -> Result<Vec<HarnessComparison>> {
    let harnesses: Vec<String> = conn
        .prepare("SELECT DISTINCT harness FROM sessions WHERE started_at >= ?1 ORDER BY harness")?
        .query_map([&since.0], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::with_capacity(harnesses.len());
    for harness in harnesses {
        let sessions = conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE harness = ?2 AND started_at >= ?1",
            params![since.0, harness],
            |r| r.get(0),
        )?;
        let (prompts, commands) = conn.query_row(
            "SELECT COALESCE(SUM(p.kind = 'prompt'), 0), COALESCE(SUM(p.kind = 'command'), 0)
             FROM prompts p JOIN sessions s ON s.id = p.session_id
             WHERE p.ts >= ?1 AND p.is_sidechain = 0 AND s.harness = ?2",
            params![since.0, harness],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let (requests, input_tokens, output_tokens, cost_usd, ai_units) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(r.input_tokens), 0), COALESCE(SUM(r.output_tokens), 0),
                    SUM(r.cost_usd), SUM(r.ai_units)
             FROM requests r JOIN sessions s ON s.id = r.session_id
             WHERE r.ts >= ?1 AND s.harness = ?2",
            params![since.0, harness],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let (tool_calls, tool_errors) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(t.outcome = 'error'), 0)
             FROM tool_calls t JOIN sessions s ON s.id = t.session_id
             WHERE t.ts >= ?1 AND s.harness = ?2",
            params![since.0, harness],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let files_touched = conn.query_row(
            "SELECT COUNT(*) FROM session_files sf JOIN sessions s ON s.id = sf.session_id
             WHERE s.started_at >= ?1 AND s.harness = ?2",
            params![since.0, harness],
            |r| r.get(0),
        )?;
        out.push(HarnessComparison {
            harness,
            sessions,
            prompts,
            commands,
            requests,
            tool_calls,
            tool_errors,
            files_touched,
            input_tokens,
            output_tokens,
            cost_usd,
            ai_units,
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostBy {
    Day,
    Project,
    Model,
}

#[derive(Debug, Serialize)]
pub struct CostRow {
    pub key: String,
    pub sessions: i64,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost_usd: f64,
}

pub fn cost(conn: &Connection, since: &Since, by: CostBy) -> Result<Vec<CostRow>> {
    let (key, order) = match by {
        CostBy::Day => ("date(r.ts, 'localtime')", "key"),
        CostBy::Project => ("COALESCE(s.project, '(unknown)')", "cost DESC"),
        CostBy::Model => ("COALESCE(r.model, '(unknown)')", "cost DESC"),
    };
    let sql = format!(
        "SELECT {key} AS key, COUNT(DISTINCT r.session_id), COUNT(*),
                SUM(r.input_tokens), SUM(r.output_tokens), SUM(r.cache_read),
                SUM(r.cache_write_5m + r.cache_write_1h), COALESCE(SUM(r.cost_usd), 0) AS cost
         FROM requests r LEFT JOIN sessions s ON s.id = r.session_id
         WHERE r.ts >= ?1 GROUP BY key ORDER BY {order}"
    );
    let rows = conn
        .prepare(&sql)?
        .query_map([&since.0], |r| {
            Ok(CostRow {
                key: r.get(0)?,
                sessions: r.get(1)?,
                requests: r.get(2)?,
                input_tokens: r.get(3)?,
                output_tokens: r.get(4)?,
                cache_read: r.get(5)?,
                cache_write: r.get(6)?,
                cost_usd: r.get(7)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

#[derive(Debug, Serialize)]
pub struct SkillRow {
    pub skill: String,
    pub runs: i64,
    pub sessions: i64,
    pub last_used: String,
}

/// Skills the model invoked through the Skill tool.
pub fn skills(conn: &Connection, since: &Since) -> Result<Vec<SkillRow>> {
    let rows = conn
        .prepare(
            "SELECT skill, COUNT(*), COUNT(DISTINCT session_id), MAX(ts)
             FROM tool_calls WHERE tool = 'Skill' AND skill IS NOT NULL AND ts >= ?1
             GROUP BY skill ORDER BY 2 DESC, 1",
        )?
        .query_map([&since.0], |r| {
            Ok(SkillRow {
                skill: r.get(0)?,
                runs: r.get(1)?,
                sessions: r.get(2)?,
                last_used: r.get(3)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

#[derive(Debug, Serialize)]
pub struct CommandRow {
    pub command: String,
    pub uses: i64,
    pub last_used: String,
}

/// Slash commands the user typed (built-in commands and user-invoked skills alike).
pub fn commands(conn: &Connection, since: &Since) -> Result<Vec<CommandRow>> {
    let mut counts: HashMap<String, (i64, String)> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT text, ts FROM prompts WHERE kind = 'command' AND is_sidechain = 0 AND ts >= ?1",
    )?;
    let mut rows = stmt.query([&since.0])?;
    while let Some(r) = rows.next()? {
        let text: String = r.get(0)?;
        let ts: String = r.get(1)?;
        let name = text.split_whitespace().next().unwrap_or(&text).to_string();
        let e = counts.entry(name).or_insert((0, String::new()));
        e.0 += 1;
        if ts > e.1 {
            e.1 = ts;
        }
    }
    let mut out: Vec<_> = counts
        .into_iter()
        .map(|(command, (uses, last_used))| CommandRow {
            command,
            uses,
            last_used,
        })
        .collect();
    out.sort_by(|a, b| b.uses.cmp(&a.uses).then(a.command.cmp(&b.command)));
    Ok(out)
}

#[derive(Debug, Serialize)]
pub struct RepeatedPrompt {
    pub example: String,
    pub count: i64,
    pub sessions: i64,
    pub last_used: String,
}

/// Prompts typed at least `min_count` times, after normalising case, whitespace and
/// trailing punctuation.
pub fn repeated_prompts(
    conn: &Connection,
    since: &Since,
    min_count: i64,
) -> Result<Vec<RepeatedPrompt>> {
    struct Group {
        example: String,
        count: i64,
        sessions: std::collections::HashSet<String>,
        last: String,
    }
    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT text, session_id, ts FROM prompts WHERE kind = 'prompt' AND is_sidechain = 0 AND ts >= ?1",
    )?;
    let mut rows = stmt.query([&since.0])?;
    while let Some(r) = rows.next()? {
        let text: String = r.get(0)?;
        let key = normalise_prompt(&text);
        if key.len() < 4 {
            continue;
        }
        let g = groups.entry(key).or_insert_with(|| Group {
            example: text.trim().to_string(),
            count: 0,
            sessions: Default::default(),
            last: String::new(),
        });
        g.count += 1;
        g.sessions.insert(r.get(1)?);
        let ts: String = r.get(2)?;
        if ts > g.last {
            g.last = ts;
        }
    }
    let mut out: Vec<_> = groups
        .into_values()
        .filter(|g| g.count >= min_count)
        .map(|g| RepeatedPrompt {
            example: g.example,
            count: g.count,
            sessions: g.sessions.len() as i64,
            last_used: g.last,
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then(b.last_used.cmp(&a.last_used)));
    Ok(out)
}

pub fn normalise_prompt(text: &str) -> String {
    let lower = text.to_lowercase();
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_end_matches(|c: char| c.is_ascii_punctuation())
        .trim()
        .to_string()
}

#[derive(Debug, Serialize)]
pub struct FrictionRow {
    pub kind: String,
    pub tool: Option<String>,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct Friction {
    pub by_kind: Vec<FrictionRow>,
    pub by_tool: Vec<FrictionRow>,
    pub tool_errors: i64,
    pub tool_calls: i64,
}

pub fn friction(conn: &Connection, since: &Since) -> Result<Friction> {
    let query = |sql: &str| -> Result<Vec<FrictionRow>> {
        Ok(conn
            .prepare(sql)?
            .query_map([&since.0], |r| {
                Ok(FrictionRow {
                    kind: r.get(0)?,
                    tool: r.get(1)?,
                    count: r.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?)
    };
    let by_kind = query(
        "SELECT kind, NULL, COUNT(*) FROM friction WHERE ts >= ?1 GROUP BY kind ORDER BY 3 DESC",
    )?;
    let by_tool = query(
        "SELECT kind, tool, COUNT(*) FROM friction WHERE ts >= ?1 AND tool IS NOT NULL
         GROUP BY kind, tool ORDER BY 3 DESC LIMIT 15",
    )?;
    let (tool_errors, tool_calls) = conn.query_row(
        "SELECT COALESCE(SUM(outcome = 'error'), 0), COUNT(*) FROM tool_calls WHERE ts >= ?1",
        [&since.0],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(Friction {
        by_kind,
        by_tool,
        tool_errors,
        tool_calls,
    })
}

#[derive(Debug, Serialize)]
pub struct CacheRow {
    pub model: String,
    pub cache_read: i64,
    pub cache_write: i64,
    pub uncached_input: i64,
    pub hit_rate: Option<f64>,
}

/// Share of input tokens served from the prompt cache, per model.
pub fn cache(conn: &Connection, since: &Since) -> Result<Vec<CacheRow>> {
    let rows = conn
        .prepare(
            "SELECT model, SUM(cache_read), SUM(cache_write_5m + cache_write_1h), SUM(input_tokens)
             FROM requests WHERE ts >= ?1 AND model IS NOT NULL
             GROUP BY model ORDER BY SUM(cache_read + input_tokens) DESC",
        )?
        .query_map(params![since.0], |r| {
            let read: i64 = r.get(1)?;
            let write: i64 = r.get(2)?;
            let input: i64 = r.get(3)?;
            Ok(CacheRow {
                model: r.get(0)?,
                cache_read: read,
                cache_write: write,
                uncached_input: input,
                hit_rate: ratio(Some(read), Some(read + write + input)),
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

#[derive(Debug, Serialize)]
pub struct SkillUsage {
    pub name: String,
    pub installed: bool,
    /// Type of folder (`global`, `project`, `plugin`); `None` if no longer installed.
    pub kind: Option<String>,
    /// Where it came from, if recorded.
    pub detail: Option<String>,
    pub description: String,
    /// Times the model chose to run it (Skill tool).
    pub model_runs: i64,
    /// Times you ran it yourself as a /command.
    pub command_runs: i64,
    pub last_used: Option<String>,
}

/// Installed skills joined with how often each ran, including skills that never did.
/// Skills that ran but are no longer installed are included with `installed: false`.
pub fn skill_usage(
    conn: &Connection,
    since: &Since,
    installed: &[crate::skills::InstalledSkill],
) -> Result<Vec<SkillUsage>> {
    let mut by_name: HashMap<String, SkillUsage> = installed
        .iter()
        .map(|s| {
            let usage = SkillUsage {
                name: s.name.clone(),
                installed: true,
                kind: Some(s.kind.clone()),
                detail: s.detail.clone(),
                description: s.description.clone(),
                model_runs: 0,
                command_runs: 0,
                last_used: None,
            };
            (s.name.clone(), usage)
        })
        .collect();
    let newer = |a: &mut Option<String>, b: &str| {
        if a.as_deref().is_none_or(|a| a < b) {
            *a = Some(b.to_string());
        }
    };
    for r in skills(conn, since)? {
        let e = by_name
            .entry(r.skill.clone())
            .or_insert_with(|| SkillUsage {
                name: r.skill.clone(),
                installed: false,
                kind: None,
                detail: Some("ran in a session but is no longer installed".into()),
                description: String::new(),
                model_runs: 0,
                command_runs: 0,
                last_used: None,
            });
        e.model_runs += r.runs;
        newer(&mut e.last_used, &r.last_used);
    }
    // Only /commands that name a known skill; built-ins like /clear are not skills.
    for c in commands(conn, since)? {
        if let Some(e) = by_name.get_mut(c.command.trim_start_matches('/')) {
            e.command_runs += c.uses;
            newer(&mut e.last_used, &c.last_used);
        }
    }
    let mut out: Vec<_> = by_name.into_values().collect();
    out.sort_by(|a, b| {
        (b.model_runs + b.command_runs)
            .cmp(&(a.model_runs + a.command_runs))
            .then(a.name.cmp(&b.name))
    });
    Ok(out)
}

/// Working directories of every session seen, used to find project-level skills.
pub fn project_dirs(conn: &Connection) -> Result<Vec<std::path::PathBuf>> {
    let dirs = conn
        .prepare("SELECT DISTINCT cwd FROM sessions WHERE cwd IS NOT NULL")?
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .map(std::path::PathBuf::from)
        .collect();
    Ok(dirs)
}

#[derive(Debug, Serialize)]
pub struct Activity {
    pub ts: String,
    /// `prompt`, `command`, `tool`, `skill`, `request`, `rejected`, `denied`, `interrupted`.
    pub kind: String,
    pub project: Option<String>,
    pub label: String,
    /// Tokens for requests; not set for other kinds.
    pub tokens: Option<i64>,
}

/// The most recent events across all tables, newest first.
pub fn recent_activity(conn: &Connection, limit: i64) -> Result<Vec<Activity>> {
    let rows = conn
        .prepare(
            "SELECT * FROM (
                 SELECT p.ts, p.kind, s.project, substr(p.text, 1, 120), NULL
                 FROM prompts p LEFT JOIN sessions s ON s.id = p.session_id
                 WHERE p.is_sidechain = 0 ORDER BY p.ts DESC LIMIT ?1)
             UNION ALL SELECT * FROM (
                 SELECT t.ts, CASE WHEN t.tool = 'Skill' THEN 'skill' ELSE 'tool' END, s.project,
                        COALESCE(t.skill, t.tool), NULL
                 FROM tool_calls t LEFT JOIN sessions s ON s.id = t.session_id
                 ORDER BY t.ts DESC LIMIT ?1)
             UNION ALL SELECT * FROM (
                 SELECT r.ts, 'request', s.project, r.model, r.output_tokens
                 FROM requests r LEFT JOIN sessions s ON s.id = r.session_id
                 ORDER BY r.ts DESC LIMIT ?1)
             UNION ALL SELECT * FROM (
                 SELECT f.ts, f.kind, s.project, COALESCE(f.tool, ''), NULL
                 FROM friction f LEFT JOIN sessions s ON s.id = f.session_id
                 ORDER BY f.ts DESC LIMIT ?1)
             ORDER BY 1 DESC LIMIT ?1",
        )?
        .query_map([limit], |r| {
            Ok(Activity {
                ts: r.get(0)?,
                kind: r.get(1)?,
                project: r.get(2)?,
                label: r.get(3)?,
                tokens: r.get(4)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

#[derive(Debug, Serialize)]
pub struct ModelRow {
    pub model: String,
    pub requests: i64,
    /// Requests made by subagents rather than the main conversation.
    pub subagent_requests: i64,
    pub sessions: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub cost_usd: f64,
    pub cache_hit_rate: Option<f64>,
    /// Requests and cost per effort level, most used first. `effort` is `None` where
    /// the model or harness doesn't record one.
    pub efforts: Vec<EffortShare>,
}

#[derive(Debug, Serialize)]
pub struct EffortShare {
    pub effort: Option<String>,
    pub requests: i64,
    pub cost_usd: f64,
}

/// Usage per model, most expensive first.
pub fn models(conn: &Connection, since: &Since) -> Result<Vec<ModelRow>> {
    let rows = conn
        .prepare(
            "SELECT model, COUNT(*), COALESCE(SUM(is_sidechain), 0), COUNT(DISTINCT session_id),
                    COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_read), 0), COALESCE(SUM(cache_write_5m + cache_write_1h), 0),
                    COALESCE(SUM(cost_usd), 0)
             FROM requests WHERE ts >= ?1 AND model IS NOT NULL
             GROUP BY model ORDER BY 9 DESC, 2 DESC",
        )?
        .query_map([&since.0], |r| {
            let input: i64 = r.get(4)?;
            let read: i64 = r.get(6)?;
            let write: i64 = r.get(7)?;
            Ok(ModelRow {
                model: r.get(0)?,
                requests: r.get(1)?,
                subagent_requests: r.get(2)?,
                sessions: r.get(3)?,
                input_tokens: input,
                output_tokens: r.get(5)?,
                cache_read: read,
                cache_write: write,
                cost_usd: r.get(8)?,
                cache_hit_rate: ratio(Some(read), Some(read + write + input)),
                efforts: Vec::new(),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut rows = rows;
    let mut stmt = conn.prepare(
        "SELECT model, effort, COUNT(*), COALESCE(SUM(cost_usd), 0) FROM requests
         WHERE ts >= ?1 AND model IS NOT NULL GROUP BY model, effort ORDER BY 3 DESC",
    )?;
    let mut shares = stmt.query([&since.0])?;
    while let Some(r) = shares.next()? {
        let model: String = r.get(0)?;
        if let Some(row) = rows.iter_mut().find(|m| m.model == model) {
            row.efforts.push(EffortShare {
                effort: r.get(1)?,
                requests: r.get(2)?,
                cost_usd: r.get(3)?,
            });
        }
    }
    Ok(rows)
}

#[derive(Debug, Serialize)]
pub struct SessionRow {
    pub id: String,
    pub harness: String,
    pub title: Option<String>,
    pub first_prompt: Option<String>,
    pub project: Option<String>,
    pub git_branch: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub prompts: i64,
    pub requests: i64,
    pub cost_usd: f64,
    pub ai_units: f64,
    pub tool_calls: i64,
    /// Interruptions, rejected and denied tool calls.
    pub friction: i64,
}

/// Sessions that started in the period, newest first.
pub fn sessions(conn: &Connection, since: &Since, limit: i64) -> Result<Vec<SessionRow>> {
    let rows = conn
        .prepare(
            "SELECT s.id, s.harness, s.title,
                    (SELECT substr(text, 1, 200) FROM prompts p
                     WHERE p.session_id = s.id AND p.is_sidechain = 0 ORDER BY p.ts LIMIT 1),
                    s.project, s.git_branch, s.started_at, s.ended_at,
                    (SELECT COUNT(*) FROM prompts p WHERE p.session_id = s.id AND p.is_sidechain = 0),
                    (SELECT COUNT(*) FROM requests r WHERE r.session_id = s.id),
                    (SELECT COALESCE(SUM(cost_usd), 0) FROM requests r WHERE r.session_id = s.id),
                    (SELECT COALESCE(SUM(ai_units), 0) FROM requests r WHERE r.session_id = s.id),
                    (SELECT COUNT(*) FROM tool_calls t WHERE t.session_id = s.id),
                    (SELECT COUNT(*) FROM friction f WHERE f.session_id = s.id)
             FROM sessions s
             WHERE s.started_at IS NOT NULL AND s.started_at >= ?1
             ORDER BY s.started_at DESC LIMIT ?2",
        )?
        .query_map(params![since.0, limit], |r| {
            Ok(SessionRow {
                id: r.get(0)?,
                harness: r.get(1)?,
                title: r.get(2)?,
                first_prompt: r.get(3)?,
                project: r.get(4)?,
                git_branch: r.get(5)?,
                started_at: r.get(6)?,
                ended_at: r.get(7)?,
                prompts: r.get(8)?,
                requests: r.get(9)?,
                cost_usd: r.get(10)?,
                ai_units: r.get(11)?,
                tool_calls: r.get(12)?,
                friction: r.get(13)?,
            })
        })?
        .filter(|r| r.as_ref().map_or(true, |s| s.prompts > 0 || s.requests > 0))
        .collect::<Result<_, _>>()?;
    Ok(rows)
}

fn ratio(part: Option<i64>, total: Option<i64>) -> Option<f64> {
    match (part, total) {
        (Some(p), Some(t)) if t > 0 => Some(p as f64 / t as f64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_parses_relative_and_absolute() {
        assert_eq!(Since::parse("all").unwrap(), Since::all());
        assert_eq!(
            Since::parse("2026-09-01").unwrap().0,
            "2026-09-01T00:00:00.000Z"
        );
        assert!(Since::parse("7d").unwrap().0.ends_with('Z'));
        assert!(Since::parse("7x").is_err());
        assert!(Since::parse("soon").is_err());
    }

    #[test]
    fn prompt_normalisation_groups_near_duplicates() {
        assert_eq!(
            normalise_prompt("  Run the tests\n and FIX them. "),
            normalise_prompt("run the tests and fix them")
        );
    }
}
