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

use crate::adapters::{claude_code, copilot_cli};
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

impl Stats {
    pub fn add(&mut self, other: Stats) {
        self.files_seen += other.files_seen;
        self.files_read += other.files_read;
        self.lines += other.lines;
        self.bad_lines += other.bad_lines;
    }
}

/// Import every supported local harness.
pub fn ingest_all(
    conn: &mut Connection,
    claude_dir: &Path,
    copilot_dir: &Path,
    prices: &PriceTable,
    progress: &mut dyn Progress,
) -> Result<Stats> {
    let mut stats = ingest_claude_code(conn, claude_dir, prices, progress)?;
    stats.add(ingest_copilot_cli(conn, copilot_dir, progress)?);
    Ok(stats)
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

/// Import Copilot CLI's normalized session store and append-only event logs.
pub fn ingest_copilot_cli(
    conn: &mut Connection,
    copilot_dir: &Path,
    progress: &mut dyn Progress,
) -> Result<Stats> {
    let mut stats = Stats::default();
    let store = copilot_dir.join("session-store.db");
    if store.is_file() {
        import_copilot_store(conn, &store)?;
    }

    let root = copilot_dir.join("session-state");
    if !root.is_dir() {
        return Ok(stats);
    }
    let files: Vec<_> = WalkDir::new(&root)
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && e.file_name() == "events.jsonl")
        .map(|e| e.into_path())
        .collect();
    let mut pending = Vec::with_capacity(files.len());
    {
        let mut known = conn.prepare("SELECT offset FROM files WHERE path = ?1")?;
        for path in files {
            let size = std::fs::metadata(&path).map_or(0, |m| m.len());
            let offset: u64 = known
                .query_row([path.to_string_lossy()], |r| r.get::<_, i64>(0))
                .optional()?
                .map_or(0, |o| o as u64);
            pending.push((path, size.saturating_sub(offset.min(size))));
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
            if ingest_copilot_event_file(&tx, &path, &mut stats)? {
                stats.files_read += 1;
            }
            tx.commit()?;
            progress.advance(todo);
        }
    }
    progress.finish();
    Ok(stats)
}

fn import_copilot_store(conn: &mut Connection, path: &Path) -> Result<()> {
    let source = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", path.display()))?;
    source.busy_timeout(std::time::Duration::from_secs(5))?;
    let tx = conn.transaction()?;

    {
        let mut rows = source.prepare(
            "SELECT id, cwd, repository, branch, summary, created_at, updated_at FROM sessions",
        )?;
        let mut q = rows.query([])?;
        let mut insert = tx.prepare_cached(
                        "INSERT INTO sessions (id, harness, project, cwd, git_branch, title, started_at, ended_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                         ON CONFLICT(id) DO UPDATE SET project=excluded.project, cwd=excluded.cwd,
                             git_branch=excluded.git_branch, title=COALESCE(excluded.title, sessions.title),
                             started_at=MIN(COALESCE(sessions.started_at, excluded.started_at), excluded.started_at),
                             ended_at=MAX(COALESCE(sessions.ended_at, excluded.ended_at), excluded.ended_at)",
                    )?;
        while let Some(r) = q.next()? {
            let native: String = r.get(0)?;
            let cwd: Option<String> = r.get(1)?;
            let repository: Option<String> = r.get(2)?;
            insert.execute(params![
                copilot_cli::namespaced(&native),
                copilot_cli::HARNESS,
                repository.or_else(|| cwd.as_deref().map(project_name)),
                cwd,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                normalize_ts(r.get::<_, String>(5)?),
                normalize_ts(r.get::<_, String>(6)?),
            ])?;
        }
    }

    {
        let mut rows =
            source.prepare("SELECT session_id, turn_index, user_message, timestamp FROM turns")?;
        let mut q = rows.query([])?;
        let mut insert = tx.prepare_cached(
            "INSERT OR IGNORE INTO prompts (id, session_id, ts, kind, text, is_sidechain)
                         VALUES (?1, ?2, ?3, ?4, ?5, 0)",
        )?;
        while let Some(r) = q.next()? {
            let session: String = r.get(0)?;
            let turn: i64 = r.get(1)?;
            let Some(text) = r.get::<_, Option<String>>(2)? else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            let kind = if text.trim_start().starts_with('/') {
                PromptKind::Command
            } else {
                PromptKind::Prompt
            };
            insert.execute(params![
                format!("{}:turn:{turn}", copilot_cli::namespaced(&session)),
                copilot_cli::namespaced(&session),
                normalize_ts(r.get::<_, String>(3)?),
                kind.as_str(),
                text,
            ])?;
        }
    }

    if source
        .prepare(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='assistant_usage_events'",
        )?
        .exists([])?
    {
        let mut rows = source.prepare(
            "SELECT id, session_id, turn_index, agent_id, parent_tool_call_id, model,
                                input_tokens, output_tokens,
                                cache_read_tokens, cache_write_tokens, total_nano_aiu, duration_ms,
                                reasoning_effort, created_at
                         FROM assistant_usage_events",
        )?;
        let mut q = rows.query([])?;
        let mut insert = tx.prepare_cached(
                        "INSERT OR REPLACE INTO requests
                         (request_id, session_id, ts, model, input_tokens, output_tokens, cache_read,
                          cache_write_5m, cache_write_1h, effort, turn_index, cost_usd, ai_units,
                          duration_ms, skill, is_sidechain)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?10, NULL, ?11, ?12, NULL, ?13)",
                    )?;
        while let Some(r) = q.next()? {
            let id: i64 = r.get(0)?;
            let session: String = r.get(1)?;
            let agent_id: Option<String> = r.get(3)?;
            let parent_tool_call_id: Option<String> = r.get(4)?;
            insert.execute(params![
                format!("{}:usage:{id}", copilot_cli::namespaced(&session)),
                copilot_cli::namespaced(&session),
                normalize_ts(r.get::<_, String>(13)?),
                r.get::<_, String>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, Option<i64>>(7)?,
                r.get::<_, Option<i64>>(8)?,
                r.get::<_, Option<i64>>(9)?,
                r.get::<_, Option<String>>(12)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(10)?
                    .map(|n| n as f64 / 1_000_000_000.0),
                r.get::<_, Option<i64>>(11)?,
                agent_id.is_some() || parent_tool_call_id.is_some(),
            ])?;
        }
    }

    copy_copilot_artifacts(&source, &tx)?;
    tx.commit()?;
    Ok(())
}

fn copy_copilot_artifacts(source: &Connection, tx: &Transaction) -> Result<()> {
    let mut files = source.prepare(
        "SELECT session_id, file_path, tool_name, turn_index, first_seen_at FROM session_files",
    )?;
    let mut q = files.query([])?;
    while let Some(r) = q.next()? {
        let session: String = r.get(0)?;
        tx.execute(
            "INSERT OR REPLACE INTO session_files
                         (session_id, file_path, tool_name, turn_index, first_seen_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                copilot_cli::namespaced(&session),
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<String>>(4)?.map(normalize_ts),
            ],
        )?;
    }
    let mut refs = source.prepare(
        "SELECT session_id, ref_type, ref_value, turn_index, created_at FROM session_refs",
    )?;
    let mut q = refs.query([])?;
    while let Some(r) = q.next()? {
        let session: String = r.get(0)?;
        tx.execute(
            "INSERT OR REPLACE INTO session_refs
                         (session_id, ref_type, ref_value, turn_index, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                copilot_cli::namespaced(&session),
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<String>>(4)?.map(normalize_ts),
            ],
        )?;
    }
    Ok(())
}

fn ingest_copilot_event_file(tx: &Transaction, path: &Path, stats: &mut Stats) -> Result<bool> {
    let native_session_id = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .context("Copilot event file has no session directory")?;
    ingest_jsonl_file(tx, path, stats, copilot_cli::HARNESS, |value| {
        copilot_cli::parse_record(value, native_session_id)
    })
}

fn normalize_ts(ts: String) -> String {
    if ts.contains('T') {
        ts
    } else {
        format!("{}Z", ts.replace(' ', "T"))
    }
}

/// Returns true if the file had anything new.
fn ingest_file(tx: &Transaction, path: &Path, stats: &mut Stats) -> Result<bool> {
    ingest_jsonl_file(tx, path, stats, claude_code::HARNESS, |value| {
        claude_code::parse_record(value)
    })
}

fn ingest_jsonl_file(
    tx: &Transaction,
    path: &Path,
    stats: &mut Stats,
    harness: &str,
    mut parse: impl FnMut(&mut Value) -> Vec<Event>,
) -> Result<bool> {
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
        if harness == claude_code::HARNESS {
            claude_code::strip_binary(&mut value);
        }
        serde_json::to_writer(&mut raw, &value)?;
        raw.push(b'\n');
        raw_lines += 1;
        for event in parse(&mut value) {
            write_event(tx, harness, &event)?;
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
    .execute(params![key, harness, size, offset, mtime])?;
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
                     cache_read, cache_write_5m, cache_write_1h, skill, is_sidechain, effort, turn_index)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT (request_id) DO UPDATE SET
                     input_tokens = MAX(requests.input_tokens, excluded.input_tokens),
                     output_tokens = MAX(requests.output_tokens, excluded.output_tokens),
                     cache_read = MAX(requests.cache_read, excluded.cache_read),
                     cache_write_5m = MAX(requests.cache_write_5m, excluded.cache_write_5m),
                     cache_write_1h = MAX(requests.cache_write_1h, excluded.cache_write_1h),
                     skill = COALESCE(requests.skill, excluded.skill),
                     effort = COALESCE(requests.effort, excluded.effort),
                     turn_index = COALESCE(requests.turn_index, excluded.turn_index)",
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
                r.effort,
                r.turn_index
            ])?;
        }
        Event::ToolCall(t) => {
            tx.prepare_cached(
                "INSERT INTO tool_calls
                 (id, session_id, request_id, ts, tool, skill, turn_index, input_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                     request_id = COALESCE(tool_calls.request_id, excluded.request_id),
                     turn_index = COALESCE(tool_calls.turn_index, excluded.turn_index),
                     input_json = COALESCE(tool_calls.input_json, excluded.input_json)",
            )?
            .execute(params![
                t.id,
                t.session_id,
                t.request_id,
                t.ts,
                t.tool,
                t.skill,
                t.turn_index,
                t.input_json
            ])?;
        }
        Event::ToolResult(r) => {
            tx.prepare_cached(
                "UPDATE tool_calls SET outcome = ?1, result_detail = ?3,
                     duration_ms = MAX(0, CAST((julianday(?4) - julianday(ts)) * 86400000 AS INTEGER))
                 WHERE id = ?2",
            )?
            .execute(params![
                r.outcome.as_str(),
                r.tool_use_id,
                r.detail,
                r.ts
            ])?;
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
        Event::ModeChanged {
            id,
            session_id,
            ts,
            mode,
        } => {
            tx.prepare_cached(
                "INSERT OR IGNORE INTO session_modes (id, session_id, ts, mode)
                 VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![id, session_id, ts, mode])?;
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
                     WHERE model = ?6 AND EXISTS (
                         SELECT 1 FROM sessions s
                         WHERE s.id = requests.session_id AND s.harness = 'claude-code'
                     )",
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
                    "UPDATE requests SET cost_usd = NULL WHERE model = ?1 AND EXISTS (
                         SELECT 1 FROM sessions s
                         WHERE s.id = requests.session_id AND s.harness = 'claude-code'
                     )",
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
