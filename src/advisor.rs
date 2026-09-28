//! Ask Claude for a plan: sends the opportunities report to Claude through the user's own
//! Claude Code (`claude -p`), streams its progress, and saves the result.
//!
//! This is opt-in and only runs when you press the button. The run is kept lean and
//! contained: no tools, no MCP servers, no skills, no hooks, calvin's own system prompt,
//! nothing saved to Claude Code's session history, and a spending cap.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::Serialize;
use serde_json::Value;

use crate::config::{AdvisorConfig, ClaudeCodeAdvisor, CommandAdvisor, find_program};
use crate::insights::{ModelRow, Summary};
use crate::opportunities::{Opportunity, Status};
use crate::skills::InstalledSkill;

const TIMEOUT: Duration = Duration::from_secs(10 * 60);

pub const SYSTEM_PROMPT: &str = "You are the advisor inside calvin, a local tool that studies how one developer uses \
Claude Code. You receive calvin's deterministic report about this developer: findings, numbers and evidence. \
Write a practical improvement plan in Markdown, specific to this person.\n\n\
Structure:\n\
1. **This week**: the 3 changes with the biggest effect, in order. For each: what to do, why (cite the numbers), and the expected effect.\n\
2. **Drafts**: ready-to-use text for those changes, such as a CLAUDE.md starter for a named project, a rewritten skill description, a settings.json snippet, or a new skill outline. Base drafts on the evidence given; mark anything you had to assume.\n\
3. **Patterns**: connections between findings (for example one project behind several problems).\n\
4. **Later**: other items worth doing, one line each.\n\n\
Rules: use only the data provided and never invent numbers; say when data is too thin to conclude; keep it concise; \
when the report gives an exact setting or command, use it verbatim and never make up settings keys or flags; \
Claude Code features you mention must be real (CLAUDE.md, /init, /memory, skills, subagents via /agents, hooks via /hooks, \
permissions, plan mode, /effort, /clear, MCP servers, settings.json).";

#[derive(Debug, Clone, Serialize, Default)]
pub struct Job {
    /// `running`, `done` or `error`.
    pub state: String,
    /// Which advisor is (or was) working.
    pub provider: String,
    pub since: String,
    /// What's happening now, in words.
    pub phase: String,
    /// Progress so far, oldest first.
    pub steps: Vec<String>,
    /// The plan as it's being written.
    pub text: String,
    pub model: Option<String>,
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<i64>,
    pub error: Option<String>,
    pub report_id: Option<i64>,
    pub elapsed_ms: u64,
    #[serde(skip)]
    started: Option<Instant>,
}

impl Job {
    fn step(&mut self, phase: &str) {
        if self.phase != phase {
            self.phase = phase.to_string();
            let secs = self.started.map_or(0, |s| s.elapsed().as_secs());
            self.steps.push(format!("{secs}s · {phase}"));
        }
    }

    pub fn snapshot(&self) -> Job {
        let mut j = self.clone();
        j.elapsed_ms = self.started.map_or(0, |s| s.elapsed().as_millis() as u64);
        j
    }
}

/// Which AI tool writes the plan. Each provider knows how to run its tool
/// non-interactively, safely, and how to read its output.
#[derive(Debug, Clone)]
pub enum Provider {
    /// Claude Code (`claude -p`): streams progress, reports cost, honours a spending cap.
    ClaudeCode(ClaudeCodeAdvisor),
    /// Any command that reads the prompt on stdin and writes Markdown to stdout.
    Command(CommandAdvisor),
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub id: &'static str,
    pub name: String,
    /// Found on this machine and ready to use.
    pub available: bool,
    /// Model, budget or command, in words.
    pub detail: String,
    pub selected: bool,
}

impl Provider {
    pub fn from_config(cfg: &AdvisorConfig) -> Result<Provider> {
        match cfg.provider.as_str() {
            "claude-code" => Ok(Provider::ClaudeCode(cfg.claude_code.clone())),
            "command" if !cfg.command.program.trim().is_empty() => {
                Ok(Provider::Command(cfg.command.clone()))
            }
            "command" => bail!("the custom advisor command isn't set up yet"),
            other => bail!("unknown advisor provider '{other}'"),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Provider::ClaudeCode(_) => "Claude Code".into(),
            Provider::Command(c) if !c.name.trim().is_empty() => c.name.trim().to_string(),
            Provider::Command(c) => c.program.clone(),
        }
    }
}

/// Every provider calvin supports, whether it's installed, and which one is chosen.
pub fn providers(cfg: &AdvisorConfig) -> Vec<ProviderInfo> {
    let cc = &cfg.claude_code;
    let cmd = &cfg.command;
    vec![
        ProviderInfo {
            id: "claude-code",
            name: "Claude Code".into(),
            available: find_program(&cc.program).is_some(),
            detail: format!("{} · up to ${:.2} per run", cc.model, cc.max_budget_usd),
            selected: cfg.provider == "claude-code",
        },
        ProviderInfo {
            id: "command",
            name: if cmd.name.trim().is_empty() {
                "Custom command".into()
            } else {
                cmd.name.clone()
            },
            available: !cmd.program.trim().is_empty() && find_program(&cmd.program).is_some(),
            detail: if cmd.program.trim().is_empty() {
                "any tool that reads a prompt on stdin and prints Markdown".into()
            } else {
                format!("{} {}", cmd.program, cmd.args.join(" "))
            },
            selected: cfg.provider == "command",
        },
    ]
}

/// The report as Claude will receive it: every finding with its evidence (including the
/// text of repeated prompts, rejected commands and skill descriptions), plus context.
pub fn build_prompt(
    since_label: &str,
    opportunities: &[Opportunity],
    skills: &[InstalledSkill],
    summary: &Summary,
    models: &[ModelRow],
) -> String {
    let mut s = String::new();
    s.push_str(&format!("# calvin report ({since_label})\n\n## Overview\n"));
    s.push_str(&format!(
        "- {} sessions, {} prompts, {} slash commands, {} model requests\n- Estimated cost at API list price: ${:.2}\n",
        summary.sessions, summary.prompts, summary.commands, summary.requests, summary.cost_usd
    ));
    if let Some(rate) = summary.cache_hit_rate {
        s.push_str(&format!("- Prompt cache hit rate: {:.0}%\n", rate * 100.0));
    }
    s.push_str("\n## Models\n");
    for m in models {
        let efforts: Vec<String> = m
            .efforts
            .iter()
            .map(|e| format!("{} {}", e.effort.as_deref().unwrap_or("n/a"), e.requests))
            .collect();
        s.push_str(&format!(
            "- {}: {} requests ({} by subagents), ${:.2}, effort: {}\n",
            m.model,
            m.requests,
            m.subagent_requests,
            m.cost_usd,
            efforts.join(", ")
        ));
    }
    let describe = |name: &str| {
        skills
            .iter()
            .find(|k| k.name == name)
            .map(|k| k.description.clone())
    };
    for (label, status) in [
        ("Worth doing", Status::Action),
        ("Worth a look", Status::Consider),
        ("Already in place", Status::Good),
    ] {
        let items: Vec<&Opportunity> = opportunities
            .iter()
            .filter(|o| o.status == status)
            .collect();
        if items.is_empty() {
            continue;
        }
        s.push_str(&format!("\n## {label}\n"));
        for o in items {
            s.push_str(&format!("\n### {} [{}]\n{}\n", o.title, o.area, o.finding));
            if let Some(v) = o.saving_usd {
                s.push_str(&format!("Estimated saving: ${v:.0}\n"));
            }
            if status != Status::Good {
                s.push_str(&format!("Suggested fix: {}\n", o.fix));
                if let Some(snippet) = &o.snippet {
                    s.push_str(&format!(
                        "Exact setting or command (use verbatim): `{snippet}`\n"
                    ));
                }
            }
            for e in &o.evidence {
                s.push_str(&format!("- {}", e.label));
                if let Some(d) = &e.detail {
                    s.push_str(&format!(" ({d})"));
                }
                if o.area == "Skills"
                    && let Some(desc) = describe(&e.label).filter(|d| !d.is_empty())
                {
                    let short: String = desc.chars().take(400).collect();
                    s.push_str(&format!("\n  Current description: {short}"));
                }
                s.push('\n');
            }
        }
    }
    s
}

/// Start a run in the background. Progress and the result land in `job`.
pub fn start(
    job: Arc<Mutex<Job>>,
    provider: Provider,
    prompt: String,
    since: String,
    db_path: PathBuf,
) -> Result<()> {
    {
        let mut j = job.lock().unwrap();
        if j.state == "running" {
            bail!("{} is already working on a plan", j.provider);
        }
        *j = Job {
            state: "running".into(),
            since: since.clone(),
            provider: provider.name(),
            started: Some(Instant::now()),
            ..Default::default()
        };
        j.step(&format!("Starting {}", provider.name()));
    }
    std::thread::spawn(move || {
        let started = Instant::now();
        let outcome = match &provider {
            Provider::ClaudeCode(settings) => run_claude_code(&job, settings, &prompt),
            Provider::Command(settings) => run_command(&job, settings, &prompt),
        };
        let mut j = job.lock().unwrap();
        match outcome {
            Ok(()) if j.error.is_none() => j.state = "done".into(),
            Ok(()) => j.state = "error".into(),
            Err(e) => {
                j.state = "error".into();
                j.error = Some(format!("{e:#}"));
            }
        }
        let phase = if j.state == "done" {
            "Done"
        } else {
            "Stopped with an error"
        };
        j.step(phase);
        let duration = j
            .duration_ms
            .unwrap_or(started.elapsed().as_millis() as i64);
        let saved = crate::db::open(&db_path).and_then(|conn| {
            conn.execute(
                "INSERT INTO ai_reports (created_at, since, model, prompt, report, status, error, cost_usd, duration_ms)
                 VALUES (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![since, j.model, prompt, j.text, j.state, j.error, j.cost_usd, duration],
            )?;
            Ok(conn.last_insert_rowid())
        });
        match saved {
            Ok(id) => j.report_id = Some(id),
            Err(e) => eprintln!("couldn't save the advisor report: {e:#}"),
        }
    });
    Ok(())
}

/// An empty working folder, so no project's instructions (CLAUDE.md, AGENTS.md, …) apply.
fn workdir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("calvin-advisor");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let _ = cmd;
}

/// Any command: calvin's instructions and the report go in on stdin, the plan comes back
/// on stdout. No progress detail or cost is available, only the text as it arrives.
fn run_command(job: &Arc<Mutex<Job>>, settings: &CommandAdvisor, prompt: &str) -> Result<()> {
    let mut cmd = Command::new(&settings.program);
    cmd.current_dir(workdir()?)
        .args(&settings.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_window(&mut cmd);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("couldn't start `{}`", settings.program))?;
    let input = format!("{SYSTEM_PROMPT}\n\n---\n\n{prompt}");
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(input.as_bytes())?;
    job.lock().unwrap().step("Sending the report");
    let stderr = child.stderr.take();
    let stderr_text = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = stderr {
            let _ = std::io::Read::read_to_string(&mut e, &mut s);
        }
        s
    });
    let deadline = Instant::now() + TIMEOUT;
    for line in BufReader::new(child.stdout.take().context("no stdout")?).lines() {
        let line = line?;
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!(
                "the advisor took longer than {} minutes",
                TIMEOUT.as_secs() / 60
            );
        }
        let mut j = job.lock().unwrap();
        j.step("Writing the plan");
        j.text.push_str(&line);
        j.text.push('\n');
    }
    let status = child.wait()?;
    let err = stderr_text.join().unwrap_or_default();
    let mut j = job.lock().unwrap();
    if !status.success() {
        j.error = Some(if err.trim().is_empty() {
            format!("{} exited with {status}", settings.program)
        } else {
            err.trim().to_string()
        });
    } else if j.text.trim().is_empty() {
        j.error = Some("the advisor returned nothing".into());
    }
    Ok(())
}

/// Claude Code, kept lean and contained: no tools, MCP servers, skills or hooks, calvin's
/// own system prompt, nothing saved to session history, and a spending cap.
fn run_claude_code(
    job: &Arc<Mutex<Job>>,
    settings: &ClaudeCodeAdvisor,
    prompt: &str,
) -> Result<()> {
    let budget = format!("{:.2}", settings.max_budget_usd);
    let mut cmd = Command::new(&settings.program);
    cmd.current_dir(workdir()?)
        .args([
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
        ])
        .args(["--tools", ""])
        .args([
            "--no-session-persistence",
            "--strict-mcp-config",
            "--disable-slash-commands",
        ])
        .args(["--settings", r#"{"disableAllHooks":true}"#])
        .args(["--model", &settings.model, "--max-budget-usd", &budget])
        .args(["--system-prompt", SYSTEM_PROMPT])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_window(&mut cmd);
    let mut child = cmd.spawn().with_context(|| {
        format!(
            "couldn't start `{}`. Is Claude Code installed and on your PATH?",
            settings.program
        )
    })?;
    child
        .stdin
        .take()
        .context("no stdin")?
        .write_all(prompt.as_bytes())?;
    let stderr = child.stderr.take();
    let stderr_text = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = stderr {
            let _ = std::io::Read::read_to_string(&mut e, &mut s);
        }
        s
    });

    let deadline = Instant::now() + TIMEOUT;
    let stdout = BufReader::new(child.stdout.take().context("no stdout")?);
    for line in stdout.lines() {
        let line = line?;
        if Instant::now() > deadline {
            let _ = child.kill();
            bail!(
                "Claude Code took longer than {} minutes",
                TIMEOUT.as_secs() / 60
            );
        }
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        apply(&mut job.lock().unwrap(), &event);
    }
    let status = child.wait()?;
    let err = stderr_text.join().unwrap_or_default();
    let mut j = job.lock().unwrap();
    if !status.success() && j.error.is_none() && j.text.is_empty() {
        j.error = Some(if err.trim().is_empty() {
            format!("Claude Code exited with {status}")
        } else {
            err.trim().to_string()
        });
    }
    Ok(())
}

/// Update the job from one stream-json event.
fn apply(j: &mut Job, e: &Value) {
    let kind = e.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "system" if e.get("subtype").and_then(Value::as_str) == Some("init") => {
            j.model = e.get("model").and_then(Value::as_str).map(str::to_string);
            let model = j.model.clone().unwrap_or_default();
            j.step(&format!("Claude Code is ready ({model})"));
            j.step("Sending the report");
        }
        "stream_event" => {
            let ev = e.get("event").cloned().unwrap_or(Value::Null);
            match ev.get("type").and_then(Value::as_str) {
                Some("message_start") => j.step("Reading your report"),
                Some("content_block_start")
                    if ev.pointer("/content_block/type").and_then(Value::as_str)
                        == Some("thinking") =>
                {
                    j.step("Thinking it through");
                }
                Some("content_block_delta") => {
                    match ev.pointer("/delta/type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            j.step("Writing the plan");
                            if let Some(t) = ev.pointer("/delta/text").and_then(Value::as_str) {
                                j.text.push_str(t);
                            }
                        }
                        Some("thinking_delta") => j.step("Thinking it through"),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        "result" => {
            j.cost_usd = e.get("total_cost_usd").and_then(Value::as_f64);
            j.duration_ms = e.get("duration_ms").and_then(Value::as_i64);
            if let Some(text) = e
                .get("result")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
            {
                j.text = text.to_string();
            }
            if e.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                let errors: Vec<String> = e
                    .get("errors")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let subtype = e.get("subtype").and_then(Value::as_str).unwrap_or("error");
                j.error = Some(if errors.is_empty() {
                    subtype.to_string()
                } else {
                    errors.join("; ")
                });
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_stream_events() {
        let mut j = Job {
            state: "running".into(),
            started: Some(Instant::now()),
            ..Default::default()
        };
        let events = [
            r##"{"type":"system","subtype":"init","model":"claude-sonnet-5"}"##,
            r##"{"type":"stream_event","event":{"type":"message_start"}}"##,
            r##"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"# Plan"}}}"##,
            r##"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"\n1. Do it"}}}"##,
            r##"{"type":"result","subtype":"success","is_error":false,"result":"# Plan\n1. Do it","total_cost_usd":0.12,"duration_ms":9000}"##,
        ];
        for e in events {
            apply(&mut j, &serde_json::from_str(e).unwrap());
        }
        assert_eq!(j.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(j.text, "# Plan\n1. Do it");
        assert_eq!(j.cost_usd, Some(0.12));
        assert!(j.error.is_none());
        assert!(j.steps.iter().any(|s| s.contains("Writing the plan")));
    }

    #[test]
    fn provider_comes_from_config() {
        let mut cfg = AdvisorConfig::default();
        assert!(matches!(
            Provider::from_config(&cfg).unwrap(),
            Provider::ClaudeCode(_)
        ));
        cfg.provider = "command".into();
        assert!(
            Provider::from_config(&cfg).is_err(),
            "a custom command must be set up first"
        );
        cfg.command.program = "codex".into();
        cfg.command.name = "Codex".into();
        assert_eq!(Provider::from_config(&cfg).unwrap().name(), "Codex");
        cfg.provider = "nope".into();
        assert!(Provider::from_config(&cfg).is_err());
        let listed = providers(&AdvisorConfig::default());
        assert_eq!(listed.iter().filter(|p| p.selected).count(), 1);
    }

    #[test]
    fn budget_errors_are_reported() {
        let mut j = Job::default();
        let e = r##"{"type":"result","subtype":"error_max_budget_usd","is_error":true,"errors":["Reached maximum budget ($0.05)"],"total_cost_usd":0.07}"##;
        apply(&mut j, &serde_json::from_str(e).unwrap());
        assert_eq!(j.error.as_deref(), Some("Reached maximum budget ($0.05)"));
    }
}
