use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use calvin::db;
use calvin::ingest::{Quiet, ingest_claude_code};
use calvin::prices::PriceTable;
use calvin::server::{AppState, SkillFolders, router};
use http_body_util::BodyExt;
use tower::ServiceExt;

const PORT: u16 = 1982;

fn app() -> (tempfile::TempDir, axum::Router) {
    let tmp = tempfile::tempdir().unwrap();
    let claude = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude-code");
    let db_path = tmp.path().join("calvin.db");
    let mut conn = db::open(&db_path).unwrap();
    ingest_claude_code(&mut conn, &claude, &PriceTable::bundled(), &mut Quiet).unwrap();
    let state = AppState::new(
        tmp.path().to_path_buf(),
        false,
        db_path,
        claude,
        tmp.path().join("copilot"),
        tmp.path().join("cursor"),
        tmp.path().join("cursor-state.vscdb"),
        SkillFolders::default(),
        PriceTable::bundled(),
        "secret".into(),
        PORT,
    );
    (tmp, router(Arc::new(state)))
}

fn get(path: &str, host: &str) -> Request<Body> {
    Request::get(path)
        .header("host", host)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn only_answers_to_localhost() {
    let (_tmp, app) = app();
    let ok = app
        .clone()
        .oneshot(get("/api/health", "127.0.0.1:1982"))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let ok = app
        .clone()
        .oneshot(get("/api/health", "localhost:1982"))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    let rebound = app
        .oneshot(get("/api/health", "evil.example:1982"))
        .await
        .unwrap();
    assert_eq!(rebound.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn every_page_shows_the_update_banner() {
    let (_tmp, app) = app();
    let res = app
        .clone()
        .oneshot(get("/update.js", "127.0.0.1:1982"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()["content-type"].to_str().unwrap(),
        "text/javascript; charset=utf-8"
    );
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("window.calvinUpdate"));

    for page in ["/", "/opportunities", "/sessions", "/skills"] {
        let res = app
            .clone()
            .oneshot(get(page, "127.0.0.1:1982"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{page}");
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert!(
            String::from_utf8_lossy(&body).contains(r#"<script src="/update.js"></script>"#),
            "{page} doesn't load the update banner"
        );
    }
}

#[tokio::test]
async fn every_page_shows_loading_progress() {
    let (_tmp, app) = app();
    let res = app
        .clone()
        .oneshot(get("/loading.js", "127.0.0.1:1982"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()["content-type"].to_str().unwrap(),
        "text/javascript; charset=utf-8"
    );
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("window.calvinLoading"));

    for page in ["/", "/opportunities", "/sessions", "/skills"] {
        let res = app
            .clone()
            .oneshot(get(page, "127.0.0.1:1982"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{page}");
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert!(
            String::from_utf8_lossy(&body).contains(r#"<script src="/loading.js"></script>"#),
            "{page} doesn't load the loading indicator"
        );
    }
}

#[tokio::test]
async fn shutdown_needs_the_token() {
    let (_tmp, app) = app();
    let post = |token: Option<&str>| {
        let mut req = Request::post("/api/shutdown").header("host", "127.0.0.1:1982");
        if let Some(t) = token {
            req = req.header("x-calvin-token", t);
        }
        req.body(Body::empty()).unwrap()
    };
    assert_eq!(
        app.clone().oneshot(post(None)).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.clone()
            .oneshot(post(Some("wrong")))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.oneshot(post(Some("secret"))).await.unwrap().status(),
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn summary_is_json_from_the_database() {
    let (_tmp, app) = app();
    let res = app
        .clone()
        .oneshot(get("/api/summary?since=all", "127.0.0.1:1982"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["requests"], 3);
    assert_eq!(v["prompts"], 4);

    let filtered = app
        .clone()
        .oneshot(get(
            "/api/summary?since=all&harness=claude-code",
            "127.0.0.1:1982",
        ))
        .await
        .unwrap();
    assert_eq!(filtered.status(), StatusCode::OK);
    let body = filtered.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["sessions"], 1);
    assert_eq!(v["requests"], 3);

    let bad = app
        .oneshot(get("/api/summary?since=soon", "127.0.0.1:1982"))
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn daily_billing_keeps_copilot_ai_units_separate() {
    let (tmp, app) = app();
    let conn = db::open(&tmp.path().join("calvin.db")).unwrap();
    conn.execute(
        "INSERT INTO sessions (id, harness, started_at)
         VALUES ('copilot-cli:billing', 'copilot-cli', '2026-09-30T12:00:00Z')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO requests
         (request_id, session_id, ts, model, input_tokens, output_tokens, cache_read,
          cache_write_5m, cache_write_1h, ai_units)
         VALUES ('copilot-billing', 'copilot-cli:billing', '2026-09-30T12:01:00Z', 'gpt-test',
                 0, 0, 0, 0, 0, 2.5)",
        [],
    )
    .unwrap();
    drop(conn);

    let res = app
        .clone()
        .oneshot(get(
            "/api/cost?by=day&since=all&harness=copilot-cli",
            "127.0.0.1:1982",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let rows: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(rows[0]["cost_usd"], 0.0);
    assert_eq!(rows[0]["ai_units"], 2.5);

    let res = app
        .oneshot(get(
            "/api/models?since=all&harness=copilot-cli",
            "127.0.0.1:1982",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let models: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(models[0]["model"], "gpt-test");
    assert_eq!(models[0]["ai_units"], 2.5);
}

#[tokio::test]
async fn cursor_is_a_supported_provider_filter() {
    let (tmp, app) = app();
    let conn = db::open(&tmp.path().join("calvin.db")).unwrap();
    conn.execute(
        "INSERT INTO sessions (id, harness, started_at)
         VALUES ('cursor:thread', 'cursor', '2026-09-30T12:00:00Z')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO requests
         (request_id, session_id, ts, model, input_tokens, output_tokens, is_sidechain)
         VALUES ('cursor:request', 'cursor:thread', '2026-09-30T12:01:00Z',
                 'claude-4.5-sonnet-thinking', 100, 20, 0)",
        [],
    )
    .unwrap();
    drop(conn);

    let res = app
        .oneshot(get(
            "/api/summary?since=all&harness=cursor",
            "127.0.0.1:1982",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let summary: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(summary["sessions"], 1);
    assert_eq!(summary["requests"], 1);
}

#[tokio::test]
async fn advisor_preview_requires_docs_for_every_harness() {
    let (tmp, app) = app();
    let conn = db::open(&tmp.path().join("calvin.db")).unwrap();
    conn.execute(
        "INSERT INTO sessions (id, harness, started_at)
         VALUES ('copilot-cli:test', 'copilot-cli', '2026-09-30T12:00:00Z'),
                ('cursor:test', 'cursor', '2026-09-30T12:00:00Z')",
        [],
    )
    .unwrap();
    drop(conn);

    let res = app
        .oneshot(get("/api/advisor/preview?since=all", "127.0.0.1:1982"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let prompt = v["prompt"].as_str().unwrap();
    assert!(prompt.contains("## Required official documentation sources"));
    assert!(prompt.contains("https://code.claude.com/docs/llms.txt"));
    assert!(prompt.contains("https://docs.github.com/en/copilot/how-tos/copilot-cli"));
    assert!(prompt.contains(
        "https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference"
    ));
    assert!(prompt.contains("https://cursor.com/docs/llms.txt"));
}

#[tokio::test]
async fn pack_only_accepts_installed_skill_names() {
    let (_tmp, app) = app();
    for path in [
        "/api/skills/pack?names=../../etc",
        "/api/skills/pack?names=not-a-skill",
        "/api/skills/pack",
        "/api/skills/detail?name=C:%5CWindows",
    ] {
        let res = app
            .clone()
            .oneshot(get(path, "127.0.0.1:1982"))
            .await
            .unwrap();
        assert!(
            res.status().is_client_error() || res.status().is_server_error(),
            "{path} → {}",
            res.status()
        );
        assert_ne!(
            res.headers()
                .get("content-type")
                .map(|v| v.to_str().unwrap()),
            Some("application/zip")
        );
    }
}

#[tokio::test]
async fn open_folder_needs_the_action_header() {
    let (_tmp, app) = app();
    let req = |header: bool| {
        let mut r =
            Request::post("/api/skills/open?name=anything").header("host", "127.0.0.1:1982");
        if header {
            r = r.header("x-calvin-action", "1");
        }
        r.body(Body::empty()).unwrap()
    };
    // A plain cross-site form post can't add the header.
    assert_eq!(
        app.clone().oneshot(req(false)).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    // With it, unknown skills are still refused, so no arbitrary path is ever opened.
    assert_eq!(
        app.oneshot(req(true)).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn sessions_list_and_timeline() {
    let (_tmp, app) = app();
    let res = app
        .clone()
        .oneshot(get("/api/sessions?since=all", "127.0.0.1:1982"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["title"], "Fix failing tests");
    assert_eq!(list[0]["friction"], 3);
    assert_eq!(list[0]["cwd"], r"C:\work\demo");

    let id = list[0]["id"].as_str().unwrap();
    let res = app
        .clone()
        .oneshot(get(
            &format!("/api/sessions/detail?id={id}"),
            "127.0.0.1:1982",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let s: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let turns = s["turns"].as_array().unwrap();
    // Prompts and the /shipit command each start a turn.
    let prompts: Vec<_> = turns.iter().filter_map(|t| t["prompt"].as_str()).collect();
    assert_eq!(prompts[0], "run the tests and fix whatever fails");
    assert!(prompts.contains(&"/shipit commit staged"));
    // The first turn holds the Bash call with its result and the split response counted once.
    let first = &turns[0];
    let tool = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "tool")
        .unwrap();
    assert_eq!(
        (
            tool["name"].as_str(),
            tool["summary"].as_str(),
            tool["outcome"].as_str()
        ),
        (Some("Bash"), Some("cargo test"), Some("ok"))
    );
    assert_eq!(tool["result"], "test result: ok");
    // The same turn continues through the Skill call (rejected) and the interruption.
    let kinds: Vec<_> = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["type"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"interrupted"));

    let missing = app
        .oneshot(get("/api/sessions/detail?id=nope", "127.0.0.1:1982"))
        .await
        .unwrap();
    assert!(!missing.status().is_success());
}
