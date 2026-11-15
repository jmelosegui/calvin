//! Failure loops: turns where the agent repeated the same failing call instead of
//! changing course.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;

use super::{Evidence, Metric, Opportunity, Status, money, pct, skip};
use crate::insights::Since;

/// Failed calls in a row, with the same error, that make a loop.
const LOOP_LENGTH: usize = 3;

/// One harness's wording and identity for the finding.
struct Harness {
    name: &'static str,
    label: &'static str,
    id: &'static str,
}

const HARNESSES: [Harness; 3] = [
    Harness {
        name: "claude-code",
        label: "Claude Code",
        id: "failure-loops",
    },
    Harness {
        name: "copilot-cli",
        label: "Copilot",
        id: "copilot-failure-loops",
    },
    Harness {
        name: "cursor",
        label: "Cursor",
        id: "cursor-failure-loops",
    },
];

/// One failure loop: the same call failing the same way, again and again, in one turn.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Loop {
    pub session_id: String,
    /// The tool, plus the first words of a shell command.
    pub call: String,
    /// The first useful line of the error, as the agent saw it (redacted).
    pub error: String,
    pub failures: usize,
    pub first_ts: String,
    pub last_ts: String,
}

/// A tool call, in the order it ran.
#[derive(Debug, Clone)]
pub(crate) struct Call {
    pub session_id: String,
    /// Calls with the same key are in the same turn.
    pub turn: String,
    pub ts: String,
    /// Model requests in the session up to this call. Calls with the same number came from
    /// one response, so they are one attempt, not a retry.
    pub request: usize,
    pub tool: String,
    pub input_json: Option<String>,
    pub failed: bool,
    pub error: Option<String>,
}

/// The failure-loop findings, one per harness that has tool calls in the period.
pub(super) fn failure_loops(conn: &Connection, since: &Since) -> Result<Vec<Opportunity>> {
    HARNESSES
        .iter()
        .map(|h| harness_loops(conn, since, h))
        .collect()
}

fn harness_loops(conn: &Connection, since: &Since, h: &Harness) -> Result<Opportunity> {
    let (turns, calls) = load_calls(conn, since, h.name)?;
    if turns == 0 {
        return Ok(skip());
    }
    let loops = find_loops(&calls);
    let mut looping_turns: Vec<(&str, &str)> = Vec::new();
    for l in &loops {
        let turn = calls
            .iter()
            .find(|c| c.session_id == l.session_id && c.ts == l.first_ts)
            .map(|c| c.turn.as_str())
            .unwrap_or_default();
        if !looping_turns.contains(&(l.session_id.as_str(), turn)) {
            looping_turns.push((l.session_id.as_str(), turn));
        }
    }
    let share = pct(looping_turns.len() as i64, turns);
    let metric = Metric::lower(share as f64, "% of turns with a failure loop");
    if loops.is_empty() {
        return Ok(Opportunity {
            id: h.id,
            area: "Workflow",
            status: Status::Good,
            title: format!("{} turns don't get stuck repeating a failing call", h.label),
            finding: format!(
                "No turn repeated the same failing call {LOOP_LENGTH} or more times in {turns} turns with tool calls."
            ),
            why: why(),
            fix: fix(),
            snippet: None,
            saving_usd: None,
            evidence: Vec::new(),
            metric,
        });
    }

    let titles = session_titles(conn, &loops)?;
    let mut costed: Vec<(Loop, Spend)> = Vec::new();
    for l in loops {
        let spend = loop_spend(conn, &l)?;
        costed.push((l, spend));
    }
    let total = costed.iter().fold(Spend::default(), |mut acc, (_, s)| {
        acc.add(s);
        acc
    });
    let failures: usize = costed.iter().map(|(l, _)| l.failures).sum();
    let share = if share == 0 {
        "under 1".to_string()
    } else {
        share.to_string()
    };
    costed.sort_by(|a, b| {
        b.1.weight()
            .total_cmp(&a.1.weight())
            .then_with(|| b.0.failures.cmp(&a.0.failures))
            .then_with(|| a.0.first_ts.cmp(&b.0.first_ts))
    });
    let n = costed.len();
    Ok(Opportunity {
        id: h.id,
        area: "Workflow",
        status: Status::Consider,
        title: format!("Break {} failure loops early", h.label),
        finding: format!(
            "{n} time{} a turn repeated the same failing call {LOOP_LENGTH} or more times ({failures} failed attempts, {}% of turns with tool calls). The repeats took {}.",
            if n == 1 { "" } else { "s" },
            share,
            total.describe(),
        ),
        why: why(),
        fix: fix(),
        snippet: None,
        saving_usd: None,
        evidence: costed
            .iter()
            .take(10)
            .map(|(l, spend)| Evidence {
                label: format!("{} failed {}× in a row", l.call, l.failures),
                detail: Some(format!(
                    "{} · {} · {}",
                    titles
                        .get(&l.session_id)
                        .cloned()
                        .unwrap_or_else(|| "(untitled session)".into()),
                    l.error,
                    spend.describe()
                )),
                link: Some(format!("/sessions#{}", l.session_id)),
            })
            .collect(),
        metric,
    })
}

fn why() -> String {
    "A call that failed the same way twice rarely works the third time. Each repeat costs a request that re-reads the whole conversation, and the cause, such as the wrong shell, a missing tool or a file that must be read first, is still there next session.".into()
}

fn fix() -> String {
    "Stop the turn when the same error shows up twice and tell the agent what is wrong. If the cause is the environment, add one line about it to the project's instruction file (AGENTS.md) so the next session starts knowing it. If it is a permission block, add a permission rule.".into()
}

/// Tool calls for sessions with at least [`LOOP_LENGTH`] failures in the period, in the
/// order they ran, plus the number of turns with any tool call (for the share).
fn load_calls(conn: &Connection, since: &Since, harness: &str) -> Result<(i64, Vec<Call>)> {
    let mut prompts: HashMap<String, Vec<String>> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT p.session_id, p.ts FROM main.prompts p JOIN main.sessions s ON s.id = p.session_id
         WHERE s.harness = ?1 AND p.is_sidechain = 0 ORDER BY p.ts",
    )?;
    for row in stmt.query_map([harness], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (session, ts) = row?;
        prompts.entry(session).or_default().push(ts);
    }
    let turn_of = |session: &str, turn_index: Option<i64>, ts: &str| -> String {
        if let Some(i) = turn_index {
            return format!("t{i}");
        }
        let n = prompts
            .get(session)
            .map(|p| p.partition_point(|t| t.as_str() <= ts))
            .unwrap_or(0);
        format!("p{n}")
    };

    let mut turns = 0;
    let mut stmt = conn.prepare(
        "SELECT t.session_id, t.ts, t.turn_index FROM main.tool_calls t
         JOIN main.sessions s ON s.id = t.session_id WHERE s.harness = ?1 AND t.ts >= ?2",
    )?;
    let mut seen: HashMap<String, Vec<String>> = HashMap::new();
    for row in stmt.query_map([harness, &since.0], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
        ))
    })? {
        let (session, ts, turn_index) = row?;
        let turn = turn_of(&session, turn_index, &ts);
        let list = seen.entry(session).or_default();
        if !list.contains(&turn) {
            list.push(turn);
            turns += 1;
        }
    }

    let failing =
        "SELECT f.session_id FROM main.tool_calls f JOIN main.sessions s ON s.id = f.session_id
         WHERE s.harness = ?1 AND f.outcome = 'error' AND f.ts >= ?2
         GROUP BY f.session_id HAVING COUNT(*) >= ?3";
    let params = rusqlite::params![harness, &since.0, LOOP_LENGTH as i64];
    let mut requests: HashMap<String, Vec<String>> = HashMap::new();
    let mut stmt = conn.prepare(&format!(
        "SELECT session_id, ts FROM main.requests WHERE session_id IN ({failing}) ORDER BY ts"
    ))?;
    for row in stmt.query_map(params, |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (session, ts) = row?;
        requests.entry(session).or_default().push(ts);
    }

    let mut stmt = conn.prepare(&format!(
        "SELECT t.session_id, t.ts, t.turn_index, t.tool, t.input_json, t.outcome, t.result_detail
         FROM main.tool_calls t
         WHERE t.ts >= ?2 AND t.session_id IN ({failing})
         ORDER BY t.session_id, t.ts, t.id"
    ))?;
    let mut calls = Vec::new();
    for row in stmt.query_map(params, |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<i64>>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, Option<String>>(6)?,
        ))
    })? {
        let (session_id, ts, turn_index, tool, input_json, outcome, error) = row?;
        let turn = turn_of(&session_id, turn_index, &ts);
        let request = requests
            .get(&session_id)
            .map(|r| r.partition_point(|t| t.as_str() <= ts.as_str()))
            .unwrap_or(0);
        calls.push(Call {
            session_id,
            turn,
            ts,
            request,
            tool,
            input_json,
            failed: outcome.as_deref() == Some("error"),
            error,
        });
    }
    Ok((turns, calls))
}

/// Finds loops in calls ordered by session and time. A success of the same call ends a
/// streak, and so does a different error: a changing error means progress, as in a
/// red-green test cycle.
pub(crate) fn find_loops(calls: &[Call]) -> Vec<Loop> {
    let mut out = Vec::new();
    // call key → (error signature, failures in the streak)
    let mut streaks: BTreeMap<String, (String, Vec<&Call>)> = BTreeMap::new();
    let mut current: Option<(&str, &str)> = None;
    let close = |streaks: &mut BTreeMap<String, (String, Vec<&Call>)>, out: &mut Vec<Loop>| {
        for (call, (_, failed)) in std::mem::take(streaks) {
            push_loop(out, call, &failed);
        }
    };
    for c in calls {
        if current != Some((c.session_id.as_str(), c.turn.as_str())) {
            close(&mut streaks, &mut out);
            current = Some((c.session_id.as_str(), c.turn.as_str()));
        }
        let key = call_key(&c.tool, c.input_json.as_deref());
        if !c.failed {
            if let Some((_, failed)) = streaks.remove(&key) {
                push_loop(&mut out, key, &failed);
            }
            continue;
        }
        let signature = error_signature(c.error.as_deref().unwrap_or_default());
        match streaks.get_mut(&key) {
            Some((sig, failed)) if *sig == signature => failed.push(c),
            _ => {
                if let Some((_, failed)) = streaks.insert(key.clone(), (signature, vec![c])) {
                    push_loop(&mut out, key, &failed);
                }
            }
        }
    }
    close(&mut streaks, &mut out);
    out.sort_by(|a, b| {
        a.session_id
            .cmp(&b.session_id)
            .then_with(|| a.first_ts.cmp(&b.first_ts))
    });
    out
}

fn push_loop(out: &mut Vec<Loop>, call: String, failed: &[&Call]) {
    let mut attempts: Vec<usize> = failed.iter().map(|c| c.request).collect();
    attempts.dedup();
    if attempts.len() < LOOP_LENGTH {
        return;
    }
    let first = failed[0];
    let last = failed[failed.len() - 1];
    out.push(Loop {
        session_id: first.session_id.clone(),
        call,
        error: crate::secrets::redact(&error_line(first.error.as_deref().unwrap_or_default())),
        failures: attempts.len(),
        first_ts: first.ts.clone(),
        last_ts: last.ts.clone(),
    });
}

/// The tool, plus the first two words of a shell command after any `cd`.
pub(crate) fn call_key(tool: &str, input_json: Option<&str>) -> String {
    let command = input_json
        .and_then(|i| serde_json::from_str::<Value>(i).ok())
        .and_then(|v| v.get("command").and_then(Value::as_str).map(str::to_string));
    let Some(command) = command else {
        return tool.to_string();
    };
    let head = command_head(&command);
    if head.is_empty() {
        tool.to_string()
    } else {
        format!("{tool}({head})")
    }
}

/// The first two words of the first step of a command that isn't a change of folder.
pub(crate) fn command_head(command: &str) -> String {
    command
        .split(['\n', ';'])
        .flat_map(|line| line.split("&&"))
        .map(str::trim)
        .find(|step| {
            let first = step.split_whitespace().next().unwrap_or_default();
            !step.is_empty()
                && !matches!(
                    first.to_ascii_lowercase().as_str(),
                    "cd" | "set-location" | "push-location" | "pushd" | "sl"
                )
        })
        .map(|step| {
            step.split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

/// The first line of an error that says what went wrong. Unwraps the JSON and tags the
/// harnesses add, and skips bare exit codes.
pub(crate) fn error_line(detail: &str) -> String {
    let text = serde_json::from_str::<Value>(detail)
        .ok()
        .and_then(|v| {
            ["message", "clientVisibleErrorMessage", "error"]
                .iter()
                .find_map(|k| v.get(*k).and_then(Value::as_str).map(str::to_string))
        })
        .unwrap_or_else(|| detail.to_string());
    let text = text
        .replace("<tool_use_error>", "")
        .replace("</tool_use_error>", "");
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.clone().find(|l| !is_exit_code(l));
    let line = first.or_else(|| lines.next()).unwrap_or_default();
    let mut out: String = line.chars().take(160).collect();
    if line.chars().count() > 160 {
        out.push('…');
    }
    out
}

fn is_exit_code(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower
        .strip_prefix("exit code")
        .is_some_and(|rest| rest.trim().chars().all(|c| c.is_ascii_digit() || c == '-'))
}

/// The error line with paths, numbers and ids removed, so the same mistake on another file
/// or line still matches.
pub(crate) fn error_signature(detail: &str) -> String {
    error_line(detail)
        .to_lowercase()
        .split_whitespace()
        .map(|word| {
            if word.contains(['/', '\\']) {
                "<path>".to_string()
            } else if word.chars().filter(char::is_ascii_hexdigit).count() >= 6
                && word
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || "-:.,'\"`()".contains(c))
            {
                "#".to_string()
            } else {
                let mut out = String::new();
                for ch in word.chars() {
                    if ch.is_ascii_digit() {
                        if !out.ends_with('#') {
                            out.push('#');
                        }
                    } else {
                        out.push(ch);
                    }
                }
                out
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What a loop cost: requests after the first failure, up to the last one.
#[derive(Debug, Default, Clone)]
struct Spend {
    requests: i64,
    usd: f64,
    credits: f64,
}

impl Spend {
    fn add(&mut self, other: &Spend) {
        self.requests += other.requests;
        self.usd += other.usd;
        self.credits += other.credits;
    }

    /// For ranking only: dollars and credits are never added up for display.
    fn weight(&self) -> f64 {
        self.usd + self.credits * 0.01 + self.requests as f64 * 1e-6
    }

    fn describe(&self) -> String {
        let requests = format!(
            "{} request{}",
            self.requests,
            if self.requests == 1 { "" } else { "s" }
        );
        let mut parts = Vec::new();
        if self.usd > 0.0 {
            parts.push(money(self.usd));
        }
        if self.credits > 0.0 {
            parts.push(format!("{:.1} AI credits", self.credits));
        }
        if parts.is_empty() {
            requests
        } else {
            format!("{requests} ({})", parts.join(", "))
        }
    }
}

fn loop_spend(conn: &Connection, l: &Loop) -> Result<Spend> {
    Ok(conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(cost_usd), 0), COALESCE(SUM(ai_units), 0)
         FROM main.requests WHERE session_id = ?1 AND ts > ?2 AND ts <= ?3",
        [&l.session_id, &l.first_ts, &l.last_ts],
        |r| {
            Ok(Spend {
                requests: r.get(0)?,
                usd: r.get(1)?,
                credits: r.get(2)?,
            })
        },
    )?)
}

fn session_titles(conn: &Connection, loops: &[Loop]) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT COALESCE(NULLIF(title, ''), project, '(untitled session)') FROM main.sessions WHERE id = ?1",
    )?;
    for l in loops {
        if out.contains_key(&l.session_id) {
            continue;
        }
        if let Ok(title) = stmt.query_row([&l.session_id], |r| r.get::<_, String>(0)) {
            out.insert(l.session_id.clone(), title);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_ignore_paths_numbers_and_ids() {
        assert_eq!(
            error_signature(
                "Exit code 1\nerror: file C:\\work\\a.rs line 12 not found (id 3fa9c2e1)"
            ),
            "error: file <path> line # not found (id #"
        );
        assert_eq!(
            error_line(r#"{"code":"failure","message":"Parent directory does not exist"}"#),
            "Parent directory does not exist"
        );
        assert_eq!(
            error_line("<tool_use_error>File has not been read yet.</tool_use_error>"),
            "File has not been read yet."
        );
        assert_eq!(error_line("Exit code 2"), "Exit code 2");
    }

    #[test]
    fn command_heads_skip_changes_of_folder() {
        assert_eq!(command_head("cd /repo && cargo test --all"), "cargo test");
        assert_eq!(command_head("Set-Location P:\\x; npm run build"), "npm run");
        assert_eq!(command_head("git status"), "git status");
    }
}
