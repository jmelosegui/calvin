//! Repository facts: things about a project's environment that agents keep relearning
//! the hard way, drafted as lines for the project's instruction file.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;

use super::loops::command_head;
use super::{Evidence, Metric, Opportunity, Status, repo_root, skip};
use crate::insights::Since;

/// Sessions of a project a fact must show up in before calvin suggests it.
const FACT_SESSIONS: usize = 3;
/// How far after a failure to look for what fixed it.
const LOOKAHEAD: usize = 8;

/// Something an agent had to find out by failing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Fact {
    /// A command isn't installed; `instead` is what worked in its place.
    Missing {
        command: String,
        instead: Option<String>,
    },
    /// The shell is PowerShell, so `&&` chains fail (Windows PowerShell 5).
    PowerShellChain,
    /// The shell is PowerShell, which has no heredocs.
    PowerShellHeredoc,
    /// A command needs an environment variable set first.
    Variable { name: String, command: String },
    /// A command only works after another one ran.
    Prerequisite { first: String, then: String },
}

impl Fact {
    /// Facts about the machine and its shell, not one repository.
    fn is_machine(&self) -> bool {
        matches!(
            self,
            Fact::Missing { .. } | Fact::PowerShellChain | Fact::PowerShellHeredoc
        )
    }

    fn line(&self) -> String {
        match self {
            Fact::Missing {
                command,
                instead: Some(instead),
            } => format!("Use `{instead}` instead of `{command}`; `{command}` is not available here."),
            Fact::Missing { command, instead: None } => {
                format!("`{command}` is not available in this environment.")
            }
            Fact::PowerShellChain => {
                "The shell is PowerShell: chain commands with `;`, not `&&`.".into()
            }
            Fact::PowerShellHeredoc => {
                "The shell is PowerShell: there are no heredocs (`<<`); pipe a here-string or write a file instead.".into()
            }
            Fact::Variable { name, command } => format!("Set `{name}` before running `{command}`."),
            Fact::Prerequisite { first, then } => format!("Run `{first}` before `{then}`."),
        }
    }

    /// Whether instruction text (lowercased) already says this.
    fn covered_by(&self, text: &str) -> bool {
        match self {
            Fact::Missing { command, instead } => {
                mentions(text, command) && instead.as_deref().is_none_or(|i| mentions(text, i))
            }
            Fact::PowerShellChain | Fact::PowerShellHeredoc => mentions(text, "powershell"),
            Fact::Variable { name, .. } => mentions(text, name),
            Fact::Prerequisite { first, .. } => mentions(text, first),
        }
    }
}

/// Whether `text` contains `phrase` as whole words (`py` is not in `copy`).
fn mentions(text: &str, phrase: &str) -> bool {
    let phrase = phrase.to_lowercase();
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    text.match_indices(&phrase).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + phrase.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

/// A tool call, in the order it ran within its session.
#[derive(Debug, Clone)]
pub(crate) struct Call {
    pub session_id: String,
    pub ts: String,
    pub tool: String,
    /// The shell command, when the call ran one.
    pub command: Option<String>,
    pub failed: bool,
    pub error: String,
}

impl Call {
    fn succeeded_shell(&self) -> Option<&str> {
        self.command.as_deref().filter(|_| !self.failed)
    }

    fn changes_files(&self) -> bool {
        let tool = self.tool.to_ascii_lowercase();
        ["edit", "write", "create", "patch", "replace"]
            .iter()
            .any(|w| tool.contains(w))
    }
}

/// One sighting of a fact.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sighting {
    pub fact: Fact,
    pub session_id: String,
    pub ts: String,
}

/// Facts found in one session's calls, in order.
pub(crate) fn find_facts(calls: &[Call]) -> Vec<Sighting> {
    let mut out = Vec::new();
    for (i, f) in calls.iter().enumerate() {
        let Some(command) = f.command.as_deref().filter(|_| f.failed) else {
            continue;
        };
        let after = || {
            calls[i + 1..]
                .iter()
                .take(LOOKAHEAD)
                .filter(|c| c.session_id == f.session_id)
        };
        let mut found = Vec::new();
        if f.tool.eq_ignore_ascii_case("powershell") {
            let lower = f.error.to_lowercase();
            if lower.contains("is not a valid statement separator") {
                found.push(Fact::PowerShellChain);
            }
            if lower.contains("missing file specification after redirection operator")
                || lower.contains("operator is reserved for future use")
            {
                found.push(Fact::PowerShellHeredoc);
            }
        }
        if let Some(missing) = missing_command(&f.error) {
            let instead = after()
                .filter_map(Call::succeeded_shell)
                .find_map(|s| replacement(&missing, command, s));
            found.push(Fact::Missing {
                command: missing,
                instead,
            });
        } else if let Some(name) = unset_variable(&f.error) {
            let set = after().filter_map(Call::succeeded_shell).any(|s| {
                s.contains(&format!("{name}="))
                    || s.contains(&format!("$env:{name}"))
                    || s.contains(&format!("export {name}"))
            });
            if set {
                found.push(Fact::Variable {
                    name,
                    command: command_head(command),
                });
            }
        } else if let Some(first) = prerequisite(command, after()) {
            found.push(Fact::Prerequisite {
                first,
                then: command_head(command),
            });
        }
        out.extend(found.into_iter().map(|fact| Sighting {
            fact,
            session_id: f.session_id.clone(),
            ts: f.ts.clone(),
        }));
    }
    out
}

/// The command named in a "not found" error.
pub(crate) fn missing_command(error: &str) -> Option<String> {
    let clean = |s: &str| {
        let s = s.trim().trim_matches(['\'', '"', '`']).to_lowercase();
        let s = s.strip_suffix(".exe").unwrap_or(&s).to_string();
        (!s.is_empty() && !s.contains([' ', '/', '\\'])).then_some(s)
    };
    for line in error.lines().map(str::trim) {
        if let Some(pos) = line.find(" is not recognized") {
            let before = &line[..pos];
            let name = before.rsplit(['\'', '"']).nth(1).unwrap_or(before);
            if let Some(name) = clean(name) {
                return Some(name);
            }
        }
        for marker in [": command not found", ": not found"] {
            if let Some(pos) = line.find(marker) {
                let before = &line[..pos];
                // `sh: 1: jq: not found`, not `error: not found`.
                if marker == ": not found" && before.matches(": ").count() < 2 {
                    continue;
                }
                if let Some(name) = clean(before.rsplit(": ").next().unwrap_or(before)) {
                    return Some(name);
                }
            }
        }
        if let Some(pos) = line.find(" was not found;")
            && let Some(name) = clean(&line[..pos])
        {
            return Some(name);
        }
    }
    None
}

/// What ran in place of a missing command: the same arguments after another program
/// (`py -m x` for `python -m x`), or a wrapper that runs it (`python -m pytest`).
fn replacement(missing: &str, failed: &str, success: &str) -> Option<String> {
    let failed_step = first_step(failed);
    let mut failed_words = failed_step.split_whitespace();
    // Only when the failed command was the missing program itself, not a pipe into it.
    if !failed_words
        .next()
        .is_some_and(|p| p.eq_ignore_ascii_case(missing))
    {
        return None;
    }
    let failed_args: Vec<&str> = failed_words.take(2).collect();
    let step = first_step(success);
    let words: Vec<&str> = step.split_whitespace().collect();
    let program = words.first()?.to_lowercase();
    if program == missing {
        return None;
    }
    if let Some(at) = words
        .iter()
        .skip(1)
        .position(|w| w.eq_ignore_ascii_case(missing))
    {
        return Some(words[..at + 2].join(" "));
    }
    let args: Vec<&str> = words.iter().skip(1).take(2).copied().collect();
    let related =
        program.len() >= 2 && (missing.starts_with(&program) || program.starts_with(missing));
    if related || (!failed_args.is_empty() && failed_args == args) {
        return Some(words[0].to_string());
    }
    None
}

/// The first step of a command that isn't a change of folder.
fn first_step(command: &str) -> String {
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
        .unwrap_or_default()
        .to_string()
}

/// An environment variable an error says is missing.
pub(crate) fn unset_variable(error: &str) -> Option<String> {
    let lower = error.to_lowercase();
    if !(lower.contains("not set")
        || lower.contains("not defined")
        || lower.contains("environment variable"))
    {
        return None;
    }
    error
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .find(|w| {
            w.len() >= 3
                && w.contains('_')
                && w.chars().any(|c| c.is_ascii_uppercase())
                && w.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        })
        .map(str::to_string)
}

/// A command that, once run, let the failed command succeed with no file changed between.
fn prerequisite<'a>(failed: &str, after: impl Iterator<Item = &'a Call>) -> Option<String> {
    let head = command_head(failed);
    if head.is_empty() {
        return None;
    }
    let mut first = None;
    for c in after {
        if c.changes_files() {
            return None;
        }
        let Some(command) = c.command.as_deref() else {
            continue;
        };
        let this = command_head(command);
        if this == head {
            return if c.failed { None } else { first };
        }
        if first.is_none() && !c.failed && !is_lookup(&this) {
            first = Some(this);
        }
    }
    None
}

/// Commands that only look around, so they can't be what fixed a failure.
fn is_lookup(head: &str) -> bool {
    let lower = head.to_lowercase();
    let program = lower.split_whitespace().next().unwrap_or_default();
    matches!(
        program,
        "ls" | "dir"
            | "cat"
            | "type"
            | "pwd"
            | "echo"
            | "which"
            | "where"
            | "where.exe"
            | "head"
            | "tail"
            | "grep"
            | "rg"
            | "find"
            | "get-childitem"
            | "get-content"
            | "get-command"
            | "get-location"
            | "select-string"
            | "test-path"
            | "write-host"
            | "write-output"
    ) || matches!(
        lower.as_str(),
        "git status" | "git log" | "git diff" | "git show" | "git branch"
    )
}

/// Session id → (project root, harness).
type Sessions = BTreeMap<String, (PathBuf, String)>;

/// Every shell call in the period, plus the other calls of those sessions, by session.
fn load_calls(conn: &Connection, since: &Since) -> Result<(Vec<Call>, Sessions)> {
    let mut sessions = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT id, cwd, harness FROM main.sessions
         WHERE cwd IS NOT NULL AND harness IN ('claude-code', 'copilot-cli', 'cursor')",
    )?;
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (id, cwd, harness) = row?;
        let dir = PathBuf::from(&cwd);
        let root = if dir.is_dir() {
            project_root(&dir)
        } else {
            dir
        };
        sessions.insert(id, (root, harness));
    }

    let mut stmt = conn.prepare(
        "SELECT t.session_id, t.ts, t.tool,
                CASE WHEN t.input_json LIKE '%\"command\"%' THEN t.input_json END,
                t.outcome, t.result_detail
         FROM main.tool_calls t
         WHERE t.ts >= ?1 AND t.session_id IN (
             SELECT session_id FROM main.tool_calls
             WHERE ts >= ?1 AND outcome = 'error' AND input_json LIKE '%\"command\"%')
            OR (t.ts >= ?1 AND t.input_json LIKE '%\"command\"%' AND t.outcome = 'ok')
         ORDER BY t.session_id, t.ts, t.id",
    )?;
    let mut calls = Vec::new();
    for row in stmt.query_map([&since.0], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })? {
        let (session_id, ts, tool, input, outcome, detail) = row?;
        let command = input
            .and_then(|i| serde_json::from_str::<Value>(&i).ok())
            .and_then(|v| v.get("command").and_then(Value::as_str).map(str::to_string));
        calls.push(Call {
            session_id,
            ts,
            tool,
            command,
            failed: outcome.as_deref() == Some("error"),
            error: detail.unwrap_or_default(),
        });
    }
    Ok((calls, sessions))
}

/// Whether a later success shows the fact no longer holds. Only a lone command counts:
/// in a chain, the last step decides the outcome. Copilot CLI marks shell calls
/// successful whatever the exit code, so its successes prove nothing.
fn contradicted(fact: &Fact, call: &Call, harness: Option<&str>) -> bool {
    let Some(command) = call.succeeded_shell() else {
        return false;
    };
    if harness == Some("copilot-cli") {
        return false;
    }
    match fact {
        Fact::Missing { command: name, .. } => {
            !command.contains([';', '|', '\n', '&'])
                && command
                    .split_whitespace()
                    .next()
                    .is_some_and(|p| p.eq_ignore_ascii_case(name))
        }
        Fact::PowerShellChain => {
            call.tool.eq_ignore_ascii_case("powershell") && command.contains("&&")
        }
        Fact::PowerShellHeredoc => {
            call.tool.eq_ignore_ascii_case("powershell") && command.contains("<<")
        }
        _ => false,
    }
}

/// Where a fact holds: one repository, or every project on this machine.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Scope {
    Machine,
    Project(PathBuf),
}

/// The repository a folder belongs to. Worktrees count as their main repository.
fn project_root(dir: &Path) -> PathBuf {
    let root = repo_root(dir);
    let main = std::fs::read_to_string(root.join(".git"))
        .ok()
        .and_then(|text| {
            let gitdir = text
                .trim()
                .strip_prefix("gitdir:")?
                .trim()
                .replace('\\', "/");
            let at = gitdir.find("/.git/worktrees/")?;
            Some(PathBuf::from(&gitdir[..at]))
        });
    main.filter(|p| p.is_dir()).unwrap_or(root)
}

/// The instruction file a project's facts belong in.
fn project_file(root: &Path, harnesses: &BTreeSet<String>) -> PathBuf {
    let only = |h: &str| harnesses.len() == 1 && harnesses.contains(h);
    if root.join("AGENTS.md").is_file() {
        root.join("AGENTS.md")
    } else if only("claude-code") {
        root.join("CLAUDE.md")
    } else if only("copilot-cli") {
        root.join(".github").join("copilot-instructions.md")
    } else {
        root.join("AGENTS.md")
    }
}

/// The personal instruction files of the harnesses that hit a machine fact.
fn personal_files(ctx: &FactDirs, harnesses: &BTreeSet<String>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if harnesses.contains("claude-code") {
        out.push(ctx.claude_dir.join("CLAUDE.md"));
    }
    if harnesses.contains("copilot-cli") {
        out.push(ctx.copilot_dir.join("copilot-instructions.md"));
    }
    if out.is_empty() {
        // Cursor keeps user rules in its settings; AGENTS.md is the shared fallback.
        out.push(ctx.claude_dir.join("CLAUDE.md"));
    }
    out
}

fn read_all(files: &[PathBuf]) -> String {
    files
        .iter()
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .collect::<Vec<_>>()
        .join("\n")
        .to_lowercase()
}

/// Where the personal instruction files live.
pub(super) struct FactDirs<'a> {
    pub claude_dir: &'a Path,
    pub copilot_dir: &'a Path,
}

#[derive(Default)]
struct Seen {
    sessions: BTreeSet<String>,
    harnesses: BTreeSet<String>,
    failures: usize,
    /// What worked in place of a missing command, and how often.
    instead: BTreeMap<String, usize>,
    last_ts: String,
    last_session: String,
}

pub(super) fn repo_facts(conn: &Connection, since: &Since, dirs: &FactDirs) -> Result<Opportunity> {
    let (calls, sessions) = load_calls(conn, since)?;
    let project_of = |id: &str| sessions.get(id).map(|(root, _)| root);

    let mut seen: BTreeMap<(Scope, Fact), Seen> = BTreeMap::new();
    for s in find_facts(&calls) {
        let Some((root, harness)) = sessions.get(&s.session_id) else {
            continue;
        };
        let (fact, instead) = match s.fact {
            Fact::Missing { command, instead } => (
                Fact::Missing {
                    command,
                    instead: None,
                },
                instead,
            ),
            other => (other, None),
        };
        let scope = if fact.is_machine() {
            Scope::Machine
        } else {
            Scope::Project(root.clone())
        };
        let entry = seen.entry((scope, fact)).or_default();
        entry.sessions.insert(s.session_id.clone());
        entry.harnesses.insert(harness.clone());
        entry.failures += 1;
        if let Some(instead) = instead {
            *entry.instead.entry(instead).or_default() += 1;
        }
        if s.ts > entry.last_ts {
            entry.last_ts = s.ts;
            entry.last_session = s.session_id;
        }
    }
    seen.retain(|_, v| v.sessions.len() >= FACT_SESSIONS);
    seen.retain(|(scope, fact), v| {
        !calls.iter().any(|c| {
            c.ts > v.last_ts
                && match scope {
                    Scope::Machine => true,
                    Scope::Project(root) => project_of(&c.session_id) == Some(root),
                }
                && contradicted(
                    fact,
                    c,
                    sessions.get(&c.session_id).map(|(_, h)| h.as_str()),
                )
        })
    });
    if seen.is_empty() {
        return Ok(skip());
    }

    let mut harnesses: BTreeMap<&PathBuf, BTreeSet<String>> = BTreeMap::new();
    for (root, harness) in sessions.values() {
        harnesses.entry(root).or_default().insert(harness.clone());
    }
    let mut ranked: Vec<_> = seen.into_iter().collect();
    ranked.sort_by(|a, b| {
        b.1.sessions
            .len()
            .cmp(&a.1.sessions.len())
            .then_with(|| b.1.failures.cmp(&a.1.failures))
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut open: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut evidence = Vec::new();
    let mut written = 0usize;
    let total = ranked.len();
    let mut projects = BTreeSet::new();
    for ((scope, mut fact), v) in ranked {
        if let Fact::Missing { instead, .. } = &mut fact {
            // The most common replacement; ties go to the first by name.
            *instead = v
                .instead
                .iter()
                .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                .map(|(i, _)| i.clone());
        }
        let (files, place) = match &scope {
            Scope::Machine => (
                personal_files(dirs, &v.harnesses),
                "this machine".to_string(),
            ),
            Scope::Project(root) => {
                projects.insert(root.clone());
                (
                    vec![project_file(
                        root,
                        harnesses.get(root).unwrap_or(&BTreeSet::new()),
                    )],
                    root.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| root.display().to_string()),
                )
            }
        };
        let covered = fact.covered_by(&match &scope {
            Scope::Machine => read_all(&files),
            Scope::Project(root) => read_all(&[
                root.join("AGENTS.md"),
                root.join("CLAUDE.md"),
                root.join(".github").join("copilot-instructions.md"),
            ]),
        });
        let target = files
            .iter()
            .map(|f| f.display().to_string())
            .collect::<Vec<_>>()
            .join(" and ");
        if covered {
            written += 1;
        } else {
            open.entry(target.clone()).or_default().push(fact.line());
        }
        evidence.push(Evidence {
            label: fact.line(),
            detail: Some(format!(
                "{place} · {} sessions · failed {}× · {}",
                v.sessions.len(),
                v.failures,
                if covered {
                    "already in the instructions".to_string()
                } else {
                    format!("add to {target}")
                }
            )),
            link: Some(format!("/sessions#{}", v.last_session)),
        });
    }
    let missing = total - written;
    let snippet = open
        .iter()
        .map(|(file, lines)| {
            let body: Vec<String> = lines.iter().map(|l| format!("- {l}")).collect();
            format!("<!-- {file} -->\n{}", body.join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let s = |n: usize| if n == 1 { "" } else { "s" };
    let place = match projects.len() {
        0 => "this machine".to_string(),
        n => format!("{n} project{}", s(n)),
    };
    Ok(Opportunity {
        id: "repo-facts",
        area: "Cross-tool",
        status: if missing == 0 { Status::Good } else { Status::Action },
        title: "Write down what agents keep relearning".into(),
        finding: if missing == 0 {
            format!(
                "Agents kept running into {total} fact{} about {place}, and your instructions already cover {}.",
                s(total),
                if total == 1 { "it" } else { "them all" },
            )
        } else {
            format!(
                "Agents learned {total} fact{} about {place} by failing, in {FACT_SESSIONS} or more sessions each. {missing} of them {} not in your instructions yet.",
                s(total),
                if missing == 1 { "is" } else { "are" },
            )
        },
        why: "Every new session starts without what the last one found out, so it fails the same way, reads the error, and tries again. One line in an instruction file saves that detour in every future session.".into(),
        fix: "Add the lines to the file shown, reword them as you like, and commit project files so every tool and teammate gets them. AGENTS.md is read by Claude Code, Copilot CLI and Cursor; facts about this machine go in your personal instructions. calvin never edits these files.".into(),
        snippet: (!snippet.is_empty()).then_some(snippet),
        saving_usd: None,
        evidence: evidence.into_iter().take(12).collect(),
        metric: Metric::lower(missing as f64, "repeated environment mistakes not written down"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_commands_are_named_from_each_shell() {
        assert_eq!(
            missing_command(
                "Exit code 1\n'vswhere.exe' is not recognized as an internal or external command,"
            ),
            Some("vswhere".into())
        );
        assert_eq!(
            missing_command("pytest : The term 'pytest' is not recognized as the name of a cmdlet"),
            Some("pytest".into())
        );
        assert_eq!(
            missing_command("/usr/bin/bash: line 1: jq: command not found"),
            Some("jq".into())
        );
        assert_eq!(
            missing_command(
                "Python was not found; run without arguments to install from the Microsoft Store"
            ),
            Some("python".into())
        );
        assert_eq!(missing_command("error: file not found"), None);
    }

    #[test]
    fn replacements_keep_the_same_intent() {
        assert_eq!(
            replacement("python", "python -c 'print(1)'", "py -c 'print(1)'"),
            Some("py".into())
        );
        assert_eq!(
            replacement("pytest", "pytest -q", "python -m pytest -q"),
            Some("python -m pytest".into())
        );
        assert_eq!(replacement("jq", "jq .a f.json", "git status"), None);
        // Piping into the missing program says nothing about what replaces it.
        assert_eq!(
            replacement("python", "gh api x | python -c 'y'", "gh api x"),
            None
        );
    }

    #[test]
    fn instructions_must_mention_whole_words() {
        assert!(!mentions("make a copy first", "py"));
        assert!(mentions("use `py` for scripts", "py"));
        assert!(
            Fact::Missing {
                command: "python".into(),
                instead: Some("py".into()),
            }
            .covered_by("python is the store stub here; run py instead.")
        );
    }

    #[test]
    fn variables_need_an_upper_case_name() {
        assert_eq!(
            unset_variable("error: environment variable `DATABASE_URL` not set"),
            Some("DATABASE_URL".into())
        );
        assert_eq!(unset_variable("value is not defined"), None);
    }
}
