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
    let s = conn.query_row(
        "SELECT COUNT(DISTINCT session_id), COUNT(*), COALESCE(SUM(cost_usd), 0),
                SUM(cost_usd IS NULL),
                SUM(cache_read), SUM(input_tokens + cache_read + cache_write_5m + cache_write_1h),
                MIN(ts), MAX(ts)
         FROM requests WHERE ts >= ?1",
        [&since.0],
        |r| {
            let read: Option<i64> = r.get(4)?;
            let total: Option<i64> = r.get(5)?;
            Ok(Summary {
                sessions: r.get(0)?,
                prompts,
                commands,
                requests: r.get(1)?,
                cost_usd: r.get(2)?,
                unpriced_requests: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                cache_hit_rate: ratio(read, total),
                first_ts: r.get(6)?,
                last_ts: r.get(7)?,
            })
        },
    )?;
    Ok(s)
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
