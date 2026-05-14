use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub mod auth_guidance;
pub mod error_hints;
pub mod telemetry;
pub mod timings;
pub use auth_guidance::{for_provider as auth_guidance_for, AuthGuidance};
pub use error_hints::{hint_for, ErrorHint};
pub use telemetry::{flush as flush_telemetry, record as record_telemetry, TelemetryEvent};

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
    Network,
    Cancelled,
    NotFound,
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

impl From<serde_json::Error> for PiError {
    fn from(value: serde_json::Error) -> Self {
        Self::new(PiErrorKind::InvalidInput, format!("JSON 解析错误：{value}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default = "now_ms")]
    pub timestamp_ms: u128,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolInvocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional image attachments (provider-side multimodal input). Each
    /// attachment is referenced by the provider mapping when supported and
    /// silently ignored when not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// Multimodal attachment that travels with a message. We keep the payload as
/// base64-encoded bytes plus a MIME type so the wire mapping is trivial for
/// every provider. Helpers in `pi_core::attachment` produce them from file
/// paths, http URLs, or raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub mime_type: String,
    pub kind: AttachmentKind,
    pub data: AttachmentData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentKind {
    Image,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachmentData {
    /// Base64-encoded bytes inline in the message.
    Base64 { data: String },
    /// External URL the provider must fetch itself.
    Url { url: String },
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            timestamp_ms: now_ms(),
            tool_call_id: None,
            tool_calls: Vec::new(),
            name: None,
            attachments: Vec::new(),
        }
    }

    pub fn with_attachment(mut self, attachment: Attachment) -> Self {
        self.attachments.push(attachment);
        self
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }

    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        tool_calls: Vec<ToolInvocation>,
    ) -> Self {
        let mut message = Self::new(Role::Assistant, content);
        message.tool_calls = tool_calls;
        message
    }

    pub fn tool_result(tool_call_id: Option<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            timestamp_ms: now_ms(),
            tool_call_id,
            tool_calls: Vec::new(),
            name: None,
            attachments: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    pub model: ModelSelection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_path: Option<String>,
    #[serde(default)]
    pub print_mode: bool,
    #[serde(default = "default_true")]
    pub tools_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_tool_names: Option<Vec<String>>,
    #[serde(default)]
    pub locale: Locale,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default = "default_max_steps")]
    pub max_tool_steps: u32,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub permission_mode: PermissionModeKind,
    #[serde(default = "default_context_window")]
    pub context_window_tokens: u32,
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f32,
    #[serde(default)]
    pub thinking_level: ThinkingLevel,
}

/// Reasoning budget for providers that support extended thinking (Anthropic
/// extended thinking, OpenAI o-series). Maps to provider-specific knobs in
/// `pi-providers`; absent or `None` means "let the model decide".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    None,
    Low,
    Medium,
    High,
}

impl ThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingLevel::None => "none",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "none" | "off" | "" => Some(Self::None),
            "low" => Some(Self::Low),
            "medium" | "med" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_max_steps() -> u32 {
    16
}

fn default_context_window() -> u32 {
    128_000
}

fn default_compaction_threshold() -> f32 {
    0.85
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
            system_prompt: None,
            max_tool_steps: default_max_steps(),
            stream: false,
            permission_mode: PermissionModeKind::default(),
            context_window_tokens: default_context_window(),
            compaction_threshold: default_compaction_threshold(),
            thinking_level: ThinkingLevel::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Locale {
    ZhCn,
    En,
}

impl Default for Locale {
    fn default() -> Self {
        Self::ZhCn
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionModeKind {
    ReadOnly,
    ConfirmMutations,
    TrustedWorkspace,
    Plan,
}

impl Default for PermissionModeKind {
    fn default() -> Self {
        Self::ConfirmMutations
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    UserMessage(String),
    AssistantDelta(String),
    AssistantMessage(String),
    ThinkingDelta(String),
    ProviderStream(StreamEvent),
    ToolStarted { name: String, input: String },
    ToolProgress { name: String, line: String },
    ToolFinished { name: String, output: String },
    ToolError { name: String, error: String },
    PermissionDecision { capability: String, allowed: bool },
    SessionSaved { path: String },
    Usage(Usage),
    Compacted { before: u32, after: u32 },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    #[serde(default)]
    pub cache_read_tokens: u32,
    #[serde(default)]
    pub cache_write_tokens: u32,
}

impl Usage {
    pub fn merge(&mut self, other: &Usage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamEvent {
    MessageStart,
    TextDelta(String),
    ThinkingDelta(String),
    ToolCallDelta {
        id: Option<String>,
        name: Option<String>,
        input_delta: String,
    },
    UsageDelta(Usage),
    MessageDone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// Free-form description of the input format. Maintained for backward
    /// compatibility with simple textual prompts; richer schemas live in
    /// `parameters`.
    #[serde(default)]
    pub input_shape: String,
    /// JSON Schema fragment describing the function's parameters object.
    /// Defaults to a single string `input` for backward-compatible tools.
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
    #[serde(default)]
    pub mutates: bool,
}

impl ToolSchema {
    pub fn parameters_or_default(&self) -> serde_json::Value {
        self.parameters
            .clone()
            .unwrap_or_else(|| default_input_schema(&self.input_shape))
    }
}

pub fn default_input_schema(input_shape: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "input": {
                "type": "string",
                "description": format!("Tool input. Expected format: {input_shape}"),
            }
        },
        "required": ["input"],
        "additionalProperties": false,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInvocation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
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

/// Legacy ad-hoc JSON string escaper kept for hand-rolled JSON in tests and
/// rare hot paths. New code should prefer `serde_json::to_string`.
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

/// Minimal base64 encoder used for attaching inline images without pulling
/// the `base64` crate. RFC 4648 standard alphabet, no line wrapping.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let chunk =
            ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(ALPHABET[((chunk >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(chunk & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let chunk = (bytes[i] as u32) << 16;
        out.push(ALPHABET[((chunk >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let chunk = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(ALPHABET[((chunk >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((chunk >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}

impl Attachment {
    pub fn image_from_bytes(mime_type: impl Into<String>, bytes: &[u8]) -> Self {
        Self {
            mime_type: mime_type.into(),
            kind: AttachmentKind::Image,
            data: AttachmentData::Base64 {
                data: base64_encode(bytes),
            },
        }
    }

    pub fn image_from_path(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path)?;
        let mime = guess_mime(path);
        Ok(Self::image_from_bytes(mime, &bytes))
    }

    pub fn image_url(url: impl Into<String>) -> Self {
        Self {
            mime_type: "image/*".to_string(),
            kind: AttachmentKind::Image,
            data: AttachmentData::Url { url: url.into() },
        }
    }

    pub fn data_url(&self) -> Option<String> {
        match &self.data {
            AttachmentData::Base64 { data } => {
                Some(format!("data:{};base64,{data}", self.mime_type))
            }
            AttachmentData::Url { url } => Some(url.clone()),
        }
    }
}

fn guess_mime(path: &std::path::Path) -> String {
    match path.extension().and_then(|s| s.to_str()) {
        Some("png") => "image/png".to_string(),
        Some("jpg") | Some("jpeg") => "image/jpeg".to_string(),
        Some("gif") => "image/gif".to_string(),
        Some("webp") => "image/webp".to_string(),
        Some("bmp") => "image/bmp".to_string(),
        Some("svg") => "image/svg+xml".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

/// A streaming sink the provider invokes with `StreamEvent`s as they arrive.
/// The agent loop adapts these into `Event::ProviderStream` / `Event::AssistantDelta`
/// for the CLI, TUI, RPC and SDK consumers.
pub trait StreamSink {
    fn emit(&mut self, event: StreamEvent) -> PiResult<()>;
    fn cancelled(&self) -> bool {
        false
    }
}

/// Convenience sink that records events into a vector. Useful in tests and the
/// default `Provider::stream` implementation.
#[derive(Debug, Default)]
pub struct VecSink {
    pub events: Vec<StreamEvent>,
}

impl StreamSink for VecSink {
    fn emit(&mut self, event: StreamEvent) -> PiResult<()> {
        self.events.push(event);
        Ok(())
    }
}

/// Rough token estimate used for context-window pressure heuristics. The TS pi
/// also uses character-based estimates for non-Anthropic providers. We slightly
/// overestimate to leave a safety margin.
pub fn estimate_tokens(text: &str) -> u32 {
    // 1 token ≈ 4 chars for English; 1 token ≈ 1.5 chars for CJK. Take the
    // worst-case of the two for safety.
    let chars = text.chars().count();
    let by_chars = (chars + 3) / 4;
    let by_cjk = (chars * 2 + 2) / 3;
    by_chars.max(by_cjk) as u32
}

pub fn estimate_messages_tokens(messages: &[Message]) -> u32 {
    messages
        .iter()
        .map(|message| estimate_tokens(&message.content) + 4)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trip_through_serde() {
        let mut message = Message::new(Role::Assistant, "你好");
        message.tool_call_id = Some("call-1".to_string());
        let encoded = serde_json::to_string(&message).expect("serialize");
        let decoded: Message = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded.role, Role::Assistant);
        assert_eq!(decoded.content, "你好");
        assert_eq!(decoded.tool_call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn estimate_tokens_handles_cjk() {
        assert!(estimate_tokens("你好世界") >= 2);
        assert!(estimate_tokens("hello world") >= 2);
    }

    #[test]
    fn base64_encodes_round_trip_to_known_value() {
        assert_eq!(base64_encode(b"hi"), "aGk=");
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn attachment_image_from_bytes_emits_data_url() {
        let attachment = Attachment::image_from_bytes("image/png", &[1u8, 2, 3]);
        let url = attachment.data_url().expect("data url");
        assert!(url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn tool_schema_default_parameters_describe_input_field() {
        let schema = ToolSchema {
            name: "ls".to_string(),
            description: "list".to_string(),
            input_shape: "path".to_string(),
            parameters: None,
            mutates: false,
        };
        let params = schema.parameters_or_default();
        assert_eq!(params["properties"]["input"]["type"], "string");
        assert_eq!(params["required"][0], "input");
    }
}
