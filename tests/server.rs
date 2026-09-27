use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use calvin::db;
use calvin::ingest::{Quiet, ingest_claude_code};
use calvin::prices::PriceTable;
use calvin::server::{AppState, router};
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
        db_path,
        claude,
        vec![],
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

    let bad = app
        .oneshot(get("/api/summary?since=soon", "127.0.0.1:1982"))
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
}
