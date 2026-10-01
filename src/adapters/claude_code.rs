//! Claude Code: `~/.claude/projects/<encoded-cwd>/<session>.jsonl`, plus
//! `<session>/subagents/agent-*.jsonl`. One JSON object per line, append-only.
//!
//! The format is undocumented. Parsing is deliberately lenient: unknown record types and
//! fields are ignored, and a line that doesn't parse is skipped, not fatal.

use serde_json::Value;

use crate::event::*;

pub const HARNESS: &str = "claude-code";

const REJECTED_MARKER: &str = "doesn't want to proceed with this tool use";
const DENIED_MARKER: &str = "Permission for this action was denied";
const INTERRUPTED_MARKER: &str = "[Request interrupted by user";

/// Parse one log line into zero or more events.
pub fn parse_line(line: &str) -> Vec<Event> {
    match serde_json::from_str::<Value>(line) {
        Ok(v) => parse_record(&v),
        Err(_) => Vec::new(),
    }
}

pub fn parse_record(v: &Value) -> Vec<Event> {
    let mut out = Vec::new();
    let Some(session_id) = str_at(v, "sessionId") else {
        return out;
    };
    let ts = str_at(v, "timestamp");
    let record_type = str_at(v, "type").unwrap_or_default();
    let is_sidechain = v
        .get("isSidechain")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if let Some(ts) = &ts {
        out.push(Event::Session(SessionInfo {
            id: session_id.clone(),
            ts: ts.clone(),
            cwd: str_at(v, "cwd"),
            git_branch: str_at(v, "gitBranch").filter(|b| !b.is_empty()),
            cli_version: str_at(v, "version"),
        }));
    }

    match record_type.as_str() {
        "ai-title" => {
            if let Some(title) = str_at(v, "aiTitle") {
                out.push(Event::Title { session_id, title });
            }
        }
        "assistant" => {
            if let Some(ts) = ts {
                parse_assistant(v, session_id, ts, is_sidechain, &mut out);
            }
        }
        "user" => {
            if let Some(ts) = ts {
                parse_user(v, session_id, ts, is_sidechain, &mut out);
            }
        }
        _ => {}
    }
    out
}

fn parse_assistant(
    v: &Value,
    session_id: String,
    ts: String,
    is_sidechain: bool,
    out: &mut Vec<Event>,
) {
    let Some(message) = v.get("message") else {
        return;
    };
    let request_id = str_at(v, "requestId").or_else(|| str_at(message, "id"));
    let model = str_at(message, "model").filter(|m| m != "<synthetic>");

    if let Some(request_id) = &request_id
        && model.is_some()
    {
        out.push(Event::Request(Request {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            ts: ts.clone(),
            model,
            usage: message.get("usage").map(parse_usage),
            skill: str_at(v, "attributionSkill"),
            effort: str_at(v, "effort"),
            turn_index: None,
            is_sidechain,
        }));
    }

    for block in content_blocks(message) {
        if str_at(block, "type").as_deref() != Some("tool_use") {
            continue;
        }
        let (Some(id), Some(tool)) = (str_at(block, "id"), str_at(block, "name")) else {
            continue;
        };
        let input = block.get("input").cloned().unwrap_or(Value::Null);
        let skill = if tool == "Skill" {
            str_at(&input, "skill")
        } else {
            None
        };
        out.push(Event::ToolCall(ToolCall {
            id,
            session_id: session_id.clone(),
            request_id: request_id.clone(),
            ts: ts.clone(),
            tool,
            skill,
            turn_index: None,
            input_json: input.to_string(),
        }));
    }
}

fn parse_usage(u: &Value) -> Usage {
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_i64).unwrap_or(0);
    let (w5, w1h) = match u.get("cache_creation") {
        Some(cc) => (
            n(cc, "ephemeral_5m_input_tokens"),
            n(cc, "ephemeral_1h_input_tokens"),
        ),
        // Older logs only have the total; price it as a 5-minute write.
        None => (n(u, "cache_creation_input_tokens"), 0),
    };
    Usage {
        input: n(u, "input_tokens"),
        output: n(u, "output_tokens"),
        cache_read: n(u, "cache_read_input_tokens"),
        cache_write_5m: w5,
        cache_write_1h: w1h,
    }
}

fn parse_user(v: &Value, session_id: String, ts: String, is_sidechain: bool, out: &mut Vec<Event>) {
    let Some(message) = v.get("message") else {
        return;
    };
    let id = str_at(v, "uuid").unwrap_or_else(|| format!("{session_id}:{ts}"));

    // Tool results: outcome of an earlier tool call.
    let mut had_tool_result = false;
    for block in content_blocks(message) {
        if str_at(block, "type").as_deref() != Some("tool_result") {
            continue;
        }
        had_tool_result = true;
        let Some(tool_use_id) = str_at(block, "tool_use_id") else {
            continue;
        };
        let text = result_text(block);
        let is_error = block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let outcome = if text.contains(REJECTED_MARKER) {
            Outcome::Rejected
        } else if text.contains(DENIED_MARKER) {
            Outcome::Denied
        } else if is_error {
            Outcome::Error
        } else {
            Outcome::Ok
        };
        let detail =
            matches!(outcome, Outcome::Rejected | Outcome::Denied).then(|| truncate(&text, 300));
        out.push(Event::ToolResult(ToolResult {
            tool_use_id,
            session_id: session_id.clone(),
            ts: ts.clone(),
            outcome,
            detail,
        }));
    }
    if had_tool_result || v.get("isMeta").and_then(Value::as_bool).unwrap_or(false) {
        return;
    }

    // Typed text: a prompt, a slash command, or an interruption marker.
    let text = user_text(message);
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if text.starts_with(INTERRUPTED_MARKER) {
        out.push(Event::Interrupted { id, session_id, ts });
        return;
    }
    if is_injected(text) {
        return;
    }
    let (kind, text) = match command_text(text) {
        Some(cmd) => (PromptKind::Command, cmd),
        None => (PromptKind::Prompt, text.to_string()),
    };
    out.push(Event::Prompt(Prompt {
        id,
        session_id,
        ts,
        kind,
        text,
        is_sidechain,
    }));
}

/// Text Claude Code writes into the log as a user message although you didn't type it:
/// output of local commands, their caveat, and background-task notifications.
fn is_injected(text: &str) -> bool {
    text.starts_with("<local-command-")
        || text.starts_with("Caveat: The messages below")
        || text.starts_with("<task-notification>")
}

/// `<command-name>/shipit</command-name> ... <command-args>x</command-args>` → `/shipit x`.
fn command_text(text: &str) -> Option<String> {
    let name = between(text, "<command-name>", "</command-name>")?;
    let args = between(text, "<command-args>", "</command-args>").unwrap_or("");
    let name = if name.starts_with('/') {
        name.to_string()
    } else {
        format!("/{name}")
    };
    Some(if args.trim().is_empty() {
        name
    } else {
        format!("{name} {}", args.trim())
    })
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = s.find(start)? + start.len();
    let to = s[from..].find(end)? + from;
    Some(s[from..to].trim())
}

/// The user's own words: text blocks, minus harness-injected reminders.
fn user_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| str_at(b, "type").as_deref() == Some("text"))
            .filter_map(|b| str_at(b, "text"))
            .filter(|t| !t.trim_start().starts_with("<system-reminder>"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| str_at(p, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn content_blocks(message: &Value) -> &[Value] {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn str_at(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Replace inlined base64 payloads (pasted images, PDFs) with a placeholder so raw lines
/// stay small. Returns true if anything was stripped.
pub fn strip_binary(v: &mut Value) -> bool {
    match v {
        Value::Object(map) => {
            let is_base64 = map
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|t| t == "base64")
                && map.contains_key("data");
            if is_base64 {
                map.insert("data".into(), Value::String("<stripped by calvin>".into()));
                return true;
            }
            let mut any = false;
            for child in map.values_mut() {
                any |= strip_binary(child);
            }
            any
        }
        Value::Array(items) => items.iter_mut().fold(false, |any, i| strip_binary(i) | any),
        _ => false,
    }
}

/// The conversation in one log file, as timeline events for the session browser.
pub fn timeline(records: &[Value]) -> Vec<crate::timeline::Event> {
    use crate::timeline::{Event, Item, preview};

    let mut out: Vec<Event> = Vec::new();
    // Where each tool call sits in `out`, to attach its result later.
    let mut tools: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for v in records {
        let Some(ts) = str_at(v, "timestamp") else {
            continue;
        };
        let Some(message) = v.get("message") else {
            continue;
        };
        match str_at(v, "type").as_deref() {
            Some("assistant") => {
                let request_id = str_at(v, "requestId").or_else(|| str_at(message, "id"));
                for block in content_blocks(message) {
                    match str_at(block, "type").as_deref() {
                        Some("text") => {
                            let text = str_at(block, "text").unwrap_or_default();
                            if !text.trim().is_empty() {
                                out.push(Event::Item(Item::Text {
                                    ts: ts.clone(),
                                    text,
                                    request_id: request_id.clone(),
                                }));
                            }
                        }
                        Some("tool_use") => {
                            let (Some(id), Some(name)) =
                                (str_at(block, "id"), str_at(block, "name"))
                            else {
                                continue;
                            };
                            let input = block.get("input").cloned().unwrap_or(Value::Null);
                            tools.insert(id.clone(), out.len());
                            out.push(Event::Item(Item::Tool {
                                ts: ts.clone(),
                                id,
                                summary: tool_summary(&name, &input),
                                name,
                                input: preview(
                                    &serde_json::to_string_pretty(&input).unwrap_or_default(),
                                ),
                                outcome: None,
                                result: None,
                                duration_ms: None,
                                request_id: request_id.clone(),
                            }));
                        }
                        _ => {}
                    }
                }
            }
            Some("user") => {
                let mut had_result = false;
                for block in content_blocks(message) {
                    if str_at(block, "type").as_deref() != Some("tool_result") {
                        continue;
                    }
                    had_result = true;
                    let Some(id) = str_at(block, "tool_use_id") else {
                        continue;
                    };
                    let Some(&index) = tools.get(&id) else {
                        continue;
                    };
                    let text = result_text(block);
                    let is_error = block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let outcome = if text.contains(REJECTED_MARKER) {
                        Outcome::Rejected
                    } else if text.contains(DENIED_MARKER) {
                        Outcome::Denied
                    } else if is_error {
                        Outcome::Error
                    } else {
                        Outcome::Ok
                    };
                    if let Event::Item(Item::Tool {
                        ts: started,
                        outcome: o,
                        result,
                        duration_ms,
                        ..
                    }) = &mut out[index]
                    {
                        *o = Some(outcome.as_str().to_string());
                        *result = Some(preview(&text));
                        *duration_ms = millis_between(started, &ts);
                    }
                }
                if had_result || v.get("isMeta").and_then(Value::as_bool).unwrap_or(false) {
                    continue;
                }
                let text = user_text(message);
                let text = text.trim();
                if text.is_empty() || is_injected(text) {
                    continue;
                }
                if text.starts_with(INTERRUPTED_MARKER) {
                    out.push(Event::Item(Item::Interrupted { ts }));
                    continue;
                }
                match command_text(text) {
                    Some(cmd) => out.push(Event::Prompt {
                        ts,
                        text: cmd,
                        kind: "command",
                        turn_index: None,
                    }),
                    None => out.push(Event::Prompt {
                        ts,
                        text: text.to_string(),
                        kind: "prompt",
                        turn_index: None,
                    }),
                }
            }
            _ => {}
        }
    }
    out
}

/// One line describing what a tool call did.
fn tool_summary(name: &str, input: &Value) -> String {
    let field = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_string);
    let text = match name {
        "Bash" | "PowerShell" => field("command"),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            field("file_path").or_else(|| field("notebook_path"))
        }
        "Grep" | "Glob" => field("pattern"),
        "Skill" => field("skill"),
        "WebFetch" => field("url"),
        "WebSearch" => field("query"),
        "Agent" | "Task" => field("description"),
        // The plan's first heading, or its first line.
        "ExitPlanMode" => field("plan").map(|p| {
            p.lines()
                .map(|l| l.trim().trim_start_matches('#').trim())
                .find(|l| !l.is_empty())
                .unwrap_or_default()
                .to_string()
        }),
        "AskUserQuestion" => input
            .get("questions")
            .and_then(|q| q.get(0))
            .and_then(|q| q.get("question"))
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    };
    let text = text.unwrap_or_else(|| {
        let compact = input.to_string();
        if compact == "{}" || compact == "null" {
            String::new()
        } else {
            compact
        }
    });
    truncate(&text.split_whitespace().collect::<Vec<_>>().join(" "), 160)
}

fn millis_between(start: &str, end: &str) -> Option<i64> {
    let a = chrono::DateTime::parse_from_rfc3339(start).ok()?;
    let b = chrono::DateTime::parse_from_rfc3339(end).ok()?;
    Some((b - a).num_milliseconds().max(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_command_is_normalised() {
        let t = "<command-message>shipit</command-message>\n<command-name>/shipit</command-name>\n<command-args>commit staged</command-args>";
        assert_eq!(command_text(t).as_deref(), Some("/shipit commit staged"));
    }

    #[test]
    fn strips_base64_images() {
        let mut v: Value = serde_json::json!({"content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}]});
        assert!(strip_binary(&mut v));
        assert_eq!(v["content"][0]["source"]["data"], "<stripped by calvin>");
    }

    #[test]
    fn garbage_line_is_ignored() {
        assert!(parse_line("{not json").is_empty());
        assert!(parse_line(r#"{"type":"mode","mode":"normal"}"#).is_empty());
    }
}
