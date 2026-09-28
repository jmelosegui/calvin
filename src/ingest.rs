//! Incremental import of harness log files into SQLite.
//!
//! Each file's read position is stored, so re-running only reads new lines. Only complete
//! lines (ending in `\n`) are consumed; a line still being written is picked up next time.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;
use walkdir::WalkDir;

use crate::adapters::claude_code;
use crate::event::*;
use crate::prices::PriceTable;

const RAW_COMPRESSION_LEVEL: i32 = 3;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stats {
    pub files_seen: usize,
    pub files_read: usize,
    pub lines: usize,
    pub bad_lines: usize,
}

/// Receives progress while importing. `start` is called once with the number of bytes
/// that need reading; `advance` after each file with the bytes it covered.
pub trait Progress {
    fn start(&mut self, _total_bytes: u64) {}
    fn advance(&mut self, _bytes: u64) {}
    fn finish(&mut self) {}
    /// Checked between files; returning true ends the import early. What was read so far
    /// is kept, and the next import continues from there.
    fn should_stop(&self) -> bool {
        false
    }
}

/// No progress output.
pub struct Quiet;
impl Progress for Quiet {}

/// Import new Claude Code log lines from `claude_dir/projects`, then refresh costs.
pub fn ingest_claude_code(
    conn: &mut Connection,
    claude_dir: &Path,
    prices: &PriceTable,
    progress: &mut dyn Progress,
) -> Result<Stats> {
    let mut stats = Stats::default();
    let root = claude_dir.join("projects");
    if root.is_dir() {
        let files: Vec<_> = WalkDir::new(&root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "jsonl")
            })
            .map(|e| e.into_path())
            .collect();

        // Work out how much is new, so progress reflects bytes to read, not file count.
        let mut pending = Vec::with_capacity(files.len());
        {
            let mut known = conn.prepare("SELECT offset FROM files WHERE path = ?1")?;
            for path in files {
                let size = std::fs::metadata(&path).map_or(0, |m| m.len());
                let offset: u64 = known
                    .query_row([path.to_string_lossy()], |r| r.get::<_, i64>(0))
                    .optional()?
                    .map_or(0, |o| o as u64);
                let todo = if offset > size { size } else { size - offset };
                pending.push((path, todo));
            }
        }
        progress.start(pending.iter().map(|(_, todo)| todo).sum());

        for (path, todo) in pending {
            if progress.should_stop() {
                break;
            }
            stats.files_seen += 1;
            if todo > 0 {
                let tx = conn.transaction()?;
                if ingest_file(&tx, &path, &mut stats)
                    .with_context(|| format!("ingesting {}", path.display()))?
                {
                    stats.files_read += 1;
                }
                tx.commit()?;
                progress.advance(todo);
            }
        }
    }
    reprice(conn, prices)?;
    progress.finish();
    Ok(stats)
}

/// Returns true if the file had anything new.
fn ingest_file(tx: &Transaction, path: &Path, stats: &mut Stats) -> Result<bool> {
    let key = path.to_string_lossy().to_string();
    let meta = std::fs::metadata(path)?;
    let size = meta.len() as i64;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);

    let known: Option<(i64, i64)> = tx
        .query_row(
            "SELECT offset, mtime FROM files WHERE path = ?1",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let mut offset = match known {
        Some((off, _)) if off == size => return Ok(false),
        // Shrunk: the file was rewritten. Start over; inserts are idempotent.
        Some((off, _)) if off > size => 0,
        Some((off, _)) => off,
        None => 0,
    };

    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset as u64))?;
    let mut buf = Vec::with_capacity((size - offset).max(0) as usize);
    file.read_to_end(&mut buf)?;
    let Some(last_newline) = buf.iter().rposition(|&b| b == b'\n') else {
        return Ok(false);
    };

    let mut raw = Vec::with_capacity(buf.len());
    let mut raw_lines = 0i64;
    for line in buf[..=last_newline].split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        stats.lines += 1;
        let Ok(mut value) = serde_json::from_slice::<Value>(line) else {
            stats.bad_lines += 1;
            continue;
        };
        claude_code::strip_binary(&mut value);
        serde_json::to_writer(&mut raw, &value)?;
        raw.push(b'\n');
        raw_lines += 1;
        for event in claude_code::parse_record(&value) {
            write_event(tx, claude_code::HARNESS, &event)?;
        }
    }
    let end_offset = offset + last_newline as i64 + 1;
    if raw_lines > 0 {
        let zdata = zstd::bulk::compress(&raw, RAW_COMPRESSION_LEVEL)?;
        tx.prepare_cached(
            "INSERT OR REPLACE INTO raw_chunks (file, start_offset, end_offset, lines, zdata)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?
        .execute(params![key, offset, end_offset, raw_lines, zdata])?;
    }
    offset = end_offset;

    tx.prepare_cached(
        "INSERT INTO files (path, harness, size, offset, mtime, ingested_at)
         VALUES (?1, ?2, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         ON CONFLICT (path) DO UPDATE SET size = excluded.size, offset = excluded.offset,
             mtime = excluded.mtime, ingested_at = excluded.ingested_at",
    )?
    .execute(params![key, claude_code::HARNESS, size, offset, mtime])?;
    Ok(true)
}

pub fn write_event(tx: &Transaction, harness: &str, event: &Event) -> Result<()> {
    match event {
        Event::Session(s) => {
            tx.prepare_cached(
                "INSERT INTO sessions (id, harness, project, cwd, git_branch, cli_version, started_at, ended_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                 ON CONFLICT (id) DO UPDATE SET
                     project = COALESCE(sessions.project, excluded.project),
                     cwd = COALESCE(sessions.cwd, excluded.cwd),
                     git_branch = COALESCE(excluded.git_branch, sessions.git_branch),
                     cli_version = COALESCE(excluded.cli_version, sessions.cli_version),
                     started_at = MIN(COALESCE(sessions.started_at, excluded.started_at), excluded.started_at),
                     ended_at = MAX(COALESCE(sessions.ended_at, excluded.ended_at), excluded.ended_at)",
            )?
            .execute(params![
                s.id,
                harness,
                s.cwd.as_deref().map(project_name),
                s.cwd,
                s.git_branch,
                s.cli_version,
                s.ts
            ])?;
        }
        Event::Title { session_id, title } => {
            tx.prepare_cached(
                "INSERT INTO sessions (id, harness, title) VALUES (?1, ?2, ?3)
                 ON CONFLICT (id) DO UPDATE SET title = excluded.title",
            )?
            .execute(params![session_id, harness, title])?;
        }
        Event::Prompt(p) => {
            tx.prepare_cached(
                "INSERT OR IGNORE INTO prompts (id, session_id, ts, kind, text, is_sidechain)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                p.id,
                p.session_id,
                p.ts,
                p.kind.as_str(),
                p.text,
                p.is_sidechain
            ])?;
        }
        Event::Request(r) => {
            let u = r.usage.clone().unwrap_or_default();
            // The same response is logged once per content block with the same usage;
            // MAX keeps the most complete numbers if they ever differ.
            tx.prepare_cached(
                "INSERT INTO requests (request_id, session_id, ts, model, input_tokens, output_tokens,
                     cache_read, cache_write_5m, cache_write_1h, skill, is_sidechain, effort)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT (request_id) DO UPDATE SET
                     input_tokens = MAX(requests.input_tokens, excluded.input_tokens),
                     output_tokens = MAX(requests.output_tokens, excluded.output_tokens),
                     cache_read = MAX(requests.cache_read, excluded.cache_read),
                     cache_write_5m = MAX(requests.cache_write_5m, excluded.cache_write_5m),
                     cache_write_1h = MAX(requests.cache_write_1h, excluded.cache_write_1h),
                     skill = COALESCE(requests.skill, excluded.skill),
                     effort = COALESCE(requests.effort, excluded.effort)",
            )?
            .execute(params![
                r.request_id,
                r.session_id,
                r.ts,
                r.model,
                u.input,
                u.output,
                u.cache_read,
                u.cache_write_5m,
                u.cache_write_1h,
                r.skill,
                r.is_sidechain,
                r.effort
            ])?;
        }
        Event::ToolCall(t) => {
            tx.prepare_cached(
                "INSERT OR IGNORE INTO tool_calls (id, session_id, request_id, ts, tool, skill, input_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![t.id, t.session_id, t.request_id, t.ts, t.tool, t.skill, t.input_json])?;
        }
        Event::ToolResult(r) => {
            tx.prepare_cached("UPDATE tool_calls SET outcome = ?1 WHERE id = ?2")?
                .execute(params![r.outcome.as_str(), r.tool_use_id])?;
            if matches!(r.outcome, Outcome::Rejected | Outcome::Denied) {
                tx.prepare_cached(
                    "INSERT OR IGNORE INTO friction (id, session_id, ts, kind, tool, detail)
                     VALUES (?1, ?2, ?3, ?4, (SELECT tool FROM tool_calls WHERE id = ?1), ?5)",
                )?
                .execute(params![
                    r.tool_use_id,
                    r.session_id,
                    r.ts,
                    r.outcome.as_str(),
                    r.detail
                ])?;
            }
        }
        Event::Interrupted { id, session_id, ts } => {
            tx.prepare_cached(
                "INSERT OR IGNORE INTO friction (id, session_id, ts, kind) VALUES (?1, ?2, ?3, 'interrupted')",
            )?
            .execute(params![id, session_id, ts])?;
        }
    }
    Ok(())
}

/// Recompute `cost_usd` for every request from the current price table.
pub fn reprice(conn: &Connection, prices: &PriceTable) -> Result<()> {
    let models: Vec<String> = conn
        .prepare("SELECT DISTINCT model FROM requests WHERE model IS NOT NULL")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let m = &prices.multipliers;
    for model in models {
        match prices.lookup(&model) {
            Some(p) => {
                conn.execute(
                    "UPDATE requests SET cost_usd = (input_tokens * ?1 + output_tokens * ?2 + cache_read * ?3
                         + cache_write_5m * ?4 + cache_write_1h * ?5) / 1000000.0
                     WHERE model = ?6",
                    params![
                        p.input,
                        p.output,
                        p.cache_read.unwrap_or(p.input * m.cache_read),
                        p.cache_write_5m.unwrap_or(p.input * m.cache_write_5m),
                        p.cache_write_1h.unwrap_or(p.input * m.cache_write_1h),
                        model
                    ],
                )?;
            }
            None => {
                conn.execute(
                    "UPDATE requests SET cost_usd = NULL WHERE model = ?1",
                    [&model],
                )?;
            }
        }
    }
    Ok(())
}

/// Last path component of a working directory, for Windows or Unix paths alike.
pub fn project_name(cwd: &str) -> String {
    cwd.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(cwd)
        .to_string()
}
