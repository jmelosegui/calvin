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
use crate::{db, ingest, pack, prices::PriceTable, skills, update};

const INDEX_HTML: &str = include_str!("../web/index.html");
const SKILLS_HTML: &str = include_str!("../web/skills.html");
const SESSIONS_HTML: &str = include_str!("../web/sessions.html");
const STYLE_CSS: &str = include_str!("../web/style.css");
/// SKILL.md previews are cut off beyond this.
const PREVIEW_BYTES: usize = 200 * 1024;
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
const SKILL_INDEX_INTERVAL: Duration = Duration::from_secs(60);
/// How often to see whether the daily update check is due.
const UPDATE_POLL: Duration = Duration::from_secs(60 * 60);

/// Skill folders from config.
#[derive(Debug, Default, Clone)]
pub struct SkillFolders {
    /// More places where skills are installed.
    pub extra: Vec<PathBuf>,
}

pub struct AppState {
    pub data_dir: PathBuf,
    pub check_updates: bool,
    pub db_path: PathBuf,
    pub claude_dir: PathBuf,
    pub skill_folders: SkillFolders,
    skill_index: Mutex<Option<Vec<skills::InstalledSkill>>>,
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
        skill_folders: SkillFolders,
        prices: PriceTable,
        token: String,
        port: u16,
    ) -> Self {
        Self {
            data_dir,
            check_updates,
            db_path,
            claude_dir,
            skill_folders,
            skill_index: Mutex::new(None),
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
        .route("/skills", get(|| async { html(SKILLS_HTML) }))
        .route("/sessions", get(|| async { html(SESSIONS_HTML) }))
        .route(
            "/style.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    STYLE_CSS,
                )
            }),
        )
        .route("/fonts/{name}", get(font))
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/status", get(status))
        .route("/api/shutdown", post(shutdown))
        .route("/api/summary", get(summary))
        .route("/api/cost", get(cost))
        .route("/api/sessions", get(session_list))
        .route("/api/sessions/detail", get(session_detail))
        .route("/api/skills", get(skill_usage))
        .route("/api/skills/detail", get(skill_detail))
        .route("/api/skills/pack", get(skill_pack))
        .route("/api/skills/open", post(skill_open))
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

fn html(body: &'static str) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body)
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
    id: Option<String>,
    name: Option<String>,
    names: Option<String>,
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
    let s = st.clone();
    read(&st, move |c| {
        let installed = installed_skills(&s, c)?;
        insights::skill_usage(c, &since, &installed)
    })
    .await
}

/// Installed skills, from the background index when it's ready. Building the index touches
/// every project folder, which can be slow on an offline network drive, so requests never
/// wait for it once it exists.
async fn session_list(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<Vec<insights::SessionRow>> {
    let since = p.since()?;
    let limit = p.limit.unwrap_or(500).clamp(1, 5000);
    read(&st, move |c| insights::sessions(c, &since, limit)).await
}

async fn session_detail(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<crate::timeline::SessionView> {
    let Some(id) = p.id.clone() else {
        return Err(ApiError(StatusCode::BAD_REQUEST, "missing ?id=".into()));
    };
    read(&st, move |c| {
        crate::timeline::session(c, &id)?.ok_or_else(|| anyhow::anyhow!("no session with id {id}"))
    })
    .await
}

fn installed_skills(st: &AppState, conn: &Connection) -> Result<Vec<skills::InstalledSkill>> {
    if let Some(cached) = st.skill_index.lock().unwrap().clone() {
        return Ok(cached);
    }
    let fresh = scan_skills(st, conn)?;
    *st.skill_index.lock().unwrap() = Some(fresh.clone());
    Ok(fresh)
}

fn scan_skills(st: &AppState, conn: &Connection) -> Result<Vec<skills::InstalledSkill>> {
    Ok(skills::installed(&skills::Locations {
        claude_dir: st.claude_dir.clone(),
        project_dirs: insights::project_dirs(conn)?,
        extra: st.skill_folders.extra.clone(),
    }))
}

/// Rebuild the skill index every minute, so new or edited skills show up.
pub async fn skill_index_loop(st: Arc<AppState>) {
    loop {
        let s = st.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = db::open_read(&s.db_path)?;
            let fresh = scan_skills(&s, &conn)?;
            *s.skill_index.lock().unwrap() = Some(fresh);
            Ok(())
        })
        .await;
        if let Ok(Err(e)) = result {
            eprintln!("skill index failed: {e:#}");
        }
        tokio::select! {
            _ = tokio::time::sleep(SKILL_INDEX_INTERVAL) => {}
            _ = st.stopped() => return,
        }
    }
}

#[derive(Serialize)]
struct SkillDetail {
    skill: skills::InstalledSkill,
    usage: Option<insights::SkillUsage>,
    plan: pack::PackPlan,
    skill_md: String,
    skill_md_truncated: bool,
}

async fn skill_detail(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<SkillDetail> {
    let Some(name) = p.name.clone() else {
        return Err(ApiError(StatusCode::BAD_REQUEST, "missing ?name=".into()));
    };
    let s = st.clone();
    read(&st, move |c| {
        let installed = installed_skills(&s, c)?;
        let skill = installed
            .iter()
            .find(|k| k.name == name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no installed skill named '{name}'"))?;
        let usage = insights::skill_usage(c, &Since::all(), &installed)?
            .into_iter()
            .find(|u| u.name == name);
        let plan = pack::plan(&skill)?;
        let text = std::fs::read_to_string(&skill.path).unwrap_or_default();
        let truncated = text.len() > PREVIEW_BYTES;
        let skill_md = if truncated {
            let cut = (0..=PREVIEW_BYTES)
                .rev()
                .find(|i| text.is_char_boundary(*i))
                .unwrap_or(0);
            text[..cut].to_string()
        } else {
            text
        };
        Ok(SkillDetail {
            skill,
            usage,
            plan,
            skill_md,
            skill_md_truncated: truncated,
        })
    })
    .await
}

/// Download a zip of the selected skills (`?names=a,b`). Only installed skills can be
/// packed, so this can never be used to read arbitrary paths.
/// Header the dashboard sends with requests that do something on this machine. Other
/// websites can't add custom headers to requests to calvin (there's no CORS), so this
/// stops a page you visit from triggering these endpoints.
const ACTION_HEADER: &str = "x-calvin-action";

/// Open a skill's folder in the system file manager.
async fn skill_open(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(p): Query<Params>,
) -> Response {
    if headers.get(ACTION_HEADER).is_none() {
        return ApiError(
            StatusCode::FORBIDDEN,
            format!("missing {ACTION_HEADER} header"),
        )
        .into_response();
    }
    let Some(name) = p.name.clone() else {
        return ApiError(StatusCode::BAD_REQUEST, "missing ?name=".into()).into_response();
    };
    let s = st.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = db::open_read(&s.db_path)?;
        let skill = installed_skills(&s, &conn)?
            .into_iter()
            .find(|k| k.name == name)
            .ok_or_else(|| anyhow::anyhow!("no installed skill named '{name}'"))?;
        open::that_detached(pack::skill_dir(&skill))?;
        Ok(())
    })
    .await;
    match result {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
        Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn skill_pack(State(st): State<Arc<AppState>>, Query(p): Query<Params>) -> Response {
    let names: Vec<String> = p
        .names
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        return ApiError(StatusCode::BAD_REQUEST, "missing ?names=".into()).into_response();
    }
    let s = st.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<(String, Vec<u8>)> {
        let conn = db::open_read(&s.db_path)?;
        let installed = installed_skills(&s, &conn)?;
        let mut plans = Vec::new();
        for name in &names {
            let skill = installed
                .iter()
                .find(|k| &k.name == name)
                .ok_or_else(|| anyhow::anyhow!("no installed skill named '{name}'"))?;
            plans.push(pack::plan(skill)?);
        }
        let file = match plans.as_slice() {
            [one] => format!("{}.zip", one.folder),
            many => format!("skills-{}.zip", many.len()),
        };
        let bytes = pack::write_zip(&plans, std::io::Cursor::new(Vec::new()))?.into_inner();
        Ok((file, bytes))
    })
    .await;
    match result {
        Ok(Ok((file, bytes))) => (
            [
                (header::CONTENT_TYPE, "application/zip".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{file}\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(e)) => ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response(),
        Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
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
    tokio::spawn(skill_index_loop(st.clone()));
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(st.stopped())
    .await?;
    Ok(())
}
