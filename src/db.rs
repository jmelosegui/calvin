use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, params};

const SCHEMA: &str = include_str!("schema.sql");

/// Schema history:
/// - 1: first release layout. Early builds kept raw log lines one per row in `raw_lines`.
/// - 2: raw lines live in zstd-compressed blocks in `raw_chunks`; `raw_lines` is migrated
///   into it and dropped.
/// - 3: background-task notifications are no longer prompts; remove the ones imported as such.
/// - 4: `requests.effort`, backfilled from the raw lines.
pub const SCHEMA_VERSION: i64 = 4;

/// Lines per compressed block when migrating old rows.
const MIGRATION_CHUNK_BYTES: usize = 4 * 1024 * 1024;

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    init(&conn)?;
    Ok(conn)
}

/// A read-only connection for queries. Expects the database to exist already.
pub fn open_read(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    init(&conn)?;
    Ok(conn)
}

fn init(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        bail!(
            "database schema v{version} is newer than this calvin (v{SCHEMA_VERSION}); upgrade calvin"
        );
    }
    // Columns added after a table was first created; CREATE TABLE IF NOT EXISTS won't add them.
    if table_exists(conn, "requests")? && !column_exists(conn, "requests", "effort")? {
        conn.execute_batch("ALTER TABLE requests ADD COLUMN effort TEXT")?;
    }
    conn.execute_batch(SCHEMA)?;
    if table_exists(conn, "raw_lines")? {
        migrate_raw_lines(conn).context("migrating raw log lines to the compressed layout")?;
    }
    if version < 3 {
        conn.execute(
            "DELETE FROM prompts WHERE text LIKE '<task-notification>%'",
            [],
        )?;
    }
    if version < 4 && table_exists(conn, "raw_chunks")? {
        backfill_effort(conn).context("backfilling request effort from raw lines")?;
    }
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(conn
        .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .any(|c| c == column))
}

/// Read the effort each request ran at from the stored raw lines, for requests imported
/// before calvin recorded it.
fn backfill_effort(conn: &Connection) -> Result<()> {
    let missing: i64 = conn.query_row(
        "SELECT COUNT(*) FROM requests WHERE effort IS NULL",
        [],
        |r| r.get(0),
    )?;
    if missing == 0 {
        return Ok(());
    }
    eprintln!("calvin: reading the effort level of {missing} earlier requests (one time)…");
    conn.execute_batch("BEGIN")?;
    let result = (|| -> Result<()> {
        let mut update = conn
            .prepare("UPDATE requests SET effort = ?1 WHERE request_id = ?2 AND effort IS NULL")?;
        let mut chunks = conn.prepare("SELECT zdata FROM raw_chunks")?;
        let mut rows = chunks.query([])?;
        while let Some(r) = rows.next()? {
            let raw = zstd::decode_all(&r.get::<_, Vec<u8>>(0)?[..])?;
            for line in raw.split(|&b| b == b'\n') {
                // Cheap filter before parsing: only assistant lines carry effort.
                if !line.windows(9).any(|w| w == b"\"effort\":") {
                    continue;
                }
                let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else {
                    continue;
                };
                let (Some(effort), Some(id)) = (
                    v.get("effort").and_then(|e| e.as_str()),
                    v.get("requestId").and_then(|e| e.as_str()),
                ) else {
                    continue;
                };
                update.execute(params![effort, id])?;
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        [name],
        |r| r.get(0),
    )?)
}

/// Move rows from the old one-line-per-row `raw_lines` table into compressed `raw_chunks`
/// blocks, then drop it and compact the file. Handles both old column layouts (`json`
/// text, and `zjson` compressed per line). Lines already stored as chunks are untouched:
/// old rows always come before them in the file, so block order is preserved.
fn migrate_raw_lines(conn: &Connection) -> Result<()> {
    let compressed = conn
        .prepare("SELECT name FROM pragma_table_info('raw_lines')")?
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .any(|c| c == "zjson");
    let column = if compressed { "zjson" } else { "json" };
    eprintln!(
        "calvin: upgrading the database to the compressed layout (one time, may take a minute)…"
    );

    conn.execute_batch("BEGIN")?;
    let result = (|| -> Result<()> {
        let files: Vec<String> = conn
            .prepare("SELECT DISTINCT file FROM raw_lines")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let mut insert = conn.prepare(
            "INSERT OR IGNORE INTO raw_chunks (file, start_offset, end_offset, lines, zdata)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut rows = conn.prepare(&format!(
            "SELECT offset, {column} FROM raw_lines WHERE file = ?1 ORDER BY offset"
        ))?;
        for file in files {
            let mut buf: Vec<u8> = Vec::new();
            let (mut start, mut end, mut lines) = (None::<i64>, 0i64, 0i64);
            let mut flush = |buf: &mut Vec<u8>,
                             start: &mut Option<i64>,
                             end: i64,
                             lines: &mut i64|
             -> Result<()> {
                if let Some(s) = start.take() {
                    let zdata = zstd::bulk::compress(buf, 3)?;
                    insert.execute(params![file, s, end, *lines, zdata])?;
                }
                buf.clear();
                *lines = 0;
                Ok(())
            };
            let mut q = rows.query([&file])?;
            while let Some(r) = q.next()? {
                let offset: i64 = r.get(0)?;
                let line: Vec<u8> = if compressed {
                    zstd::decode_all(&r.get::<_, Vec<u8>>(1)?[..])?
                } else {
                    r.get::<_, String>(1)?.into_bytes()
                };
                start.get_or_insert(offset);
                end = offset + line.len() as i64 + 1;
                buf.extend_from_slice(&line);
                buf.push(b'\n');
                lines += 1;
                if buf.len() >= MIGRATION_CHUNK_BYTES {
                    flush(&mut buf, &mut start, end, &mut lines)?;
                }
            }
            flush(&mut buf, &mut start, end, &mut lines)?;
        }
        conn.execute_batch("DROP TABLE raw_lines")?;
        Ok(())
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    // Give the space back to the disk; the old table was several times larger.
    conn.execute_batch("VACUUM")?;
    eprintln!("calvin: database upgrade done.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_old_raw_lines_into_chunks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("old.db");
        {
            // A database as the first builds left it.
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE raw_lines (file TEXT NOT NULL, offset INTEGER NOT NULL, type TEXT,
                     json TEXT NOT NULL, PRIMARY KEY (file, offset));
                 INSERT INTO raw_lines VALUES ('a.jsonl', 0, 'user', '{\"n\":1}');
                 INSERT INTO raw_lines VALUES ('a.jsonl', 8, 'user', '{\"n\":2}');
                 INSERT INTO raw_lines VALUES ('b.jsonl', 0, 'user', '{\"n\":3}');
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }

        let conn = open(&path).unwrap();
        assert!(!table_exists(&conn, "raw_lines").unwrap());
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        let (lines, zdata): (i64, Vec<u8>) = conn
            .query_row(
                "SELECT lines, zdata FROM raw_chunks WHERE file = 'a.jsonl'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(lines, 2);
        assert_eq!(
            zstd::decode_all(&zdata[..]).unwrap(),
            b"{\"n\":1}\n{\"n\":2}\n"
        );
        let chunks: i64 = conn
            .query_row("SELECT COUNT(*) FROM raw_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(chunks, 2);

        // Opening again is a no-op.
        drop(conn);
        let conn = open(&path).unwrap();
        let chunks: i64 = conn
            .query_row("SELECT COUNT(*) FROM raw_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(chunks, 2);
    }
}
