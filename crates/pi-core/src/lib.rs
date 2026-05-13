use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

pub type PiResult<T> = Result<T, PiError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiErrorKind {
    Config,
    Provider,
    PermissionDenied,
    Session,
    Tool,
    Io,
    InvalidInput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiError {
    pub kind: PiErrorKind,
    pub message: String,
}

impl PiError {
    pub fn new(kind: PiErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for PiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PiError {}

impl From<std::io::Error> for PiError {
    fn from(value: std::io::Error) -> Self {
        Self::new(PiErrorKind::Io, format!("文件系统错误：{value}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub timestamp_ms: u128,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            timestamp_ms: now_ms(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
}

impl Default for ModelSelection {
    fn default() -> Self {
        Self {
            provider: "echo".to_string(),
            model: "echo-local".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppConfig {
    pub model: ModelSelection,
    pub session_path: Option<String>,
    pub print_mode: bool,
    pub tools_enabled: bool,
    pub enabled_tool_names: Option<Vec<String>>,
    pub locale: Locale,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            model: ModelSelection::default(),
            session_path: None,
            print_mode: false,
            tools_enabled: true,
            enabled_tool_names: None,
            locale: Locale::ZhCn,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    ZhCn,
    En,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    UserMessage(String),
    AssistantDelta(String),
    AssistantMessage(String),
    ToolStarted { name: String },
    ToolFinished { name: String, output: String },
    PermissionDecision { capability: String, allowed: bool },
    SessionSaved { path: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_shape: String,
    pub mutates: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvocation {
    pub id: Option<String>,
    pub name: String,
    pub input: String,
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

pub fn escape_json_string(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
