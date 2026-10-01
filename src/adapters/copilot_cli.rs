//! GitHub Copilot CLI: `~/.copilot/session-state/<session>/events.jsonl`.
//!
//! The session store is imported separately because it is the authoritative source for
//! turns and usage. These events add tool execution and live session metadata.

use serde_json::Value;

use crate::event::*;

pub const HARNESS: &str = "copilot-cli";

pub fn parse_record(v: &Value, native_session_id: &str) -> Vec<Event> {
    let Some(kind) = v.get("type").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(data) = v.get("data") else {
        return Vec::new();
    };
    let ts = v
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    match kind {
        "session.start" => {
            let Some(id) = data.get("sessionId").and_then(Value::as_str) else {
                return Vec::new();
            };
            vec![Event::Session(SessionInfo {
                id: namespaced(id),
                ts: data
                    .get("startTime")
                    .and_then(Value::as_str)
                    .unwrap_or(&ts)
                    .to_string(),
                cwd: data
                    .pointer("/context/cwd")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                git_branch: data
                    .pointer("/context/gitBranch")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                cli_version: data
                    .get("copilotVersion")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })]
        }
        "tool.execution_start" => {
            let (Some(id), Some(tool)) = (
                data.get("toolCallId").and_then(Value::as_str),
                data.get("toolName").and_then(Value::as_str),
            ) else {
                return Vec::new();
            };
            vec![Event::ToolCall(ToolCall {
                id: namespaced(id),
                session_id: namespaced(native_session_id),
                request_id: data
                    .get("interactionId")
                    .and_then(Value::as_str)
                    .map(namespaced),
                ts,
                tool: tool.to_string(),
                skill: skill_name(tool, data.get("arguments")),
                turn_index: data.get("turnIndex").and_then(Value::as_i64),
                input_json: data
                    .get("arguments")
                    .cloned()
                    .unwrap_or(Value::Null)
                    .to_string(),
            })]
        }
        "tool.execution_complete" => {
            let Some(id) = data.get("toolCallId").and_then(Value::as_str) else {
                return Vec::new();
            };
            let outcome = if data.get("success").and_then(Value::as_bool) == Some(true) {
                Outcome::Ok
            } else {
                Outcome::Error
            };
            vec![Event::ToolResult(ToolResult {
                tool_use_id: namespaced(id),
                session_id: namespaced(native_session_id),
                ts,
                outcome,
                detail: copilot_result_detail(data),
            })]
        }
        "user.message" => {
            let Some(text) = data.get("content").and_then(Value::as_str) else {
                return Vec::new();
            };
            let text = text.trim();
            if !text.starts_with('/') {
                return Vec::new();
            }
            let id = v
                .get("id")
                .and_then(Value::as_str)
                .map(namespaced)
                .unwrap_or_else(|| format!("{}:command:{ts}", namespaced(native_session_id)));
            vec![Event::Prompt(Prompt {
                id,
                session_id: namespaced(native_session_id),
                ts,
                kind: PromptKind::Command,
                text: text.to_string(),
                is_sidechain: false,
            })]
        }
        "session.mode_changed" => {
            let Some(mode) = data.get("newMode").and_then(Value::as_str) else {
                return Vec::new();
            };
            let id = v
                .get("id")
                .and_then(Value::as_str)
                .map(namespaced)
                .unwrap_or_else(|| format!("{}:mode:{ts}", namespaced(native_session_id)));
            vec![Event::ModeChanged {
                id,
                session_id: namespaced(native_session_id),
                ts,
                mode: mode.to_string(),
            }]
        }
        _ => Vec::new(),
    }
}

fn copilot_result_detail(data: &Value) -> Option<String> {
    let value = data.get("error").or_else(|| data.get("result"))?;
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    if text.is_empty() {
        None
    } else {
        Some(text.chars().take(2_000).collect())
    }
}

fn skill_name(tool: &str, input: Option<&Value>) -> Option<String> {
    if !tool.eq_ignore_ascii_case("skill") {
        return None;
    }
    input?
        .get("skill")
        .or_else(|| input?.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub fn namespaced(id: &str) -> String {
    format!("{HARNESS}:{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tool_execution() {
        let v = serde_json::json!({
            "type": "tool.execution_start",
            "timestamp": "2026-09-30T12:00:00Z",
            "sessionId": "session-1",
            "data": {
                "toolCallId": "tool-1",
                "toolName": "powershell",
                "arguments": {"command": "cargo test"}
            }
        });
        let events = parse_record(&v, "session-1");
        let [Event::ToolCall(call)] = events.as_slice() else {
            panic!("expected a tool call");
        };
        assert_eq!(call.id, "copilot-cli:tool-1");
        assert_eq!(call.session_id, "copilot-cli:session-1");
    }

    #[test]
    fn imports_commands_but_not_normal_messages() {
        let command = serde_json::json!({
            "id": "event-command",
            "type": "user.message",
            "timestamp": "2026-09-30T12:00:00Z",
            "data": {"content": "/review"}
        });
        let events = parse_record(&command, "session-1");
        let [Event::Prompt(prompt)] = events.as_slice() else {
            panic!("expected a command");
        };
        assert_eq!(prompt.text, "/review");
        assert_eq!(prompt.kind, PromptKind::Command);

        let message = serde_json::json!({
            "type": "user.message",
            "timestamp": "2026-09-30T12:00:00Z",
            "data": {"content": "review this"}
        });
        assert!(parse_record(&message, "session-1").is_empty());
    }

    #[test]
    fn parses_mode_changes() {
        let v = serde_json::json!({
            "id": "event-mode",
            "type": "session.mode_changed",
            "timestamp": "2026-09-30T12:00:00Z",
            "data": {"previousMode": "interactive", "newMode": "plan"}
        });
        let events = parse_record(&v, "session-1");
        let [Event::ModeChanged { mode, .. }] = events.as_slice() else {
            panic!("expected a mode change");
        };
        assert_eq!(mode, "plan");
    }
}
