//! The local web server: the dashboard page plus a JSON API over [`insights`].
//!
//! Every API handler is a thin wrapper: parse query parameters, open a read connection,
//! call the matching `insights` function on a blocking thread, return JSON. All logic
//! lives in `insights`, shared with the terminal reports.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::insights::{self, CostBy, Since};
use crate::{db, ingest, prices::PriceTable, skills, update};

const INDEX_HTML: &str = include_str!("../web/index.html");
const LOGO_SVG: &str = include_str!("../assets/logo.svg");
const FONTS: &[(&str, &[u8])] = &[
    (
        "Outfit-400-700.woff2",
        include_bytes!("../web/fonts/Outfit-400-700.woff2"),
    ),
    (
        "JetBrainsMono-500-700.woff2",
        include_bytes!("../web/fonts/JetBrainsMono-500-700.woff2"),
    ),
    (
        "Silkscreen-400.woff2",
        include_bytes!("../web/fonts/Silkscreen-400.woff2"),
    ),
];
const SYNC_INTERVAL: Duration = Duration::from_secs(3);
/// How often to see whether the daily update check is due.
const UPDATE_POLL: Duration = Duration::from_secs(60 * 60);

pub struct AppState {
    pub data_dir: PathBuf,
    pub check_updates: bool,
    pub db_path: PathBuf,
    pub claude_dir: PathBuf,
    pub extra_skill_paths: Vec<PathBuf>,
    pub prices: PriceTable,
    pub token: String,
    pub port: u16,
    shutdown: watch::Sender<bool>,
    started: Instant,
    last_sync: Mutex<Option<SyncInfo>>,
}

#[derive(Clone, Serialize)]
struct SyncInfo {
    at: String,
    lines: usize,
    error: Option<String>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        data_dir: PathBuf,
        check_updates: bool,
        db_path: PathBuf,
        claude_dir: PathBuf,
        extra_skill_paths: Vec<PathBuf>,
        prices: PriceTable,
        token: String,
        port: u16,
    ) -> Self {
        Self {
            data_dir,
            check_updates,
            db_path,
            claude_dir,
            extra_skill_paths,
            prices,
            token,
            port,
            shutdown: watch::channel(false).0,
            started: Instant::now(),
            last_sync: Mutex::new(None),
        }
    }

    /// Ask the server and sync loop to stop.
    pub fn request_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// Resolves once shutdown has been requested (immediately if it already was).
    pub fn stopped(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let mut rx = self.shutdown.subscribe();
        async move {
            let _ = rx.wait_for(|stop| *stop).await;
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    INDEX_HTML,
                )
            }),
        )
        .route(
            "/logo.svg",
            get(|| async { ([(header::CONTENT_TYPE, "image/svg+xml")], LOGO_SVG) }),
        )
        .route("/fonts/{name}", get(font))
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/status", get(status))
        .route("/api/shutdown", post(shutdown))
        .route("/api/summary", get(summary))
        .route("/api/cost", get(cost))
        .route("/api/skills", get(skill_usage))
        .route("/api/prompts", get(prompts))
        .route("/api/friction", get(friction))
        .route("/api/cache", get(cache))
        .route("/api/live", get(live))
        .layer(middleware::from_fn_with_state(state.clone(), local_only))
        .with_state(state)
}

/// Reject requests whose Host isn't this machine. Binding to 127.0.0.1 already keeps other
/// machines out; this also stops a web page using DNS rebinding to read your data.
async fn local_only(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let allowed = [
        format!("127.0.0.1:{}", st.port),
        format!("localhost:{}", st.port),
    ];
    if allowed.iter().any(|a| a == host) {
        next.run(req).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "calvin only answers requests to localhost",
        )
            .into_response()
    }
}

/// Keep the database current: import new log lines every few seconds until shutdown.
pub async fn sync_loop(st: Arc<AppState>) {
    loop {
        let s = st.clone();
        let result =
            tokio::task::spawn_blocking(move || -> Result<usize> {
                let mut conn = db::open(&s.db_path)?;
                Ok(ingest::ingest_claude_code(
                    &mut conn,
                    &s.claude_dir,
                    &s.prices,
                    &mut ingest::Quiet,
                )?
                .lines)
            })
            .await;
        let (lines, error) = match result {
            Ok(Ok(lines)) => (lines, None),
            Ok(Err(e)) => (0, Some(format!("{e:#}"))),
            Err(e) => (0, Some(e.to_string())),
        };
        if let Some(e) = &error {
            eprintln!("sync failed: {e}");
        }
        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        *st.last_sync.lock().unwrap() = Some(SyncInfo { at, lines, error });
        tokio::select! {
            _ = tokio::time::sleep(SYNC_INTERVAL) => {}
            _ = st.stopped() => return,
        }
    }
}

async fn font(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    match FONTS.iter().find(|(n, _)| *n == name) {
        Some((_, bytes)) => (
            [
                (header::CONTENT_TYPE, "font/woff2"),
                (header::CACHE_CONTROL, "max-age=31536000, immutable"),
            ],
            *bytes,
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Keep the cached update check fresh (the check itself runs at most once a day).
pub async fn update_loop(st: Arc<AppState>) {
    if !st.check_updates {
        return;
    }
    loop {
        let dir = st.data_dir.clone();
        if let Ok(Ok(result)) =
            tokio::task::spawn_blocking(move || update::check_if_due(&dir)).await
            && let Some(e) = result.error
        {
            eprintln!("update check failed: {e}");
        }
        tokio::select! {
            _ = tokio::time::sleep(UPDATE_POLL) => {}
            _ = st.stopped() => return,
        }
    }
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

/// Run `f` against a read connection on a blocking thread.
async fn read<T, F>(st: &AppState, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T> + Send + 'static,
{
    let path = st.db_path.clone();
    let result = tokio::task::spawn_blocking(move || f(&db::open_read(&path)?)).await;
    match result {
        Ok(Ok(v)) => Ok(Json(v)),
        Ok(Err(e)) => Err(ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("{e:#}"),
        )),
        Err(e) => Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

#[derive(Deserialize)]
struct Params {
    since: Option<String>,
    by: Option<String>,
    min: Option<i64>,
    limit: Option<i64>,
}

impl Params {
    fn since(&self) -> Result<Since, ApiError> {
        Since::parse(self.since.as_deref().unwrap_or("30d"))
            .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))
    }
}

#[derive(Serialize)]
struct Status {
    version: &'static str,
    pid: u32,
    port: u16,
    uptime_secs: u64,
    last_sync: Option<SyncInfo>,
    db_bytes: Option<u64>,
    update_checks: bool,
    update: Option<update::Available>,
}

async fn status(State(st): State<Arc<AppState>>) -> Json<Status> {
    Json(Status {
        version: env!("CARGO_PKG_VERSION"),
        pid: std::process::id(),
        port: st.port,
        uptime_secs: st.started.elapsed().as_secs(),
        last_sync: st.last_sync.lock().unwrap().clone(),
        db_bytes: std::fs::metadata(&st.db_path).ok().map(|m| m.len()),
        update_checks: st.check_updates,
        update: update::available(&st.data_dir),
    })
}

async fn shutdown(State(st): State<Arc<AppState>>, headers: HeaderMap) -> StatusCode {
    let given = headers.get("x-calvin-token").and_then(|v| v.to_str().ok());
    if given != Some(st.token.as_str()) {
        return StatusCode::UNAUTHORIZED;
    }
    st.request_shutdown();
    StatusCode::ACCEPTED
}

async fn summary(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<insights::Summary> {
    let since = p.since()?;
    read(&st, move |c| insights::summary(c, &since)).await
}

async fn cost(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<Vec<insights::CostRow>> {
    let since = p.since()?;
    let by = match p.by.as_deref().unwrap_or("day") {
        "day" => CostBy::Day,
        "project" => CostBy::Project,
        "model" => CostBy::Model,
        other => {
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                format!("unknown grouping '{other}'"),
            ));
        }
    };
    read(&st, move |c| insights::cost(c, &since, by)).await
}

async fn skill_usage(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<Vec<insights::SkillUsage>> {
    let since = p.since()?;
    let claude_dir = st.claude_dir.clone();
    let extra = st.extra_skill_paths.clone();
    read(&st, move |c| {
        let installed = skills::installed(&claude_dir, &insights::project_dirs(c)?, &extra);
        insights::skill_usage(c, &since, &installed)
    })
    .await
}

async fn prompts(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<Vec<insights::RepeatedPrompt>> {
    let since = p.since()?;
    let (min, limit) = (p.min.unwrap_or(3), p.limit.unwrap_or(20).max(0) as usize);
    read(&st, move |c| {
        Ok(insights::repeated_prompts(c, &since, min)?
            .into_iter()
            .take(limit)
            .collect())
    })
    .await
}

async fn friction(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<insights::Friction> {
    let since = p.since()?;
    read(&st, move |c| insights::friction(c, &since)).await
}

async fn cache(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<Vec<insights::CacheRow>> {
    let since = p.since()?;
    read(&st, move |c| insights::cache(c, &since)).await
}

async fn live(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<Vec<insights::Activity>> {
    let limit = p.limit.unwrap_or(12).clamp(1, 200);
    read(&st, move |c| insights::recent_activity(c, limit)).await
}

/// Serve until `shutdown` is notified. The listener must already be bound.
pub async fn serve(listener: tokio::net::TcpListener, st: Arc<AppState>) -> Result<()> {
    let app = router(st.clone());
    tokio::spawn(sync_loop(st.clone()));
    tokio::spawn(update_loop(st.clone()));
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(st.stopped())
    .await?;
    Ok(())
}
