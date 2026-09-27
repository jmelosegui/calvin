use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use calvin::config::{self, Config};
use calvin::insights::{CostBy, Since};
use calvin::{db, doctor, ingest, report};
use indicatif::{ProgressBar, ProgressStyle};

/// Import progress on stderr. Hidden when stderr isn't a terminal, and for small
/// incremental imports that finish in a blink.
#[derive(Default)]
struct Bar(Option<ProgressBar>);

impl ingest::Progress for Bar {
    fn start(&mut self, total_bytes: u64) {
        if total_bytes < 5_000_000 {
            return;
        }
        let bar = ProgressBar::new(total_bytes).with_style(
            ProgressStyle::with_template(
                "Importing logs {bar:30.208/238} {binary_bytes}/{binary_total_bytes} · {eta} left",
            )
            .expect("valid template")
            .progress_chars("━╸─"),
        );
        self.0 = Some(bar);
    }
    fn advance(&mut self, bytes: u64) {
        if let Some(bar) = &self.0 {
            bar.inc(bytes);
        }
    }
    fn finish(&mut self) {
        if let Some(bar) = self.0.take() {
            bar.finish_and_clear();
        }
    }
}

/// Learn how you use AI coding agents, from the logs they already write.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Import new activity from your AI tools' logs (safe to re-run).
    Ingest,
    /// Print a report in the terminal. Imports new activity first.
    Report {
        #[command(subcommand)]
        report: Option<ReportKind>,
        /// Period to cover: 7d, 4w, 3m, all, or a date like 2026-09-01.
        #[arg(long, default_value = "30d", global = true)]
        since: String,
    },
    /// Show what was detected, where, and anything that needs attention.
    Doctor,
}

#[derive(Subcommand)]
enum ReportKind {
    /// Estimated cost at API list price.
    Cost {
        #[arg(long, value_enum, default_value_t = GroupBy::Day)]
        by: GroupBy,
    },
    /// Skills the model ran.
    Skills,
    /// Slash commands you typed.
    Commands,
    /// Prompts you keep typing.
    Prompts {
        /// Minimum number of times a prompt must appear.
        #[arg(long, default_value_t = 3)]
        min: i64,
        #[arg(long, default_value_t = 25)]
        limit: usize,
    },
    /// Rejected and denied tool calls, interruptions, tool errors.
    Friction,
    /// Prompt-cache hit rate per model.
    Cache,
}

#[derive(Clone, Copy, ValueEnum)]
enum GroupBy {
    Day,
    Project,
    Model,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load()?;
    let mut conn = db::open(&config::db_path()?)?;
    let prices = cfg.price_table();

    match cli.command {
        Some(Command::Ingest) => {
            let stats = ingest::ingest_claude_code(
                &mut conn,
                &cfg.claude_dir()?,
                &prices,
                &mut Bar::default(),
            )?;
            println!(
                "Imported {} lines from {} of {} Claude Code log files.",
                stats.lines, stats.files_read, stats.files_seen
            );
            if stats.bad_lines > 0 {
                println!(
                    "Skipped {} lines that were not valid JSON.",
                    stats.bad_lines
                );
            }
        }
        Some(Command::Report {
            report: kind,
            since,
        }) => {
            let since = Since::parse(&since)?;
            ingest::ingest_claude_code(
                &mut conn,
                &cfg.claude_dir()?,
                &prices,
                &mut Bar::default(),
            )?;
            match kind {
                None => report::summary(&conn, &since)?,
                Some(ReportKind::Cost { by }) => report::cost(
                    &conn,
                    &since,
                    match by {
                        GroupBy::Day => CostBy::Day,
                        GroupBy::Project => CostBy::Project,
                        GroupBy::Model => CostBy::Model,
                    },
                )?,
                Some(ReportKind::Skills) => report::skills(&conn, &since)?,
                Some(ReportKind::Commands) => report::commands(&conn, &since)?,
                Some(ReportKind::Prompts { min, limit }) => {
                    report::prompts(&conn, &since, min, limit)?
                }
                Some(ReportKind::Friction) => report::friction(&conn, &since)?,
                Some(ReportKind::Cache) => report::cache(&conn, &since)?,
            }
        }
        Some(Command::Doctor) => doctor::run(&cfg, &conn)?,
        None => {
            ingest::ingest_claude_code(
                &mut conn,
                &cfg.claude_dir()?,
                &prices,
                &mut Bar::default(),
            )?;
            report::summary(&conn, &Since::parse("30d")?)?;
        }
    }
    Ok(())
}
