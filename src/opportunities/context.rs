//! Context overhead: what the harness loads into every request before your prompt, and
//! MCP servers that cost context without being used.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;

use super::{Evidence, Metric, Opportunity, Status, money, pct, read_json, skip};
use crate::event::Usage;
use crate::insights::Since;
use crate::prices::PriceTable;

/// Sessions a server must sit idle through before calvin calls it unused.
pub(super) const IDLE_SESSIONS: i64 = 5;
/// Fixed Claude Code context worth trimming, in tokens per session.
const CLAUDE_FIXED_TOKENS: i64 = 10_000;
/// Copilot tool definitions worth trimming, in tokens per request.
const COPILOT_TOOL_TOKENS: i64 = 10_000;

/// Rough tokens for a text size; the logs record characters, not tokens.
fn tokens(chars: i64) -> i64 {
    (chars + 3) / 4
}

fn median(values: &mut [i64]) -> i64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    values[values.len() / 2]
}

fn plural(n: i64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

pub(super) fn thousands(n: i64) -> String {
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// Claude Code: the skill list and instruction files loaded into every session.
/// Runs inside the Claude-only views.
pub(super) fn claude_context_budget(
    conn: &Connection,
    since: &Since,
    prices: &PriceTable,
) -> Result<Opportunity> {
    let mut per_session: BTreeMap<String, i64> = BTreeMap::new();
    // name → (sessions, largest size in chars)
    let mut items: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT ci.session_id, ci.name, ci.chars FROM context_items ci
         JOIN sessions s ON s.id = ci.session_id
         WHERE ci.kind IN ('skill_listing', 'instructions') AND ci.chars > 0
           AND ci.first_seen >= ?1",
    )?;
    for row in stmt.query_map([&since.0], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (session, name, chars) = row?;
        *per_session.entry(session).or_default() += tokens(chars);
        let item = items.entry(name).or_default();
        item.0 += 1;
        item.1 = item.1.max(chars);
    }
    if per_session.len() < 3 {
        return Ok(skip());
    }

    // Every main-thread request re-reads that context, mostly from the prompt cache.
    let mut cost = 0.0;
    let mut stmt = conn.prepare(
        "SELECT session_id, model, COUNT(*) FROM requests
         WHERE is_sidechain = 0 AND model IS NOT NULL AND ts >= ?1 GROUP BY session_id, model",
    )?;
    for row in stmt.query_map([&since.0], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })? {
        let (session, model, requests) = row?;
        if let Some(&fixed) = per_session.get(&session) {
            let usage = Usage {
                cache_read: fixed,
                ..Usage::default()
            };
            cost += prices.cost(&model, &usage).unwrap_or(0.0) * requests as f64;
        }
    }

    let sessions = per_session.len() as i64;
    let typical = median(&mut per_session.into_values().collect::<Vec<_>>());
    let mut ranked: Vec<(String, (i64, i64))> = items.into_iter().collect();
    ranked.sort_by(|a, b| {
        (b.1.0 * b.1.1)
            .cmp(&(a.1.0 * a.1.1))
            .then_with(|| a.0.cmp(&b.0))
    });
    Ok(Opportunity {
        id: "context-budget",
        area: "Context",
        status: if typical >= CLAUDE_FIXED_TOKENS { Status::Consider } else { Status::Good },
        title: "Keep the context loaded into every session small".into(),
        finding: format!(
            "Claude Code loads about {} tokens of skill list and instruction files into a typical session (median of {sessions} sessions). Re-reading them with each request cost about {} in this period.",
            thousands(typical),
            money(cost)
        ),
        why: "These load before you type and are re-sent with every request, mostly from the prompt cache. Long instruction files also crowd out the rules that matter and leave less room for the work.".into(),
        fix: "Keep instruction files to rules the agent can't infer from the code. Move detail it needs only sometimes into a skill or a document it reads when needed, and remove skills you don't use.".into(),
        snippet: None,
        saving_usd: None,
        evidence: ranked
            .iter()
            .take(10)
            .map(|(name, (n, chars))| Evidence {
                label: if name == "skills" { "Skill list".into() } else { name.clone() },
                detail: Some(format!(
                    "about {} tokens · loaded in {n} session{}",
                    thousands(tokens(*chars)),
                    plural(*n)
                )),
                link: (name == "skills").then(|| "/skills".to_string()),
            })
            .collect(),
        metric: Metric::lower(typical as f64, "tokens loaded into every session"),
    })
}

/// Claude Code: MCP servers that keep failing to start. Runs inside the Claude-only views.
pub(super) fn claude_mcp_failing(conn: &Connection, since: &Since) -> Result<Opportunity> {
    let mut stmt = conn.prepare(
        "SELECT ci.name, COUNT(DISTINCT ci.session_id), MAX(ci.session_id) FROM context_items ci
         JOIN sessions s ON s.id = ci.session_id
         WHERE ci.kind = 'mcp_failed' AND ci.first_seen >= ?1
         GROUP BY ci.name HAVING COUNT(DISTINCT ci.session_id) >= 2 ORDER BY 2 DESC, 1",
    )?;
    let failing: Vec<(String, i64, String)> = stmt
        .query_map([&since.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let Some((first, _, _)) = failing.first() else {
        return Ok(skip());
    };
    Ok(Opportunity {
        id: "mcp-failing",
        area: "Context",
        status: Status::Action,
        title: "Fix or remove MCP servers that fail to start".into(),
        finding: format!(
            "{} MCP server{} failed to start in two or more sessions.",
            failing.len(),
            if failing.len() == 1 { "" } else { "s" }
        ),
        why: "A server that fails to start gives you none of its tools, but Claude Code still tries it every session, which slows startup and can leave the model expecting tools that aren't there.".into(),
        fix: "Run /mcp in Claude Code to see the error, then fix the server's command or sign-in, or remove it.".into(),
        snippet: Some(format!("claude mcp remove {first}")),
        saving_usd: None,
        evidence: failing
            .iter()
            .map(|(name, n, session)| Evidence {
                label: name.clone(),
                detail: Some(format!("failed to start in {n} sessions")),
                link: Some(format!("/sessions#{session}")),
            })
            .collect(),
        metric: Metric::lower(failing.len() as f64, "MCP servers failing to start"),
    })
}

/// Claude Code: MCP servers the harness loaded (from config, plugins or connectors), with
/// the number of sessions each loaded in. Runs inside the Claude-only views.
pub(super) fn claude_loaded_mcp_servers(
    conn: &Connection,
    since: &Since,
) -> Result<BTreeMap<String, i64>> {
    let mut stmt = conn.prepare(
        "SELECT ci.session_id, ci.name FROM context_items ci
         JOIN sessions s ON s.id = ci.session_id
         WHERE ci.kind = 'tool' AND ci.name LIKE 'mcp\\_\\_%' ESCAPE '\\' AND ci.first_seen >= ?1",
    )?;
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for row in stmt.query_map([&since.0], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (session, tool) = row?;
        if let Some(server) = crate::adapters::claude_code::mcp_server(&tool) {
            seen.insert((server, session));
        }
    }
    let mut out = BTreeMap::new();
    for (server, _) in seen {
        *out.entry(server).or_default() += 1;
    }
    Ok(out)
}

/// Copilot CLI: tokens of tool definitions sent with every request.
pub(super) fn copilot_context_budget(
    conn: &Connection,
    since: &Since,
    copilot_dir: &Path,
) -> Result<Opportunity> {
    let mut stmt = conn.prepare(
        "SELECT s.id, COALESCE(s.title, ''), COALESCE(s.project, ''), COUNT(*),
                SUM(c.tool_tokens), COALESCE(SUM(c.prompt_tokens), 0),
                MAX(c.tool_tokens), MAX(c.tool_count)
         FROM context_samples c JOIN sessions s ON s.id = c.session_id
         WHERE s.harness = 'copilot-cli' AND c.is_sidechain = 0 AND c.tool_tokens IS NOT NULL
           AND c.ts >= ?1
         GROUP BY s.id",
    )?;
    let rows: Vec<CopilotSession> = stmt
        .query_map([&since.0], |r| {
            Ok(CopilotSession {
                id: r.get(0)?,
                title: r.get(1)?,
                project: r.get(2)?,
                requests: r.get(3)?,
                tool_tokens: r.get(4)?,
                prompt_tokens: r.get(5)?,
                peak_tokens: r.get(6)?,
                peak_tools: r.get::<_, Option<i64>>(7)?.unwrap_or_default(),
            })
        })?
        .collect::<Result<_, _>>()?;
    if rows.len() < 3 {
        return Ok(skip());
    }
    let typical = median(
        &mut rows
            .iter()
            .map(|r| r.tool_tokens / r.requests.max(1))
            .collect::<Vec<_>>(),
    );
    let tool_tokens: i64 = rows.iter().map(|r| r.tool_tokens).sum();
    let prompt_tokens: i64 = rows.iter().map(|r| r.prompt_tokens).sum();
    let tool_search =
        copilot_setting(copilot_dir, "toolSearch").and_then(|v| v.as_bool()) == Some(true);
    let mut ranked: Vec<&CopilotSession> = rows.iter().collect();
    ranked.sort_by(|a, b| {
        b.peak_tokens
            .cmp(&a.peak_tokens)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(Opportunity {
        id: "copilot-context-budget",
        area: "Context",
        status: if typical >= COPILOT_TOOL_TOKENS { Status::Consider } else { Status::Good },
        title: "Send fewer tool definitions with each Copilot request".into(),
        finding: format!(
            "Tool definitions take about {} tokens of a typical Copilot request (median of {} sessions): {}% of all prompt tokens, {} tokens in this period.",
            thousands(typical),
            rows.len(),
            pct(tool_tokens, prompt_tokens),
            thousands(tool_tokens)
        ),
        why: "Copilot sends the definition of every available tool with each request. Tools you never call still take room in the context window and add to what every request processes.".into(),
        fix: if tool_search {
            "Tool search is on. Disable the MCP servers you don't use, and leave out tools you never need with --excluded-tools.".into()
        } else {
            "Turn on tool search so tool definitions load only when needed, and disable the MCP servers you don't use.".into()
        },
        snippet: Some(if tool_search {
            "/mcp disable <server-name>".into()
        } else {
            "/settings toolSearch on".into()
        }),
        saving_usd: None,
        evidence: ranked
            .iter()
            .take(8)
            .map(|s| Evidence {
                label: if s.title.is_empty() { "(untitled session)".into() } else { s.title.clone() },
                detail: Some(format!(
                    "{} · up to {} tokens for {} tools per request · {}% of prompt tokens over {} request{}",
                    s.project,
                    thousands(s.peak_tokens),
                    s.peak_tools,
                    pct(s.tool_tokens, s.prompt_tokens),
                    s.requests,
                    plural(s.requests)
                )),
                link: Some(format!("/sessions#{}", s.id)),
            })
            .collect(),
        metric: Metric::lower(typical as f64, "tool-definition tokens per Copilot request"),
    })
}

struct CopilotSession {
    id: String,
    title: String,
    project: String,
    requests: i64,
    tool_tokens: i64,
    prompt_tokens: i64,
    peak_tokens: i64,
    peak_tools: i64,
}

/// Copilot CLI: MCP servers that load with every session but were not called.
pub(super) fn copilot_mcp_unused(
    conn: &Connection,
    since: &Since,
    copilot_dir: &Path,
) -> Result<Opportunity> {
    let sessions: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sessions WHERE harness = 'copilot-cli' AND started_at >= ?1",
        [&since.0],
        |r| r.get(0),
    )?;
    if sessions < IDLE_SESSIONS {
        return Ok(skip());
    }
    // name → where calvin learned about it
    let mut known: BTreeMap<String, String> = BTreeMap::new();
    if let Some(servers) = read_json(&copilot_dir.join("mcp-config.json"))
        .get("mcpServers")
        .and_then(Value::as_object)
    {
        for name in servers.keys() {
            known.insert(name.clone(), "configured in mcp-config.json".into());
        }
    }
    let mut stmt = conn.prepare(
        "SELECT t.mcp_server, MAX(t.ts) FROM tool_calls t JOIN sessions s ON s.id = t.session_id
         WHERE s.harness = 'copilot-cli' AND t.mcp_server IS NOT NULL GROUP BY t.mcp_server",
    )?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (name, last) = row?;
        known
            .entry(name)
            .or_insert_with(|| format!("last called {}", &last[..last.len().min(10)]));
    }
    let disabled: BTreeSet<String> = copilot_setting(copilot_dir, "disabledMcpServers")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    known.retain(|name, _| !disabled.contains(name));
    if known.is_empty() {
        return Ok(skip());
    }

    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut stmt = conn.prepare(
        "SELECT DISTINCT t.mcp_server, t.tool FROM tool_calls t
         JOIN sessions s ON s.id = t.session_id
         WHERE s.harness = 'copilot-cli' AND t.ts >= ?1",
    )?;
    for row in stmt.query_map([&since.0], |r| {
        Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?))
    })? {
        let (server, tool) = row?;
        match server {
            Some(server) => {
                used.insert(server);
            }
            // Older Copilot versions don't name the server; their tools are `<server>-<tool>`.
            None => {
                if let Some(name) = known.keys().find(|k| tool.starts_with(&format!("{k}-"))) {
                    used.insert(name.clone());
                }
            }
        }
    }
    let unused: Vec<(&String, &String)> = known
        .iter()
        .filter(|(name, _)| !used.contains(*name))
        .collect();
    let builtin_unused = unused.iter().any(|(name, _)| *name == "github-mcp-server");
    Ok(Opportunity {
        id: "copilot-mcp-unused",
        area: "Context",
        status: if unused.is_empty() { Status::Good } else { Status::Action },
        title: "Disable Copilot MCP servers you don't use".into(),
        finding: if unused.is_empty() {
            format!(
                "All {} MCP servers calvin knows about were used in your {sessions} Copilot sessions.",
                known.len()
            )
        } else {
            format!(
                "{} of {} MCP servers were not called once in your {sessions} Copilot sessions in this period.",
                unused.len(),
                known.len()
            )
        },
        why: "Every enabled server adds the definitions of all its tools to each request, whether or not the model calls them.".into(),
        fix: format!(
            "Disable the ones you don't need with /mcp disable, which keeps their configuration so /mcp enable turns them back on.{}",
            if builtin_unused {
                " To leave out the built-in GitHub server for one session, start Copilot with --disable-builtin-mcps."
            } else {
                ""
            }
        ),
        snippet: unused
            .first()
            .map(|(name, _)| format!("copilot mcp disable {name}")),
        saving_usd: None,
        evidence: unused
            .iter()
            .map(|(name, source)| Evidence {
                label: (*name).clone(),
                detail: Some(format!("{source} · not called in this period")),
                link: None,
            })
            .collect(),
        metric: Metric::lower(unused.len() as f64, "unused Copilot MCP servers"),
    })
}

/// A Copilot setting from `settings.json`, else `config.json` (which may hold comments).
fn copilot_setting(copilot_dir: &Path, key: &str) -> Option<Value> {
    ["settings.json", "config.json"].iter().find_map(|file| {
        let text = std::fs::read_to_string(copilot_dir.join(file)).ok()?;
        let value: Value = serde_json::from_str(&strip_comments(&text)).ok()?;
        value.get(key).cloned()
    })
}

/// JSON with `//` and `/* */` comments → JSON. Strings are left alone.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_comments_outside_strings() {
        let text = "{\n // note\n \"url\": \"http://x\", /* gone */ \"on\": true\n}";
        let value: Value = serde_json::from_str(&strip_comments(text)).unwrap();
        assert_eq!(value["url"], "http://x");
        assert_eq!(value["on"], true);
    }
}
