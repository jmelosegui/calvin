use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;

use crate::event::{
    AssistantMessage, Event, FileTouched, Outcome, Prompt, PromptKind, Request, SessionInfo,
    ToolCall, ToolResult, Usage,
};

pub const HARNESS: &str = "cursor";

pub fn namespace(id: &str) -> String {
    format!("{HARNESS}:{id}")
}

pub fn composer_id(value: &Value) -> Option<&str> {
    string(value, &["composerId", "id"])
}

pub fn updated_at(value: &Value) -> i64 {
    integer(value, &["lastUpdatedAt", "updatedAt"]).unwrap_or_default()
}

pub fn workspace_identifier(value: &Value) -> Option<&str> {
    string(value, &["workspaceIdentifier", "workspaceId"])
}

pub fn bubble_ids(value: &Value) -> Vec<String> {
    value
        .get("fullConversationHeadersOnly")
        .or_else(|| value.get("conversationHeaders"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.as_str()
                .or_else(|| string(item, &["bubbleId", "id"]))
                .map(str::to_string)
        })
        .collect()
}

pub fn events(
    composer: &Value,
    header: Option<&Value>,
    cwd: Option<String>,
    bubbles: &[Value],
) -> Vec<Event> {
    let Some(native_id) = composer_id(composer).or_else(|| header.and_then(composer_id)) else {
        return Vec::new();
    };
    let session_id = namespace(native_id);
    let started = timestamp(composer, &["createdAt"])
        .or_else(|| header.and_then(|h| timestamp(h, &["createdAt"])))
        .or_else(|| bubbles.iter().find_map(|b| timestamp(b, &["createdAt"])))
        .unwrap_or_else(now);
    let ended = timestamp(composer, &["lastUpdatedAt", "updatedAt"])
        .or_else(|| header.and_then(|h| timestamp(h, &["lastUpdatedAt", "updatedAt"])))
        .or_else(|| {
            bubbles
                .iter()
                .rev()
                .find_map(|b| timestamp(b, &["createdAt"]))
        })
        .unwrap_or_else(|| started.clone());

    let session = |ts: String| {
        Event::Session(SessionInfo {
            id: session_id.clone(),
            ts,
            cwd: cwd.clone(),
            git_branch: None,
            cli_version: None,
        })
    };
    let mut out = vec![session(started.clone())];
    if ended != started {
        out.push(session(ended));
    }

    if let Some(title) = string(composer, &["name", "title"])
        .or_else(|| header.and_then(|h| string(h, &["name", "title"])))
        .filter(|title| !title.trim().is_empty())
    {
        out.push(Event::Title {
            session_id: session_id.clone(),
            title: title.to_string(),
        });
    }

    if let Some(mode) = string(composer, &["unifiedMode", "mode"])
        .or_else(|| header.and_then(|h| string(h, &["unifiedMode", "mode"])))
        .map(normalize_mode)
    {
        out.push(Event::ModeChanged {
            id: format!("{session_id}:mode"),
            session_id: session_id.clone(),
            ts: started,
            mode,
        });
    }

    let mut turn_index = -1i64;
    let mut request_id: Option<String> = None;
    let mut request_has_usage = false;
    let composer_model = composer
        .get("modelConfig")
        .and_then(|value| string(value, &["modelName", "modelId"]))
        .or_else(|| {
            header
                .and_then(|value| value.get("modelConfig"))
                .and_then(|value| string(value, &["modelName", "modelId"]))
        })
        .map(str::to_string);
    for (position, bubble) in bubbles.iter().enumerate() {
        let bubble_id = string(bubble, &["bubbleId", "id"])
            .map(str::to_string)
            .unwrap_or_else(|| position.to_string());
        let ts = timestamp(bubble, &["createdAt"]).unwrap_or_else(now);
        match integer(bubble, &["type"]) {
            Some(1) => {
                turn_index += 1;
                let text = string(bubble, &["text"])
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or("");
                if !text.is_empty() {
                    out.push(Event::Prompt(Prompt {
                        id: format!("{session_id}:turn:{turn_index}"),
                        session_id: session_id.clone(),
                        ts: ts.clone(),
                        kind: if text.trim_start().starts_with('/') {
                            PromptKind::Command
                        } else {
                            PromptKind::Prompt
                        },
                        text: text.to_string(),
                        is_sidechain: false,
                    }));
                }
                let native_request = string(bubble, &["requestId"])
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{native_id}:{bubble_id}"));
                let namespaced_request = namespace(&native_request);
                request_id = Some(namespaced_request.clone());
                let usage = usage(bubble);
                request_has_usage = usage.is_some();
                let model = bubble
                    .get("modelInfo")
                    .and_then(|v| string(v, &["modelName", "modelId"]))
                    .or_else(|| string(bubble, &["modelName", "model"]))
                    .map(str::to_string)
                    .or_else(|| composer_model.clone());
                out.push(Event::Request(Request {
                    request_id: namespaced_request,
                    session_id: session_id.clone(),
                    ts,
                    model: model.clone(),
                    usage,
                    skill: None,
                    effort: model.as_deref().and_then(effort),
                    turn_index: Some(turn_index),
                    duration_ms: integer(bubble, &["turnDurationMs", "durationMs"]),
                    is_sidechain: false,
                }));
            }
            Some(2) => {
                let bubble_usage = usage(bubble);
                if let Some(usage) = bubble_usage {
                    let usage_request_id = if request_has_usage {
                        namespace(
                            string(bubble, &["usageUuid", "requestId"])
                                .filter(|id| !id.is_empty())
                                .unwrap_or(&bubble_id),
                        )
                    } else {
                        request_id
                            .clone()
                            .unwrap_or_else(|| namespace(&format!("{native_id}:{bubble_id}")))
                    };
                    request_has_usage = true;
                    out.push(Event::Request(Request {
                        request_id: usage_request_id.clone(),
                        session_id: session_id.clone(),
                        ts: ts.clone(),
                        model: bubble
                            .get("modelInfo")
                            .and_then(|v| string(v, &["modelName", "modelId"]))
                            .map(str::to_string)
                            .or_else(|| composer_model.clone()),
                        usage: Some(usage),
                        skill: None,
                        effort: composer_model.as_deref().and_then(effort),
                        turn_index: Some(turn_index.max(0)),
                        duration_ms: integer(bubble, &["turnDurationMs", "durationMs"]),
                        is_sidechain: false,
                    }));
                    request_id = Some(usage_request_id);
                }
                if let Some(tool_data) = bubble.get("toolFormerData")
                    && let Some(name) = string(tool_data, &["name"])
                {
                    tool_events(
                        &mut out,
                        &session_id,
                        &bubble_id,
                        &ts,
                        request_id.clone(),
                        turn_index.max(0),
                        name,
                        tool_data,
                    );
                    continue;
                }
                let capability = integer(bubble, &["capabilityType"]);
                if capability == Some(30) {
                    continue;
                }
                if let Some(text) = string(bubble, &["text"]).filter(|text| !text.trim().is_empty())
                {
                    out.push(Event::AssistantMessage(AssistantMessage {
                        id: format!("{session_id}:message:{bubble_id}"),
                        session_id: session_id.clone(),
                        request_id: request_id.clone(),
                        ts,
                        text: text.to_string(),
                        turn_index: Some(turn_index.max(0)),
                    }));
                }
            }
            _ => {}
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn tool_events(
    out: &mut Vec<Event>,
    session_id: &str,
    bubble_id: &str,
    ts: &str,
    request_id: Option<String>,
    turn_index: i64,
    name: &str,
    data: &Value,
) {
    let native_tool_id = string(data, &["toolCallId"])
        .filter(|id| !id.is_empty())
        .unwrap_or(bubble_id);
    let tool_id = namespace(native_tool_id);
    let input = data
        .get("params")
        .or_else(|| data.get("rawArgs"))
        .map(json_value)
        .unwrap_or(Value::Null);
    out.push(Event::ToolCall(ToolCall {
        id: tool_id.clone(),
        session_id: session_id.to_string(),
        request_id,
        ts: ts.to_string(),
        tool: name.to_string(),
        skill: None,
        turn_index: Some(turn_index),
        input_json: serde_json::to_string(&input).unwrap_or_else(|_| "null".to_string()),
    }));

    if edits_files(name) {
        let mut paths = Vec::new();
        collect_paths(&input, &mut paths);
        paths.sort();
        paths.dedup();
        for path in paths {
            out.push(Event::FileTouched(FileTouched {
                session_id: session_id.to_string(),
                path,
                tool: Some(name.to_string()),
                turn_index: Some(turn_index),
                ts: ts.to_string(),
            }));
        }
    }

    let outcome = match string(data, &["status"]).unwrap_or("") {
        "completed" | "success" => Some(Outcome::Ok),
        "error" | "failed" => Some(Outcome::Error),
        "cancelled" | "canceled" => Some(Outcome::Rejected),
        _ => match string(data, &["userDecision"]).unwrap_or("") {
            "rejected" | "denied" => Some(Outcome::Rejected),
            _ => None,
        },
    };
    if let Some(outcome) = outcome {
        let detail = data
            .get("error")
            .or_else(|| data.get("result"))
            .and_then(value_text)
            .map(|text| text.chars().take(2_000).collect());
        out.push(Event::ToolResult(ToolResult {
            tool_use_id: tool_id,
            session_id: session_id.to_string(),
            ts: ts.to_string(),
            outcome,
            detail,
        }));
    }
}

fn usage(value: &Value) -> Option<Usage> {
    let token_count = value.get("tokenCount")?;
    let input = integer(token_count, &["inputTokens", "input"]).unwrap_or_default();
    let output = integer(token_count, &["outputTokens", "output"]).unwrap_or_default();
    (input != 0 || output != 0).then_some(Usage {
        input,
        output,
        ..Usage::default()
    })
}

fn effort(model: &str) -> Option<String> {
    ["xhigh", "high", "medium", "low"]
        .into_iter()
        .find(|level| model.to_ascii_lowercase().contains(level))
        .map(str::to_string)
}

fn normalize_mode(mode: &str) -> String {
    match mode.to_ascii_lowercase().as_str() {
        "agent" | "edit" => "agent".to_string(),
        "plan" => "plan".to_string(),
        "ask" | "chat" => "ask".to_string(),
        other => other.to_string(),
    }
}

fn edits_files(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "search_replace",
        "edit_file",
        "apply_patch",
        "write",
        "delete_file",
        "create_file",
        "multi_edit",
    ]
    .iter()
    .any(|part| name.contains(part))
}

fn collect_paths(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "relativeWorkspacePath"
                        | "targetFile"
                        | "path"
                        | "filePath"
                        | "targetDirectory"
                        | "directoryPath"
                ) && let Some(path) = value.as_str()
                    && !path.trim().is_empty()
                {
                    out.push(path.to_string());
                }
                collect_paths(value, out);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_paths(value, out);
            }
        }
        _ => {}
    }
}

fn timestamp(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(value) = value.get(*key) else {
            continue;
        };
        if let Some(text) = value.as_str() {
            if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
                return Some(parsed.with_timezone(&Utc).to_rfc3339());
            }
            if let Ok(number) = text.parse::<i64>() {
                return millis_timestamp(number);
            }
        } else if let Some(number) = value.as_i64() {
            return millis_timestamp(number);
        } else if let Some(number) = value.as_f64() {
            return millis_timestamp(number as i64);
        }
    }
    None
}

fn millis_timestamp(value: i64) -> Option<String> {
    let millis = if value < 10_000_000_000 {
        value.saturating_mul(1_000)
    } else {
        value
    };
    Utc.timestamp_millis_opt(millis)
        .single()
        .map(|ts| ts.to_rfc3339())
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

fn string<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
}

fn integer(value: &Value, keys: &[&str]) -> Option<i64> {
    keys.iter().find_map(|key| {
        let value = value.get(*key)?;
        value
            .as_i64()
            .or_else(|| value.as_f64().map(|number| number as i64))
            .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
    })
}

fn value_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| serde_json::to_string(value).ok())
}

fn json_value(value: &Value) -> Value {
    value
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_else(|| value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_cursor_conversation() {
        let composer = json!({
            "composerId": "thread-1",
            "name": "Add Cursor support",
            "createdAt": 1_700_000_000_000_i64,
            "lastUpdatedAt": 1_700_000_005_000_i64,
            "unifiedMode": "agent"
        });
        let bubbles = vec![
            json!({
                "bubbleId": "user-1", "type": 1, "createdAt": "2023-11-14T22:13:20Z",
                "requestId": "request-1", "text": "Update src/main.rs",
                "modelInfo": {"modelName": "claude-4.5-sonnet-thinking"},
                "tokenCount": {"inputTokens": 120, "outputTokens": 15}
            }),
            json!({
                "bubbleId": "tool-1", "type": 2, "createdAt": "2023-11-14T22:13:21Z",
                "capabilityType": 15,
                "toolFormerData": {
                    "toolCallId": "call-1", "name": "search_replace", "status": "completed",
                    "params": {"relativeWorkspacePath": "src/main.rs"}, "result": "done"
                }
            }),
            json!({
                "bubbleId": "answer-1", "type": 2, "createdAt": "2023-11-14T22:13:22Z",
                "text": "Updated the file."
            })
        ];
        let events = events(&composer, None, Some("C:\\work\\calvin".into()), &bubbles);
        assert!(events.iter().any(|event| matches!(
            event,
            Event::Request(request)
                if request.model.as_deref() == Some("claude-4.5-sonnet-thinking")
                    && request.usage.as_ref().is_some_and(|usage| usage.input == 120)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::FileTouched(file) if file.path == "src/main.rs"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            Event::AssistantMessage(message) if message.text == "Updated the file."
        )));
    }
}
