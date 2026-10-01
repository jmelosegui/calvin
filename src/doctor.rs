//! `calvin doctor`: what was detected, where, and anything that needs attention.

use anyhow::Result;
use rusqlite::Connection;

use crate::config::{self, Config};

pub fn run(cfg: &Config, conn: &Connection) -> Result<()> {
    let db = config::db_path()?;
    let claude = cfg.claude_dir()?;
    let copilot = cfg.copilot_dir()?;
    println!("Database      {}", db.display());
    if let Ok(meta) = std::fs::metadata(&db) {
        println!("              {:.1} MB", meta.len() as f64 / 1e6);
    }
    println!("Config file   {}", config::config_file()?.display());
    let data_dir = config::data_dir()?;
    if crate::update::enabled(cfg.updates.check) {
        match crate::update::read_cache(&data_dir) {
            Some(c) => {
                let latest = c.latest.as_deref().unwrap_or("unknown");
                let when = c.checked_at.format("%Y-%m-%d %H:%M UTC");
                println!(
                    "Updates       calvin {} · latest release {latest} (checked {when})",
                    crate::update::current_version()
                );
                if let Some(e) = c.error {
                    println!("              last check failed: {e}");
                }
            }
            None => {
                println!("Updates       checked daily while calvin is running (not checked yet)")
            }
        }
        if let Some(u) = crate::update::available(&data_dir) {
            println!(
                "              → calvin {} is available: {}",
                u.latest, u.url
            );
        }
    } else {
        println!("Updates       checks are off");
    }
    println!();

    let projects = claude.join("projects");
    if projects.is_dir() {
        let (files, lines, requests): (i64, i64, i64) = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM files WHERE harness = 'claude-code'),
                    (SELECT COALESCE(SUM(lines), 0) FROM raw_chunks),
                    (SELECT COUNT(*) FROM requests)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        println!("Claude Code   {}", projects.display());
        println!(
            "              {files} log files imported, {lines} lines, {requests} model requests"
        );
        match cleanup_period_days(&claude) {
            Some(days) => println!("              keeps logs for {days} days (cleanupPeriodDays)"),
            None => {
                println!(
                    "              keeps logs for 30 days (the default). Older sessions are deleted by"
                );
                println!(
                    "              Claude Code; calvin keeps its own copy, but only of what it has seen."
                );
                println!(
                    "              Run calvin at least once a month, or raise cleanupPeriodDays in"
                );
                println!("              {}", claude.join("settings.json").display());
            }
        }
    } else {
        println!("Claude Code   not found at {}", projects.display());
    }

    let copilot_store = copilot.join("session-store.db");
    let copilot_events = copilot.join("session-state");
    if copilot_store.is_file() || copilot_events.is_dir() {
        let (sessions, files, requests): (i64, i64, i64) = conn.query_row(
            "SELECT
                 (SELECT COUNT(*) FROM sessions WHERE harness = 'copilot-cli'),
                 (SELECT COUNT(*) FROM files WHERE harness = 'copilot-cli'),
                 (SELECT COUNT(*) FROM requests r JOIN sessions s ON s.id = r.session_id
                  WHERE s.harness = 'copilot-cli')",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        println!();
        println!("Copilot CLI   {}", copilot.display());
        println!("              {sessions} sessions, {files} event logs, {requests} usage records");
    } else {
        println!();
        println!("Copilot CLI   not found at {}", copilot.display());
    }

    let unpriced: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT r.model FROM requests r JOIN sessions s ON s.id = r.session_id
             WHERE r.cost_usd IS NULL AND r.model IS NOT NULL AND s.harness = 'claude-code'",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if !unpriced.is_empty() {
        println!();
        println!("No price known for: {}", unpriced.join(", "));
        println!(
            "Add them to config.toml under [prices.models.\"<id>\"] with input/output USD per million tokens."
        );
    }
    Ok(())
}

fn cleanup_period_days(claude_dir: &std::path::Path) -> Option<i64> {
    let text = std::fs::read_to_string(claude_dir.join("settings.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("cleanupPeriodDays")?.as_i64()
}
