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
                detail: None,
            })]
        }
        _ => Vec::new(),
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
}
