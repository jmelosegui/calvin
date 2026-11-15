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
    ContextSample(ContextSample),
    ContextItem(ContextItem),
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
    /// The MCP server the tool belongs to, when it is an MCP tool.
    pub mcp_server: Option<String>,
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

/// What one model request carried besides the conversation, when the harness reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextSample {
    pub id: String,
    pub session_id: String,
    pub ts: String,
    /// Tools whose definitions were sent with the request.
    pub tool_count: Option<i64>,
    /// Tokens those tool definitions took.
    pub tool_tokens: Option<i64>,
    /// Every prompt token of the request, cached or not.
    pub prompt_tokens: Option<i64>,
    pub is_sidechain: bool,
}

/// Something a harness loaded into a session's context. Only names and sizes are kept,
/// never the content.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextItem {
    pub session_id: String,
    pub ts: String,
    pub kind: ContextKind,
    pub name: String,
    /// Size in characters, when the harness records the text.
    pub chars: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextKind {
    /// A tool offered to the model.
    Tool,
    /// An MCP server that failed to start.
    McpFailed,
    /// The list of skills the model can choose from.
    SkillListing,
    /// An instruction or memory file.
    Instructions,
}

impl ContextKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ContextKind::Tool => "tool",
            ContextKind::McpFailed => "mcp_failed",
            ContextKind::SkillListing => "skill_listing",
            ContextKind::Instructions => "instructions",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileTouched {
    pub session_id: String,
    pub path: String,
    pub tool: Option<String>,
    pub turn_index: Option<i64>,
    pub ts: String,
}
