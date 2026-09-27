//! A session's conversation, for the session browser: your prompts, the model's replies,
//! and every tool call, grouped into turns (one prompt and everything until the next).
//!
//! Built from the raw log lines calvin keeps, so replies are available even though the
//! parsed tables don't store them.

use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

use crate::adapters::claude_code;

/// Tool results and inputs are cut to this length for display.
pub const PREVIEW_CHARS: usize = 2_000;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    /// Something the model said.
    Text {
        ts: String,
        text: String,
        request_id: Option<String>,
    },
    /// A tool call, filled in with its result when that arrives.
    Tool {
        ts: String,
        id: String,
        name: String,
        /// Short description: the command, file path, pattern or skill.
        summary: String,
        input: String,
        /// `ok`, `error`, `rejected`, `denied`, or `None` if no result was logged.
        outcome: Option<String>,
        result: Option<String>,
        duration_ms: Option<i64>,
        request_id: Option<String>,
    },
    /// You pressed Esc.
    Interrupted { ts: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct Turn {
    /// What you typed. `None` for activity before the first prompt.
    pub prompt: Option<String>,
    /// `prompt` or `command`.
    pub kind: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub items: Vec<Item>,
    pub requests: i64,
    pub cost_usd: f64,
    pub output_tokens: i64,
    pub tool_calls: i64,
}

#[derive(Debug, Serialize)]
pub struct SessionView {
    pub id: String,
    pub title: Option<String>,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub turns: Vec<Turn>,
    pub cost_usd: f64,
    /// Cost of subagents this session started (their conversations aren't shown).
    pub subagent_cost_usd: f64,
}

/// Load a session's timeline from the stored raw lines.
pub fn session(conn: &Connection, id: &str) -> Result<Option<SessionView>> {
    let meta = conn
        .query_row(
            "SELECT title, project, cwd, git_branch, started_at, ended_at FROM sessions WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()?;
    let Some((title, project, cwd, git_branch, started_at, ended_at)) = meta else {
        return Ok(None);
    };

    // The main conversation is `<id>.jsonl`; subagents live in `<id>/subagents/`.
    let main_file: Option<String> = conn
        .query_row(
            "SELECT path FROM files WHERE path LIKE ?1 ORDER BY length(path) LIMIT 1",
            [format!("%{id}.jsonl")],
            |r| r.get(0),
        )
        .optional()?;
    let records = match &main_file {
        Some(file) => raw_records(conn, file)?,
        None => Vec::new(),
    };

    let costs = request_costs(conn, id)?;
    let turns = group_turns(claude_code::timeline(&records), &costs);
    let (cost_usd, subagent_cost_usd): (f64, f64) = conn.query_row(
        "SELECT COALESCE(SUM(cost_usd), 0), COALESCE(SUM(CASE WHEN is_sidechain = 1 THEN cost_usd END), 0)
         FROM requests WHERE session_id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(Some(SessionView {
        id: id.to_string(),
        title,
        project,
        cwd,
        git_branch,
        started_at,
        ended_at,
        turns,
        cost_usd,
        subagent_cost_usd,
    }))
}

/// Every stored line of one log file, in order.
fn raw_records(conn: &Connection, file: &str) -> Result<Vec<Value>> {
    let mut stmt =
        conn.prepare("SELECT zdata FROM raw_chunks WHERE file = ?1 ORDER BY start_offset")?;
    let chunks = stmt.query_map([file], |r| r.get::<_, Vec<u8>>(0))?;
    let mut out = Vec::new();
    for chunk in chunks {
        let raw = zstd::decode_all(&chunk?[..])?;
        for line in raw.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            if let Ok(v) = serde_json::from_slice::<Value>(line) {
                out.push(v);
            }
        }
    }
    Ok(out)
}

struct RequestCost {
    cost: f64,
    output: i64,
}

fn request_costs(conn: &Connection, session: &str) -> Result<HashMap<String, RequestCost>> {
    let mut stmt =
        conn.prepare("SELECT request_id, COALESCE(cost_usd, 0), COALESCE(output_tokens, 0) FROM requests WHERE session_id = ?1")?;
    let rows = stmt.query_map([session], |r| {
        Ok((
            r.get::<_, String>(0)?,
            RequestCost {
                cost: r.get(1)?,
                output: r.get(2)?,
            },
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A prompt event starts a new turn; everything else joins the current one.
pub enum Event {
    Prompt {
        ts: String,
        text: String,
        kind: &'static str,
    },
    Item(Item),
}

fn group_turns(events: Vec<Event>, costs: &HashMap<String, RequestCost>) -> Vec<Turn> {
    let new_turn = |prompt: Option<String>, kind: Option<String>, ts: Option<String>| Turn {
        prompt,
        kind,
        started_at: ts.clone(),
        ended_at: ts,
        items: Vec::new(),
        requests: 0,
        cost_usd: 0.0,
        output_tokens: 0,
        tool_calls: 0,
    };
    let mut turns: Vec<Turn> = Vec::new();
    let mut seen_requests = std::collections::HashSet::new();
    for event in events {
        match event {
            Event::Prompt { ts, text, kind } => {
                turns.push(new_turn(Some(text), Some(kind.to_string()), Some(ts)));
            }
            Event::Item(item) => {
                if turns.is_empty() {
                    turns.push(new_turn(None, None, None));
                }
                let turn = turns.last_mut().unwrap();
                let (ts, request_id) = match &item {
                    Item::Text { ts, request_id, .. } => (ts, request_id.as_ref()),
                    Item::Tool { ts, request_id, .. } => {
                        turn.tool_calls += 1;
                        (ts, request_id.as_ref())
                    }
                    Item::Interrupted { ts } => (ts, None),
                };
                if turn.started_at.is_none() {
                    turn.started_at = Some(ts.clone());
                }
                turn.ended_at = Some(ts.clone());
                if let Some(rid) = request_id
                    && seen_requests.insert(rid.clone())
                    && let Some(c) = costs.get(rid)
                {
                    turn.requests += 1;
                    turn.cost_usd += c.cost;
                    turn.output_tokens += c.output;
                }
                turn.items.push(item);
            }
        }
    }
    turns
}

pub fn preview(s: &str) -> String {
    match s.char_indices().nth(PREVIEW_CHARS) {
        Some((i, _)) => format!(
            "{}\n… ({} more characters)",
            &s[..i],
            s[i..].chars().count()
        ),
        None => s.to_string(),
    }
}
