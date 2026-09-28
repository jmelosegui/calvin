//! `calvin start` / `stop` / `status` / `open`: the background process lifecycle.
//!
//! `start` launches a detached copy of itself (`start --foreground`), which:
//! 1. takes a lock so only one calvin runs per data directory,
//! 2. binds 127.0.0.1 on port 1982 (or the next free one),
//! 3. writes a state file with its PID, port and a random shutdown token,
//! 4. serves the dashboard and keeps the database in sync until told to stop.
//!
//! `stop` asks it to shut down over HTTP with the token, which works the same on every OS,
//! and only kills the process if it doesn't exit in time.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::config::{self, Config};
use crate::server::{self, AppState};

/// Susan Calvin's birth year, and the year U.S. Robots was founded, in *I, Robot*.
pub const DEFAULT_PORT: u16 = 1982;
const PORT_ATTEMPTS: u16 = 20;
const START_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Serialize, Deserialize)]
pub struct RunState {
    pub pid: u32,
    pub port: u16,
    pub token: String,
    pub started_at: String,
    pub version: String,
}

impl RunState {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }
}

fn state_path() -> Result<PathBuf> {
    Ok(config::data_dir()?.join("calvin.state.json"))
}

fn lock_path() -> Result<PathBuf> {
    Ok(config::data_dir()?.join("calvin.lock"))
}

pub fn log_path() -> Result<PathBuf> {
    Ok(config::data_dir()?.join("calvin.log"))
}

fn read_state() -> Result<Option<RunState>> {
    let path = state_path()?;
    match fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str(&text).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// The running instance, if its state file exists and it answers. Stale state files
/// (process gone) are removed.
pub fn running() -> Result<Option<RunState>> {
    let Some(state) = read_state()? else {
        return Ok(None);
    };
    if healthy(state.port) {
        return Ok(Some(state));
    }
    let _ = fs::remove_file(state_path()?);
    Ok(None)
}

pub fn start(open_browser: bool) -> Result<()> {
    if let Some(state) = running()? {
        println!("calvin is already running at {}", state.url());
        if open_browser {
            open_url(&state.url());
        }
        return Ok(());
    }

    let data_dir = config::data_dir()?;
    fs::create_dir_all(&data_dir)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path()?)?;
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(["start", "--foreground"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    detach(&mut cmd);
    let mut child = cmd.spawn().context("starting the background process")?;

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(state) = running()? {
            println!("calvin is running at {}", state.url());
            println!("It keeps your data up to date in the background. Stop it with: calvin stop");
            if open_browser {
                open_url(&state.url());
            }
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "the background process exited ({status}). See {}",
                log_path()?.display()
            );
        }
        if Instant::now() > deadline {
            bail!(
                "calvin didn't start within {:?}. See {}",
                START_TIMEOUT,
                log_path()?.display()
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // Own process group, so closing the terminal doesn't take it down.
    cmd.process_group(0);
}

/// `calvin start --foreground`: run the server in this process until stopped.
pub fn run_foreground(cfg: &Config) -> Result<()> {
    let data_dir = config::data_dir()?;
    fs::create_dir_all(&data_dir)?;
    let lock = File::create(lock_path()?)?;
    if lock.try_lock().is_err() {
        bail!("calvin is already running for {}", data_dir.display());
    }

    let db_path = config::db_path()?;
    // Create or migrate the database before serving reads.
    crate::db::open(&db_path)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let (listener, port) = bind().await?;
        let token = random_token()?;
        let st = Arc::new(
            AppState::new(
                data_dir.clone(),
                crate::update::enabled(cfg.updates.check),
                db_path,
                cfg.claude_dir()?,
                cfg.skill_folders(),
                cfg.price_table(),
                token.clone(),
                port,
            )
            .with_advisor(cfg.advisor.clone()),
        );
        let state = RunState {
            pid: std::process::id(),
            port,
            token,
            started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            version: env!("CARGO_PKG_VERSION").to_string(),
        };
        write_state(&state)?;
        eprintln!(
            "[{}] calvin {} listening on {}",
            state.started_at,
            state.version,
            state.url()
        );

        let on_ctrl_c = st.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                on_ctrl_c.request_shutdown();
            }
        });
        let result = server::serve(listener, st).await;
        let _ = fs::remove_file(state_path()?);
        eprintln!(
            "[{}] calvin stopped",
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        );
        result
    })?;
    drop(lock);
    Ok(())
}

async fn bind() -> Result<(tokio::net::TcpListener, u16)> {
    let first = std::env::var("CALVIN_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    for port in first..first.saturating_add(PORT_ATTEMPTS) {
        if let Ok(listener) = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            return Ok((listener, port));
        }
    }
    bail!(
        "no free port between {first} and {}",
        first + PORT_ATTEMPTS - 1
    )
}

fn write_state(state: &RunState) -> Result<()> {
    let path = state_path()?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("no randomness available: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn stop() -> Result<()> {
    let Some(state) = read_state()? else {
        println!("calvin is not running.");
        return Ok(());
    };
    if !healthy(state.port) {
        let _ = fs::remove_file(state_path()?);
        println!("calvin is not running (cleaned up a stale state file).");
        return Ok(());
    }

    let asked =
        http(state.port, "POST", "/api/shutdown", Some(&state.token)).map(|(code, _)| code == 202);
    let deadline = Instant::now() + STOP_TIMEOUT;
    // Wait for the process itself to exit, not just the server: until it does, Windows
    // keeps the executable locked and an installer can't replace it.
    while Instant::now() < deadline {
        if !healthy(state.port) && !process_alive(state.pid) {
            let _ = fs::remove_file(state_path()?);
            println!("calvin stopped.");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    kill(state.pid)?;
    let _ = fs::remove_file(state_path()?);
    match asked {
        Ok(true) => println!(
            "calvin didn't stop in time, so it was killed (PID {}).",
            state.pid
        ),
        _ => println!(
            "calvin didn't accept the stop request, so it was killed (PID {}).",
            state.pid
        ),
    }
    Ok(())
}

fn process_alive(pid: u32) -> bool {
    let pid = pid.to_string();
    if cfg!(windows) {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
            .unwrap_or(false)
    } else {
        Command::new("kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

fn kill(pid: u32) -> Result<()> {
    let status = if cfg!(windows) {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(Stdio::null())
            .status()?
    } else {
        Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()?
    };
    if !status.success() {
        bail!("could not stop process {pid}");
    }
    Ok(())
}

pub fn status() -> Result<()> {
    let Some(state) = running()? else {
        println!("calvin is not running. Start it with: calvin start");
        return Ok(());
    };
    println!("calvin {} is running at {}", state.version, state.url());
    println!("  PID        {}", state.pid);
    println!("  started    {}", state.started_at);
    if let Ok((200, body)) = http(state.port, "GET", "/api/status", None)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&body)
    {
        if let Some(sync) = v.get("last_sync").filter(|s| !s.is_null()) {
            let at = sync["at"].as_str().unwrap_or("?");
            let lines = sync["lines"].as_u64().unwrap_or(0);
            println!("  last sync  {at} ({lines} new lines)");
            if let Some(err) = sync["error"].as_str() {
                println!("  sync error {err}");
            }
        }
        if let Some(bytes) = v["db_bytes"].as_u64() {
            println!(
                "  database   {:.1} MB  {}",
                bytes as f64 / 1e6,
                config::db_path()?.display()
            );
        }
    }
    println!("  log        {}", log_path()?.display());
    if let Some(u) = crate::update::available(&config::data_dir()?) {
        println!();
        println!(
            "A new version is available: calvin {} (you have {}).",
            u.latest, u.current
        );
        println!("  {}", u.url);
    }
    Ok(())
}

pub fn open() -> Result<()> {
    match running()? {
        Some(state) => {
            open_url(&state.url());
            println!("Opened {}", state.url());
        }
        None => println!("calvin is not running. Start it with: calvin start"),
    }
    Ok(())
}

fn open_url(url: &str) {
    if let Err(e) = open::that_detached(url) {
        eprintln!("Couldn't open a browser ({e}). Open {url} yourself.");
    }
}

fn healthy(port: u16) -> bool {
    matches!(http(port, "GET", "/api/health", None), Ok((200, _)))
}

/// Just enough HTTP/1.1 to talk to our own server, without pulling in an HTTP client.
fn http(port: u16, method: &str, path: &str, token: Option<&str>) -> Result<(u16, String)> {
    let addr = (Ipv4Addr::LOCALHOST, port).into();
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let token_header = token
        .map(|t| format!("x-calvin-token: {t}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{token_header}Content-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let code = response
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| anyhow!("bad response from calvin"))?;
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Ok((code, body))
}
