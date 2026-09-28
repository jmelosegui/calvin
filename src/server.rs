//! The local web server: the dashboard page plus a JSON API over [`insights`].
//!
//! Every API handler is a thin wrapper: parse query parameters, open a read connection,
//! call the matching `insights` function on a blocking thread, return JSON. All logic
//! lives in `insights`, shared with the terminal reports.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
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
const OPPORTUNITIES_HTML: &str = include_str!("../web/opportunities.html");
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
    /// Which AI tool writes plans; can change from the Opportunities page.
    advisor: RwLock<crate::config::AdvisorConfig>,
    advisor_job: Arc<Mutex<crate::advisor::Job>>,
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
            advisor: RwLock::new(crate::config::AdvisorConfig::default()),
            advisor_job: Arc::new(Mutex::new(crate::advisor::Job::default())),
            prices,
            token,
            port,
            shutdown: watch::channel(false).0,
            started: Instant::now(),
            last_sync: Mutex::new(None),
        }
    }

    pub fn with_advisor(self, advisor: crate::config::AdvisorConfig) -> Self {
        *self.advisor.write().unwrap() = advisor;
        self
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
        .route("/opportunities", get(|| async { html(OPPORTUNITIES_HTML) }))
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
        .route("/api/models", get(models))
        .route("/api/opportunities", get(opportunities))
        .route("/api/opportunities/trends", get(opportunity_trends))
        .route("/api/advisor/providers", get(advisor_providers))
        .route("/api/advisor/provider", post(advisor_set_provider))
        .route("/api/advisor/preview", get(advisor_preview))
        .route("/api/advisor/run", post(advisor_run))
        .route("/api/advisor/status", get(advisor_status))
        .route("/api/advisor/reports", get(advisor_reports))
        .route("/api/advisor/report", get(advisor_report))
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
/// Lets `calvin stop` interrupt a long import between files instead of waiting for it.
struct StopOnShutdown<'a>(&'a AppState);

impl ingest::Progress for StopOnShutdown<'_> {
    fn should_stop(&self) -> bool {
        *self.0.shutdown.borrow()
    }
}

pub async fn sync_loop(st: Arc<AppState>) {
    loop {
        let s = st.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<usize> {
            let mut conn = db::open(&s.db_path)?;
            Ok(ingest::ingest_claude_code(
                &mut conn,
                &s.claude_dir,
                &s.prices,
                &mut StopOnShutdown(&s),
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

async fn models(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<Vec<insights::ModelRow>> {
    let since = p.since()?;
    read(&st, move |c| insights::models(c, &since)).await
}

async fn opportunities(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<Vec<crate::opportunities::Opportunity>> {
    let since = p.since()?;
    let s = st.clone();
    read(&st, move |c| compute_opportunities(&s, c, &since)).await
}

fn compute_opportunities(
    st: &AppState,
    c: &Connection,
    since: &Since,
) -> Result<Vec<crate::opportunities::Opportunity>> {
    let skills = installed_skills(st, c)?;
    let ctx = crate::opportunities::Context {
        claude_dir: &st.claude_dir,
        prices: &st.prices,
        skills: &skills,
    };
    crate::opportunities::run(c, since, &ctx)
}

/// Once a day, record each opportunity's metric (last 30 days) for the trend lines.
pub async fn snapshot_loop(st: Arc<AppState>) {
    loop {
        let s = st.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = db::open(&s.db_path)?;
            let today = chrono::Local::now().format("%Y-%m-%d").to_string();
            let done: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM opportunity_snapshots WHERE day = ?1)",
                [&today],
                |r| r.get(0),
            )?;
            if done {
                return Ok(());
            }
            for o in compute_opportunities(&s, &conn, &Since::parse("30d")?)? {
                let status = serde_json::to_value(o.status)?;
                conn.execute(
                    "INSERT OR REPLACE INTO opportunity_snapshots (day, id, status, metric, saving_usd)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        today,
                        o.id,
                        status.as_str().unwrap_or_default(),
                        o.metric.map(|m| m.value),
                        o.saving_usd
                    ],
                )?;
            }
            Ok(())
        })
        .await;
        if let Ok(Err(e)) = result {
            eprintln!("opportunity snapshot failed: {e:#}");
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(60 * 60)) => {}
            _ = st.stopped() => return,
        }
    }
}

#[derive(Serialize)]
struct TrendPoint {
    day: String,
    status: String,
    metric: Option<f64>,
}

type Trends = std::collections::BTreeMap<String, Vec<TrendPoint>>;

async fn opportunity_trends(State(st): State<Arc<AppState>>) -> ApiResult<Trends> {
    read(&st, move |c| {
        let mut out = Trends::new();
        let mut stmt = c.prepare(
            "SELECT id, day, status, metric FROM opportunity_snapshots
             WHERE day >= date('now', '-180 days') ORDER BY id, day",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            out.entry(r.get(0)?).or_default().push(TrendPoint {
                day: r.get(1)?,
                status: r.get(2)?,
                metric: r.get(3)?,
            });
        }
        Ok(out)
    })
    .await
}

fn advisor_prompt(st: &AppState, c: &Connection, since_text: &str) -> Result<String> {
    let since = Since::parse(since_text)?;
    let opportunities = compute_opportunities(st, c, &since)?;
    let skills = installed_skills(st, c)?;
    let summary = insights::summary(c, &since)?;
    let models = insights::models(c, &since)?;
    let mut inventory =
        crate::inventory::collect(c, &since, &st.claude_dir)?.to_markdown("Claude Code");
    let advisor = st.advisor.read().unwrap().clone();
    let docs = match advisor.provider.as_str() {
        "claude-code" if advisor.claude_code.research => advisor.claude_code.docs_index,
        "command" => advisor.command.docs_index,
        _ => String::new(),
    };
    if !docs.trim().is_empty() {
        inventory.push_str(&format!(
            "- Official documentation index: {docs} (fetch it for the current feature list)\n"
        ));
    }
    Ok(crate::advisor::build_prompt(
        &format!("last {since_text}"),
        &opportunities,
        &skills,
        &summary,
        &models,
        &inventory,
    ))
}

async fn advisor_providers(
    State(st): State<Arc<AppState>>,
) -> Json<Vec<crate::advisor::ProviderInfo>> {
    Json(crate::advisor::providers(&st.advisor.read().unwrap()))
}

#[derive(Deserialize)]
struct ProviderChoice {
    provider: String,
    name: Option<String>,
    program: Option<String>,
    args: Option<Vec<String>>,
}

/// Choose the advisor (and, for a custom command, what to run). Saved to config.toml.
async fn advisor_set_provider(
    State(st): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(choice): Json<ProviderChoice>,
) -> Response {
    if headers.get(ACTION_HEADER).is_none() {
        return ApiError(
            StatusCode::FORBIDDEN,
            format!("missing {ACTION_HEADER} header"),
        )
        .into_response();
    }
    let mut cfg = st.advisor.read().unwrap().clone();
    cfg.provider = choice.provider;
    if let Some(name) = choice.name {
        cfg.command.name = name;
    }
    if let Some(program) = choice.program {
        cfg.command.program = program;
    }
    if let Some(args) = choice.args {
        cfg.command.args = args;
    }
    if let Err(e) = crate::advisor::Provider::from_config(&cfg) {
        return ApiError(StatusCode::BAD_REQUEST, format!("{e:#}")).into_response();
    }
    if let Err(e) = crate::config::save_advisor(&cfg) {
        return ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response();
    }
    *st.advisor.write().unwrap() = cfg;
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Serialize)]
struct AdvisorPreview {
    provider: String,
    system: &'static str,
    prompt: String,
}

/// Exactly what would be sent, so you can check before pressing the button.
async fn advisor_preview(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<AdvisorPreview> {
    let since = p.since.clone().unwrap_or_else(|| "30d".into());
    let s = st.clone();
    read(&st, move |c| {
        let provider = crate::advisor::Provider::from_config(&s.advisor.read().unwrap())?;
        Ok(AdvisorPreview {
            provider: provider.name(),
            system: crate::advisor::SYSTEM_PROMPT,
            prompt: advisor_prompt(&s, c, &since)?,
        })
    })
    .await
}

async fn advisor_run(
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
    let since = p.since.clone().unwrap_or_else(|| "30d".into());
    let s = st.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<()> {
        let provider = crate::advisor::Provider::from_config(&s.advisor.read().unwrap())?;
        let conn = db::open_read(&s.db_path)?;
        let prompt = advisor_prompt(&s, &conn, &since)?;
        crate::advisor::start(
            s.advisor_job.clone(),
            provider,
            prompt,
            since,
            s.db_path.clone(),
        )
    })
    .await;
    match result {
        Ok(Ok(())) => StatusCode::ACCEPTED.into_response(),
        Ok(Err(e)) => ApiError(StatusCode::CONFLICT, format!("{e:#}")).into_response(),
        Err(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn advisor_status(State(st): State<Arc<AppState>>) -> Json<crate::advisor::Job> {
    Json(st.advisor_job.lock().unwrap().snapshot())
}

#[derive(Serialize)]
struct ReportRow {
    id: i64,
    created_at: String,
    since: String,
    model: Option<String>,
    status: String,
    error: Option<String>,
    cost_usd: Option<f64>,
    duration_ms: Option<i64>,
    report: Option<String>,
}

fn report_row(r: &rusqlite::Row) -> rusqlite::Result<ReportRow> {
    Ok(ReportRow {
        id: r.get(0)?,
        created_at: r.get(1)?,
        since: r.get(2)?,
        model: r.get(3)?,
        status: r.get(4)?,
        error: r.get(5)?,
        cost_usd: r.get(6)?,
        duration_ms: r.get(7)?,
        report: r.get(8)?,
    })
}

async fn advisor_reports(State(st): State<Arc<AppState>>) -> ApiResult<Vec<ReportRow>> {
    read(&st, |c| {
        let rows = c
            .prepare(
                "SELECT id, created_at, since, model, status, error, cost_usd, duration_ms, substr(report, 1, 240)
                 FROM ai_reports ORDER BY id DESC LIMIT 50",
            )?
            .query_map([], report_row)?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    })
    .await
}

async fn advisor_report(
    State(st): State<Arc<AppState>>,
    Query(p): Query<Params>,
) -> ApiResult<ReportRow> {
    let Some(id) = p.id.clone().and_then(|i| i.parse::<i64>().ok()) else {
        return Err(ApiError(StatusCode::BAD_REQUEST, "missing ?id=".into()));
    };
    read(&st, move |c| {
        Ok(c.query_row(
            "SELECT id, created_at, since, model, status, error, cost_usd, duration_ms, report
             FROM ai_reports WHERE id = ?1",
            [id],
            report_row,
        )?)
    })
    .await
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
    tokio::spawn(snapshot_loop(st.clone()));
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(st.stopped())
    .await?;
    Ok(())
}
