use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use calvin::db;
use calvin::ingest::{Quiet, ingest_claude_code, ingest_copilot_cli};
use calvin::insights::{self, CostBy, Since};
use calvin::prices::PriceTable;
use calvin::timeline;
use rusqlite::Connection;

const SESSION: &str = "00000000-0000-4000-8000-000000000001";

/// Copy the fixture into a temp dir so tests can append to it.
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-code");
    let dst = tmp.path().join("claude");
    copy_dir(&src, &dst);
    (tmp, dst)
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn log_file(claude: &Path) -> PathBuf {
    claude
        .join("projects/C--work-demo")
        .join(format!("{SESSION}.jsonl"))
}

fn ingest(conn: &mut Connection, claude: &Path) -> calvin::ingest::Stats {
    ingest_claude_code(conn, claude, &PriceTable::bundled(), &mut Quiet).unwrap()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn copilot_fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/copilot-cli");
    let dst = tmp.path().join("copilot");
    copy_dir(&src, &dst);
    let store = Connection::open(dst.join("session-store.db")).unwrap();
    store
        .execute_batch(
            "CREATE TABLE sessions (
                 id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, host_type TEXT, branch TEXT,
                 summary TEXT, created_at TEXT, updated_at TEXT
             );
             CREATE TABLE turns (
                 id INTEGER PRIMARY KEY, session_id TEXT, turn_index INTEGER, user_message TEXT,
                 assistant_response TEXT, timestamp TEXT
             );
             CREATE TABLE assistant_usage_events (
                 id INTEGER PRIMARY KEY, session_id TEXT, turn_index INTEGER, agent_id TEXT,
                 parent_tool_call_id TEXT, model TEXT, copilot_usage_model TEXT,
                 input_tokens INTEGER, output_tokens INTEGER, cache_read_tokens INTEGER,
                 cache_write_tokens INTEGER, reasoning_tokens INTEGER, total_nano_aiu INTEGER,
                 request_multiplier REAL, duration_ms INTEGER, time_to_first_token_ms INTEGER,
                 output_ttft_ms REAL, inter_token_latency_ms INTEGER, initiator TEXT,
                 api_endpoint TEXT, reasoning_effort TEXT, finish_reason TEXT,
                 content_filter_triggered INTEGER, token_details_json TEXT, created_at TEXT
             );
             CREATE TABLE session_files (
                 id INTEGER PRIMARY KEY, session_id TEXT, file_path TEXT, tool_name TEXT,
                 turn_index INTEGER, first_seen_at TEXT
             );
             CREATE TABLE session_refs (
                 id INTEGER PRIMARY KEY, session_id TEXT, ref_type TEXT, ref_value TEXT,
                 turn_index INTEGER, created_at TEXT
             );
             INSERT INTO sessions VALUES (
                 '11111111-1111-4111-8111-111111111111', 'C:\\work\\demo', 'demo',
                 'cli', 'main', 'Improve tests', '2026-09-15 10:00:00', '2026-09-15 10:05:00'
             );
             INSERT INTO turns VALUES (
                 1, '11111111-1111-4111-8111-111111111111', 0,
                 'run the tests', 'All tests pass.', '2026-09-15 10:00:30'
             );
             INSERT INTO assistant_usage_events
                 (id, session_id, turn_index, model, input_tokens, output_tokens,
                  cache_read_tokens, cache_write_tokens, total_nano_aiu, duration_ms,
                  reasoning_effort, created_at)
             VALUES (
                 1, '11111111-1111-4111-8111-111111111111', 0, 'gpt-test',
                 100, 20, 50, 5, 1500000000, 1200, 'high', '2026-09-15 10:00:31'
             );
             INSERT INTO session_files VALUES (
                 1, '11111111-1111-4111-8111-111111111111', 'src/lib.rs',
                 'apply_patch', 0, '2026-09-15 10:01:00'
             );
             INSERT INTO session_refs VALUES (
                 1, '11111111-1111-4111-8111-111111111111', 'issue', '42',
                 0, '2026-09-15 10:02:00'
             );",
        )
        .unwrap();
    (tmp, dst)
}

#[test]
fn imports_copilot_cli_store_and_events() {
    let (_tmp, copilot) = copilot_fixture();
    let mut conn = db::open_in_memory().unwrap();
    let stats = ingest_copilot_cli(&mut conn, &copilot, &mut Quiet).unwrap();
    assert_eq!((stats.files_read, stats.lines), (1, 5));
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM sessions WHERE harness = 'copilot-cli'"
        ),
        1
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM prompts"), 2);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM tool_calls"), 1);
    let (tokens, ai_units, turn_index): (i64, f64, i64) = conn
        .query_row(
            "SELECT output_tokens, ai_units, turn_index FROM requests",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(tokens, 20);
    assert!((ai_units - 1.5).abs() < f64::EPSILON);
    assert_eq!(turn_index, 0);
    let (duration_ms, detail): (i64, String) = conn
        .query_row(
            "SELECT duration_ms, result_detail FROM tool_calls",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!((4_999..=5_001).contains(&duration_ms));
    assert_eq!(detail, "tests passed");
    let command: String = conn
        .query_row("SELECT text FROM prompts WHERE kind = 'command'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(command, "/review");
    let mode: String = conn
        .query_row("SELECT mode FROM session_modes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "plan");
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM session_files"), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM session_refs"), 1);

    let session = timeline::session(&conn, "copilot-cli:11111111-1111-4111-8111-111111111111")
        .unwrap()
        .unwrap();
    assert_eq!(session.turns[0].requests, 1);
    assert!((session.turns[0].ai_units - 1.5).abs() < f64::EPSILON);
    assert_eq!(session.turns[0].output_tokens, 20);

    let activity = insights::recent_activity(&conn, 20).unwrap();
    assert!(activity.iter().all(|item| item.harness == "copilot-cli"));
}

#[test]
fn copilot_inventory_and_recommendations_use_provider_evidence() {
    let (tmp, copilot) = copilot_fixture();
    std::fs::write(
        copilot.join("mcp-config.json"),
        r#"{"mcpServers":{"github":{"command":"secret-value"}}}"#,
    )
    .unwrap();
    std::fs::write(
        copilot.join("permissions-config.json"),
        r#"{"locations":{"C:\\work\\demo":{"allow":["secret-value"]}}}"#,
    )
    .unwrap();
    let mut conn = db::open_in_memory().unwrap();
    ingest_copilot_cli(&mut conn, &copilot, &mut Quiet).unwrap();
    conn.execute_batch(
        "INSERT INTO sessions
             (id, harness, project, cwd, title, started_at, ended_at)
         VALUES
             ('copilot-cli:second', 'copilot-cli', 'demo', 'C:\\work\\demo',
              'Second test run', '2026-09-15T11:00:00Z', '2026-09-15T11:05:00Z');
         INSERT INTO prompts (id, session_id, ts, kind, text, is_sidechain) VALUES
             ('repeat-1', 'copilot-cli:11111111-1111-4111-8111-111111111111',
              '2026-09-15T10:03:00Z', 'prompt', 'run the tests', 0),
             ('repeat-2', 'copilot-cli:11111111-1111-4111-8111-111111111111',
              '2026-09-15T10:04:00Z', 'prompt', 'run the tests', 0),
             ('repeat-3', 'copilot-cli:second',
              '2026-09-15T11:01:00Z', 'prompt', 'run the tests', 0);
         INSERT INTO session_files (session_id, file_path, tool_name, turn_index) VALUES
             ('copilot-cli:second', 'src/a.rs', 'apply_patch', 0),
             ('copilot-cli:second', 'src/b.rs', 'apply_patch', 0),
             ('copilot-cli:second', 'src/c.rs', 'apply_patch', 0),
             ('copilot-cli:second', 'src/d.rs', 'apply_patch', 0),
             ('copilot-cli:second', 'src/e.rs', 'apply_patch', 0);
         INSERT INTO requests
             (request_id, session_id, ts, model, input_tokens, output_tokens, is_sidechain)
         VALUES
             ('high-context', 'copilot-cli:second', '2026-09-15T11:02:00Z',
              'gpt-test', 100000, 10, 0);",
    )
    .unwrap();

    let inventory = calvin::inventory::copilot_markdown(&conn, &Since::all(), &copilot).unwrap();
    assert!(inventory.contains("Modes entered: plan (1)"));
    assert!(inventory.contains("MCP servers configured (names only): github"));
    assert!(inventory.contains("Permission locations configured: 1"));
    assert!(!inventory.contains("secret-value"));

    let prices = PriceTable::bundled();
    let ctx = calvin::opportunities::Context {
        claude_dir: tmp.path(),
        copilot_dir: &copilot,
        prices: &prices,
        skills: &[],
    };
    let found = calvin::opportunities::run(&conn, &Since::all(), &ctx).unwrap();
    let ids: Vec<_> = found.iter().map(|opportunity| opportunity.id).collect();
    assert!(ids.contains(&"copilot-plan-mode"));
    assert!(ids.contains(&"copilot-compact"));
    assert!(ids.contains(&"copilot-review"));
    assert!(ids.contains(&"copilot-autopilot"));
    for feature in [
        "copilot-hooks",
        "copilot-status-line",
        "copilot-command-history",
        "copilot-subagents",
        "copilot-worktrees",
        "copilot-notifications",
        "copilot-keep-alive",
        "copilot-memory",
        "copilot-extensions",
        "copilot-handoff",
        "copilot-context-controls",
        "copilot-prompt-tools",
        "copilot-development-integrations",
        "copilot-session-navigation",
    ] {
        assert!(ids.contains(&feature), "missing {feature}");
    }
    assert_eq!(
        found
            .iter()
            .find(|opportunity| opportunity.id == "copilot-plan-mode")
            .unwrap()
            .status,
        calvin::opportunities::Status::Consider
    );
    assert_eq!(
        found
            .iter()
            .find(|opportunity| opportunity.id == "copilot-extensions")
            .unwrap()
            .status,
        calvin::opportunities::Status::Good
    );
}

#[test]
fn imports_the_fixture() {
    let (_tmp, claude) = fixture();
    let mut conn = db::open_in_memory().unwrap();
    let stats = ingest(&mut conn, &claude);
    assert_eq!(stats.files_read, 1);
    assert_eq!(stats.lines, 18);
    assert_eq!(stats.bad_lines, 1);

    // Session metadata.
    let (project, title, branch): (String, String, String) = conn
        .query_row(
            "SELECT project, title, git_branch FROM sessions WHERE id = ?1",
            [SESSION],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (project.as_str(), title.as_str(), branch.as_str()),
        ("demo", "Fix failing tests", "main")
    );

    // Prompts: meta records, tool results, reminders and the interrupt marker are not prompts.
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM prompts WHERE kind = 'prompt'"),
        4
    );
    let command: String = conn
        .query_row("SELECT text FROM prompts WHERE kind = 'command'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(command, "/shipit commit staged");

    // One response split over two log lines counts once; <synthetic> is not a request.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM requests"), 3);
    let (output, read): (i64, i64) = conn
        .query_row(
            "SELECT output_tokens, cache_read FROM requests WHERE request_id = 'req_1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((output, read), (100, 1000));

    // opus-5 req_1: 10×5 + 100×25 + 1000×0.5 + 200×6.25 = 4300
    // haiku req_2:  5×1 + 50×5 + 100×1.25 (legacy total priced as 5m write) = 380
    // opus-5 req_4: 1×5 + 1×25 = 30
    let cost: f64 = conn
        .query_row("SELECT SUM(cost_usd) FROM requests", [], |r| r.get(0))
        .unwrap();
    assert!((cost - 4710.0 / 1e6).abs() < 1e-12, "cost was {cost}");

    // Tool outcomes and friction.
    let outcomes: Vec<(String, String)> = conn
        .prepare("SELECT id, outcome FROM tool_calls ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        outcomes,
        vec![
            ("tool_1".into(), "ok".into()),
            ("tool_2".into(), "rejected".into()),
            ("tool_3".into(), "denied".into())
        ]
    );
    let friction: Vec<(String, Option<String>)> = conn
        .prepare("SELECT kind, tool FROM friction ORDER BY kind")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        friction,
        vec![
            ("denied".into(), Some("Bash".into())),
            ("interrupted".into(), None),
            ("rejected".into(), Some("Skill".into()))
        ]
    );

    // Raw copy: every valid line kept, image data stripped.
    let (lines, zdata): (i64, Vec<u8>) = conn
        .query_row("SELECT lines, zdata FROM raw_chunks", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(lines, 17);
    let raw = String::from_utf8(zstd::decode_all(&zdata[..]).unwrap()).unwrap();
    assert!(!raw.contains("iVBORw0KGgo"));
    assert!(raw.contains("<stripped by calvin>"));
}

#[test]
fn insights_over_the_fixture() {
    let (_tmp, claude) = fixture();
    let mut conn = db::open_in_memory().unwrap();
    ingest(&mut conn, &claude);
    let all = Since::all();

    let s = insights::summary(&conn, &all).unwrap();
    assert_eq!(
        (s.sessions, s.prompts, s.commands, s.requests),
        (1, 4, 1, 3)
    );

    let skills = insights::skills(&conn, &all).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!((skills[0].skill.as_str(), skills[0].runs), ("shipit", 1));

    let repeated = insights::repeated_prompts(&conn, &all, 3).unwrap();
    assert_eq!(repeated.len(), 1);
    assert_eq!(repeated[0].count, 3);

    let by_model = insights::cost(&conn, &all, CostBy::Model).unwrap();
    assert_eq!(by_model[0].key, "claude-opus-5");

    let commands = insights::commands(&conn, &all).unwrap();
    assert_eq!(
        (commands[0].command.as_str(), commands[0].uses),
        ("/shipit", 1)
    );

    // Nothing after the fixture's date.
    let later = Since::parse("2030-01-01").unwrap();
    assert_eq!(insights::summary(&conn, &later).unwrap().requests, 0);
}

#[test]
fn reingest_reads_only_new_complete_lines() {
    let (_tmp, claude) = fixture();
    let mut conn = db::open_in_memory().unwrap();
    ingest(&mut conn, &claude);

    let again = ingest(&mut conn, &claude);
    assert_eq!((again.files_read, again.lines), (0, 0));

    // A line still being written (no trailing newline) is left for next time.
    let mut f = OpenOptions::new()
        .append(true)
        .open(log_file(&claude))
        .unwrap();
    let line = format!(
        r#"{{"type":"user","sessionId":"{SESSION}","uuid":"u99","timestamp":"2026-09-01T11:00:00.000Z","message":{{"role":"user","content":"one more thing"}}}}"#
    );
    write!(f, "{line}").unwrap();
    f.flush().unwrap();
    assert_eq!(ingest(&mut conn, &claude).lines, 0);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM prompts WHERE id = 'u99'"),
        0
    );

    writeln!(f).unwrap();
    f.flush().unwrap();
    assert_eq!(ingest(&mut conn, &claude).lines, 1);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM prompts WHERE id = 'u99'"),
        1
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM raw_chunks"), 2);
}

#[test]
fn background_task_notifications_are_not_prompts() {
    let (_tmp, claude) = fixture();
    let mut f = OpenOptions::new()
        .append(true)
        .open(log_file(&claude))
        .unwrap();
    writeln!(
        f,
        r#"{{"type":"user","sessionId":"{SESSION}","uuid":"u50","timestamp":"2026-09-01T11:05:00.000Z","message":{{"role":"user","content":"<task-notification>\n<task-id>abc</task-id>\n<status>completed</status>\n</task-notification>"}}}}"#
    )
    .unwrap();
    let mut conn = db::open_in_memory().unwrap();
    ingest(&mut conn, &claude);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM prompts WHERE id = 'u50'"),
        0
    );
}

#[test]
fn model_usage_per_model() {
    let (_tmp, claude) = fixture();
    let mut conn = db::open_in_memory().unwrap();
    ingest(&mut conn, &claude);
    let rows = insights::models(&conn, &Since::all()).unwrap();
    let names: Vec<_> = rows.iter().map(|r| r.model.as_str()).collect();
    assert_eq!(names, vec!["claude-opus-5", "claude-haiku-4-5-20251001"]);
    assert_eq!((rows[0].requests, rows[0].output_tokens), (2, 101));
    assert_eq!(rows[1].requests, 1);
}

#[test]
fn effort_is_recorded_per_request() {
    let (_tmp, claude) = fixture();
    let mut f = OpenOptions::new()
        .append(true)
        .open(log_file(&claude))
        .unwrap();
    writeln!(
        f,
        r#"{{"type":"assistant","sessionId":"{SESSION}","uuid":"a99","timestamp":"2026-09-01T11:10:00.000Z","requestId":"req_effort","effort":"xhigh","message":{{"model":"claude-opus-5","role":"assistant","content":[{{"type":"text","text":"done"}}],"usage":{{"input_tokens":1,"output_tokens":1}}}}}}"#
    )
    .unwrap();
    let mut conn = db::open_in_memory().unwrap();
    ingest(&mut conn, &claude);
    let effort: Option<String> = conn
        .query_row(
            "SELECT effort FROM requests WHERE request_id = 'req_effort'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(effort.as_deref(), Some("xhigh"));
    let opus = insights::models(&conn, &Since::all())
        .unwrap()
        .into_iter()
        .find(|m| m.model == "claude-opus-5")
        .unwrap();
    assert!(
        opus.efforts
            .iter()
            .any(|e| e.effort.as_deref() == Some("xhigh") && e.requests == 1)
    );
}

#[test]
fn opportunities_run_on_the_fixture() {
    let (tmp, claude) = fixture();
    let mut conn = db::open_in_memory().unwrap();
    ingest(&mut conn, &claude);
    // A project folder the fixture session could have run in, without CLAUDE.md.
    std::fs::create_dir_all(tmp.path().join("work/demo")).unwrap();
    let prices = PriceTable::bundled();
    let ctx = calvin::opportunities::Context {
        claude_dir: &claude,
        copilot_dir: tmp.path(),
        prices: &prices,
        skills: &[],
    };
    let found = calvin::opportunities::run(&conn, &Since::all(), &ctx).unwrap();
    let ids: Vec<_> = found.iter().map(|o| o.id).collect();
    assert!(ids.contains(&"log-retention"));
    assert!(ids.contains(&"hooks"));
    // Sorted: actions before suggestions before things already in place.
    let order: Vec<_> = found.iter().map(|o| o.status).collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted);
}
