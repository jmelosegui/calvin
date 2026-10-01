//! The normalised events every harness adapter produces.
//!
//! Adapters translate their own log format into these; the ingest step writes them to
//! SQLite. Anything a harness doesn't record is `None`.

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Seen on (almost) every record: keeps the session row and its time range current.
    Session(SessionInfo),
    Title {
        session_id: String,
        title: String,
    },
    Prompt(Prompt),
    AssistantMessage(AssistantMessage),
    Request(Request),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    FileTouched(FileTouched),
    Interrupted {
        id: String,
        session_id: String,
        ts: String,
    },
    ModeChanged {
        id: String,
        session_id: String,
        ts: String,
        mode: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub ts: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub cli_version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Prompt,
    Command,
}

impl PromptKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptKind::Prompt => "prompt",
            PromptKind::Command => "command",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Prompt {
    pub id: String,
    pub session_id: String,
    pub ts: String,
    pub kind: PromptKind,
    pub text: String,
    pub is_sidechain: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub request_id: String,
    pub session_id: String,
    pub ts: String,
    pub model: Option<String>,
    pub usage: Option<Usage>,
    pub skill: Option<String>,
    /// Reasoning effort the request ran at (`low` … `max`), if the harness records it.
    pub effort: Option<String>,
    pub turn_index: Option<i64>,
    pub duration_ms: Option<i64>,
    pub is_sidechain: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssistantMessage {
    pub id: String,
    pub session_id: String,
    pub request_id: Option<String>,
    pub ts: String,
    pub text: String,
    pub turn_index: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub session_id: String,
    pub request_id: Option<String>,
    pub ts: String,
    pub tool: String,
    /// For skill invocations, the skill's name.
    pub skill: Option<String>,
    pub turn_index: Option<i64>,
    pub input_json: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Error,
    Rejected,
    Denied,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Error => "error",
            Outcome::Rejected => "rejected",
            Outcome::Denied => "denied",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub session_id: String,
    pub ts: String,
    pub outcome: Outcome,
    /// First part of the result text, kept for rejected/denied results.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileTouched {
    pub session_id: String,
    pub path: String,
    pub tool: Option<String>,
    pub turn_index: Option<i64>,
    pub ts: String,
}
