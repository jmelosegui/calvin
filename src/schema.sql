-- calvin database schema, version 1.
-- Every harness adapter writes the same tables. Columns a harness can't fill stay NULL.

CREATE TABLE IF NOT EXISTS files (
    path        TEXT PRIMARY KEY,
    harness     TEXT NOT NULL,
    size        INTEGER NOT NULL,
    offset      INTEGER NOT NULL,
    mtime       INTEGER NOT NULL,
    ingested_at TEXT NOT NULL
);

-- The original log lines (images stripped), so data survives the harness deleting its logs
-- and can be re-parsed when a format changes. Each row is one batch of lines read from a
-- file, joined with newlines and zstd-compressed as a block (much smaller than per line).
CREATE TABLE IF NOT EXISTS raw_chunks (
    file         TEXT NOT NULL,
    start_offset INTEGER NOT NULL,
    end_offset   INTEGER NOT NULL,
    lines        INTEGER NOT NULL,
    zdata        BLOB NOT NULL,
    PRIMARY KEY (file, start_offset)
);

CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY,
    harness     TEXT NOT NULL,
    project     TEXT,
    cwd         TEXT,
    git_branch  TEXT,
    title       TEXT,
    cli_version TEXT,
    started_at  TEXT,
    ended_at    TEXT
);

-- What the user typed. kind: 'prompt' (free text) or 'command' (a /slash command).
CREATE TABLE IF NOT EXISTS prompts (
    id           TEXT PRIMARY KEY,
    session_id   TEXT NOT NULL,
    ts           TEXT NOT NULL,
    kind         TEXT NOT NULL,
    text         TEXT NOT NULL,
    is_sidechain INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS prompts_ts ON prompts (ts);

-- One row per model API request. Harnesses that split one response across several log
-- lines are deduplicated on request_id.
CREATE TABLE IF NOT EXISTS requests (
    request_id     TEXT PRIMARY KEY,
    session_id     TEXT NOT NULL,
    ts             TEXT NOT NULL,
    model          TEXT,
    input_tokens   INTEGER,
    output_tokens  INTEGER,
    cache_read     INTEGER,
    cache_write_5m INTEGER,
    cache_write_1h INTEGER,
    cost_usd       REAL,
    skill          TEXT,
    is_sidechain   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS requests_ts ON requests (ts);

-- outcome: NULL until the result is seen, then 'ok', 'error', 'rejected' or 'denied'.
CREATE TABLE IF NOT EXISTS tool_calls (
    id         TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    request_id TEXT,
    ts         TEXT NOT NULL,
    tool       TEXT NOT NULL,
    skill      TEXT,
    input_json TEXT,
    outcome    TEXT
);
CREATE INDEX IF NOT EXISTS tool_calls_ts ON tool_calls (ts);

-- Moments the user stopped or overruled the agent.
-- kind: 'rejected' (user said no to a tool), 'denied' (permission rule or classifier),
--       'interrupted' (user pressed Esc).
CREATE TABLE IF NOT EXISTS friction (
    id         TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    ts         TEXT NOT NULL,
    kind       TEXT NOT NULL,
    tool       TEXT,
    detail     TEXT
);
CREATE INDEX IF NOT EXISTS friction_ts ON friction (ts);

-- Per-session lookups for the session browser.
CREATE INDEX IF NOT EXISTS prompts_session ON prompts (session_id);
CREATE INDEX IF NOT EXISTS requests_session ON requests (session_id);
CREATE INDEX IF NOT EXISTS tool_calls_session ON tool_calls (session_id);
CREATE INDEX IF NOT EXISTS friction_session ON friction (session_id);
CREATE INDEX IF NOT EXISTS sessions_started ON sessions (started_at);
