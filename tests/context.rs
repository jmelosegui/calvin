//! Context overhead, failure loops and repository facts: ingest and recommendations.

use std::fs;
use std::path::{Path, PathBuf};

use calvin::db;
use calvin::ingest::{Quiet, ingest_claude_code, ingest_copilot_cli};
use calvin::insights::Since;
use calvin::opportunities::{self, Context, Opportunity, Status};
use calvin::prices::PriceTable;
use rusqlite::Connection;
use serde_json::{Value, json};

const CLAUDE_SESSION: &str = "00000000-0000-4000-8000-0000000000c1";
const COPILOT_SESSION: &str = "22222222-2222-4222-8222-222222222222";

/// Writes one Claude Code session log and returns the `.claude` folder.
fn claude_log(root: &Path, session: &str, lines: &[Value]) -> PathBuf {
    let claude = root.join("claude");
    let dir = claude.join("projects/C--work-demo");
    fs::create_dir_all(&dir).unwrap();
    write_lines(&dir.join(format!("{session}.jsonl")), lines);
    claude
}

/// Writes one Copilot CLI session log and returns the `.copilot` folder.
fn copilot_log(root: &Path, session: &str, lines: &[Value]) -> PathBuf {
    let copilot = root.join("copilot");
    let dir = copilot.join("session-state").join(session);
    fs::create_dir_all(&dir).unwrap();
    write_lines(&dir.join("events.jsonl"), lines);
    copilot
}

fn write_lines(path: &Path, lines: &[Value]) {
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    fs::write(path, text).unwrap();
}

fn claude(session: &str, ts: &str, kind: &str, extra: Value) -> Value {
    let mut line = json!({
        "sessionId": session,
        "uuid": format!("{session}-{ts}-{kind}"),
        "timestamp": ts,
        "cwd": "C:\\work\\demo",
        "gitBranch": "main",
        "version": "2.1.0",
        "isSidechain": false,
        "type": kind,
    });
    for (k, v) in extra.as_object().unwrap() {
        line[k] = v.clone();
    }
    line
}

fn claude_prompt(session: &str, ts: &str, text: &str) -> Value {
    claude(
        session,
        ts,
        "user",
        json!({"message": {"role": "user", "content": text}}),
    )
}

fn claude_tool(session: &str, ts: &str, id: &str, name: &str, input: Value) -> Value {
    claude(
        session,
        ts,
        "assistant",
        json!({
            "requestId": format!("req-{id}"),
            "message": {
                "model": "claude-opus-5",
                "role": "assistant",
                "content": [{"type": "tool_use", "id": id, "name": name, "input": input}],
                "usage": {"input_tokens": 10, "output_tokens": 50,
                          "cache_read_input_tokens": 20000, "cache_creation_input_tokens": 0}
            }
        }),
    )
}

fn claude_result(session: &str, ts: &str, id: &str, is_error: bool, text: &str) -> Value {
    claude(
        session,
        ts,
        "user",
        json!({"message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "is_error": is_error, "content": text}
        ]}}),
    )
}

fn claude_attachment(session: &str, ts: &str, attachment: Value) -> Value {
    claude(session, ts, "attachment", json!({"attachment": attachment}))
}

fn copilot(ts: &str, id: &str, kind: &str, data: Value) -> Value {
    json!({"id": id, "timestamp": ts, "type": kind, "data": data})
}

fn copilot_start(session: &str) -> Value {
    copilot(
        "2026-09-20T10:00:00.000Z",
        "start",
        "session.start",
        json!({"sessionId": session, "startTime": "2026-09-20T10:00:00.000Z",
               "copilotVersion": "1.0.0", "context": {"cwd": "C:\\work\\demo"}}),
    )
}

fn copilot_usage(ts: &str, id: &str, tools: i64, tool_tokens: i64, input: i64) -> Value {
    copilot(
        ts,
        id,
        "session.usage_record",
        json!({"usage": {"apiCallId": format!("call-{id}"), "availableToolCount": tools,
                         "toolTokenCount": tool_tokens, "inputTokens": input,
                         "initiator": "user"}}),
    )
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn find<'a>(items: &'a [Opportunity], id: &str) -> Option<&'a Opportunity> {
    items.iter().find(|o| o.id == id)
}

fn run_opportunities(
    conn: &Connection,
    root: &Path,
    claude: &Path,
    copilot: &Path,
) -> Vec<Opportunity> {
    let prices = PriceTable::bundled();
    let cursor = root.join("cursor");
    let state = root.join("state.vscdb");
    let context = Context {
        claude_dir: claude,
        copilot_dir: copilot,
        cursor_dir: &cursor,
        cursor_state_db: &state,
        prices: &prices,
        skills: &[],
    };
    opportunities::run(conn, &Since::all(), &context).unwrap()
}

fn context_session(session: &str) -> Vec<Value> {
    context_session_with(session, 4_000, "mcp__browser__click")
}

fn context_session_with(session: &str, skill_chars: usize, tool: &str) -> Vec<Value> {
    vec![
        claude_prompt(session, "2026-09-01T10:00:00.000Z", "look at the page"),
        claude_attachment(
            session,
            "2026-09-01T10:00:00.100Z",
            json!({"type": "deferred_tools_delta",
                   "addedNames": ["NotebookEdit", "mcp__browser__click"],
                   "removedNames": [], "failedMcpServers": ["broken"]}),
        ),
        claude_attachment(
            session,
            "2026-09-01T10:00:00.200Z",
            json!({"type": "skill_listing", "content": "x".repeat(skill_chars),
                   "skillCount": 3, "isInitial": true}),
        ),
        claude_attachment(
            session,
            "2026-09-01T10:00:00.300Z",
            json!({"type": "instructions", "files": [
                {"path": "C:\\work\\demo\\CLAUDE.md", "type": "Project", "content": "y".repeat(2_000)}
            ]}),
        ),
        claude_tool(
            session,
            "2026-09-01T10:00:01.000Z",
            &format!("t-{session}"),
            tool,
            json!({"uid": "1"}),
        ),
        claude_result(
            session,
            "2026-09-01T10:00:02.000Z",
            &format!("t-{session}"),
            true,
            "login failed with token ghp_abcdefghijklmnop0123 rejected",
        ),
    ]
}

fn session_id(n: usize) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

#[test]
fn claude_context_and_failures_are_recorded_without_content() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = claude_log(tmp.path(), CLAUDE_SESSION, &context_session(CLAUDE_SESSION));
    let mut conn = db::open_in_memory().unwrap();
    ingest_claude_code(&mut conn, &dir, &PriceTable::bundled(), &mut Quiet).unwrap();

    let items: Vec<(String, String, Option<i64>)> = conn
        .prepare("SELECT kind, name, chars FROM context_items ORDER BY kind, name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        items,
        vec![
            (
                "instructions".into(),
                "C:\\work\\demo\\CLAUDE.md".into(),
                Some(2_000)
            ),
            ("mcp_failed".into(), "broken".into(), None),
            ("skill_listing".into(), "skills".into(), Some(4_000)),
            ("tool".into(), "NotebookEdit".into(), None),
            ("tool".into(), "mcp__browser__click".into(), None),
        ]
    );
    let (server, detail): (String, String) = conn
        .query_row(
            "SELECT mcp_server, result_detail FROM tool_calls WHERE outcome = 'error'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(server, "browser");
    assert_eq!(detail, "login failed with token ghp_[redacted] rejected");
}

#[test]
fn copilot_tool_tokens_and_mcp_servers_are_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = copilot_log(
        tmp.path(),
        COPILOT_SESSION,
        &[
            copilot_start(COPILOT_SESSION),
            copilot_usage("2026-09-20T10:00:05.000Z", "u1", 31, 20_000, 37_000),
            copilot(
                "2026-09-20T10:00:06.000Z",
                "t1",
                "tool.execution_start",
                json!({"toolCallId": "tool-1", "toolName": "github-mcp-server-get_file_contents",
                       "mcpServerName": "github-mcp-server", "arguments": {}}),
            ),
        ],
    );
    let mut conn = db::open_in_memory().unwrap();
    ingest_copilot_cli(&mut conn, &dir, &mut Quiet).unwrap();
    let sample: (i64, i64, i64) = conn
        .query_row(
            "SELECT tool_count, tool_tokens, prompt_tokens FROM context_samples",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(sample, (31, 20_000, 37_000));
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM tool_calls WHERE mcp_server = 'github-mcp-server'"
        ),
        1
    );
}

#[test]
fn upgrading_backfills_context_from_stored_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = claude_log(tmp.path(), CLAUDE_SESSION, &context_session(CLAUDE_SESSION));
    let path = tmp.path().join("calvin.db");
    {
        let mut conn = db::open(&path).unwrap();
        ingest_claude_code(&mut conn, &dir, &PriceTable::bundled(), &mut Quiet).unwrap();
        // Make it look like a v7 database: no context, no MCP servers, no error previews.
        conn.execute_batch(
            "DELETE FROM context_items;
             UPDATE tool_calls SET mcp_server = NULL, result_detail = NULL;
             PRAGMA user_version = 7;",
        )
        .unwrap();
    }
    let conn = db::open(&path).unwrap();
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM context_items"), 5);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM tool_calls WHERE mcp_server = 'browser'
             AND result_detail LIKE '%[redacted]%'"
        ),
        1
    );
}

#[test]
fn claude_context_budget_and_mcp_servers() {
    let tmp = tempfile::tempdir().unwrap();
    let mut claude_dir = PathBuf::new();
    for n in 1..=5 {
        let id = session_id(n);
        // A long skill list, and the browser server loads every time but is never called.
        claude_dir = claude_log(tmp.path(), &id, &context_session_with(&id, 40_000, "Bash"));
    }
    let mut conn = db::open_in_memory().unwrap();
    ingest_claude_code(&mut conn, &claude_dir, &PriceTable::bundled(), &mut Quiet).unwrap();
    let items = run_opportunities(&conn, tmp.path(), &claude_dir, &tmp.path().join("copilot"));

    let budget = find(&items, "context-budget").expect("context budget");
    assert_eq!(budget.status, Status::Consider);
    assert!(
        budget.finding.contains("about 10,500 tokens"),
        "{}",
        budget.finding
    );
    assert_eq!(budget.evidence[0].label, "Skill list");
    assert!(budget.metric.as_ref().unwrap().lower_is_better);

    let failing = find(&items, "mcp-failing").expect("failing servers");
    assert_eq!(failing.status, Status::Action);
    assert_eq!(failing.evidence[0].label, "broken");
    assert_eq!(failing.snippet.as_deref(), Some("claude mcp remove broken"));

    let unused = find(&items, "mcp-unused").expect("unused servers");
    assert_eq!(unused.status, Status::Action);
    assert_eq!(unused.evidence[0].label, "browser");
    assert_eq!(
        unused.evidence[0].detail.as_deref(),
        Some("loaded in 5 sessions")
    );
}

#[test]
fn small_claude_context_is_good_and_used_servers_are_not_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    let mut claude_dir = PathBuf::new();
    for n in 1..=5 {
        let id = session_id(n);
        claude_dir = claude_log(tmp.path(), &id, &context_session(&id));
    }
    let mut conn = db::open_in_memory().unwrap();
    ingest_claude_code(&mut conn, &claude_dir, &PriceTable::bundled(), &mut Quiet).unwrap();
    let items = run_opportunities(&conn, tmp.path(), &claude_dir, &tmp.path().join("copilot"));
    assert_eq!(find(&items, "context-budget").unwrap().status, Status::Good);
    assert_eq!(find(&items, "mcp-unused").unwrap().status, Status::Good);
}

fn copilot_sessions(root: &Path, sessions: usize, tool_tokens: i64) -> PathBuf {
    let mut dir = PathBuf::new();
    for n in 0..sessions {
        let id = format!("22222222-2222-4222-8222-{n:012}");
        dir = copilot_log(
            root,
            &id,
            &[
                copilot_start(&id),
                copilot_usage("2026-09-20T10:00:05.000Z", "u1", 30, tool_tokens, 40_000),
                copilot_usage("2026-09-20T10:00:09.000Z", "u2", 30, tool_tokens, 60_000),
                copilot(
                    "2026-09-20T10:00:06.000Z",
                    "t1",
                    "tool.execution_start",
                    json!({"toolCallId": format!("tool-{n}"), "toolName": "github-mcp-server-get_file_contents",
                           "mcpServerName": "github-mcp-server", "arguments": {}}),
                ),
            ],
        );
    }
    dir
}

#[test]
fn copilot_tool_definitions_and_unused_servers() {
    let tmp = tempfile::tempdir().unwrap();
    let copilot_dir = copilot_sessions(tmp.path(), 5, 15_000);
    fs::write(
        copilot_dir.join("mcp-config.json"),
        r#"{"mcpServers": {"idle": {}, "parked": {}, "github-mcp-server": {}}}"#,
    )
    .unwrap();
    fs::write(
        copilot_dir.join("config.json"),
        "{\n  // parked on purpose\n  \"disabledMcpServers\": [\"parked\"]\n}",
    )
    .unwrap();
    let mut conn = db::open_in_memory().unwrap();
    ingest_copilot_cli(&mut conn, &copilot_dir, &mut Quiet).unwrap();
    let items = run_opportunities(&conn, tmp.path(), &tmp.path().join("claude"), &copilot_dir);

    let budget = find(&items, "copilot-context-budget").expect("tool budget");
    assert_eq!(budget.status, Status::Consider);
    assert!(
        budget.finding.contains("about 15,000 tokens"),
        "{}",
        budget.finding
    );
    assert!(
        budget.finding.contains("30% of all prompt tokens"),
        "{}",
        budget.finding
    );
    assert_eq!(budget.snippet.as_deref(), Some("/settings toolSearch on"));

    let unused = find(&items, "copilot-mcp-unused").expect("unused servers");
    assert_eq!(unused.status, Status::Action);
    let labels: Vec<_> = unused.evidence.iter().map(|e| e.label.as_str()).collect();
    assert_eq!(labels, ["idle"]);
    assert_eq!(unused.snippet.as_deref(), Some("copilot mcp disable idle"));
}

#[test]
fn copilot_with_tool_search_and_few_tools_is_good() {
    let tmp = tempfile::tempdir().unwrap();
    let copilot_dir = copilot_sessions(tmp.path(), 3, 4_000);
    fs::write(copilot_dir.join("settings.json"), r#"{"toolSearch": true}"#).unwrap();
    let mut conn = db::open_in_memory().unwrap();
    ingest_copilot_cli(&mut conn, &copilot_dir, &mut Quiet).unwrap();
    let items = run_opportunities(&conn, tmp.path(), &tmp.path().join("claude"), &copilot_dir);
    let budget = find(&items, "copilot-context-budget").unwrap();
    assert_eq!(budget.status, Status::Good);
    assert_eq!(
        budget.snippet.as_deref(),
        Some("/mcp disable <server-name>")
    );
    // Too few sessions to call a server unused.
    assert!(find(&items, "copilot-mcp-unused").is_none());
}
