//! Provider trait and built-in implementations.
//!
//! Design:
//! - `Provider` exposes one synchronous `complete` for non-streaming callers
//!   and a `stream` method that drives a [`StreamSink`] with incremental
//!   `StreamEvent`s. The default `stream` falls back to invoking `complete`
//!   and replaying its captured events; real providers (OpenAI-compatible,
//!   Anthropic, Ollama, Gemini) override it with a true SSE pump.
//! - JSON request/response shapes live in private `wire` modules and use
//!   serde. The legacy hand-rolled escaper is kept only as a fallback for
//!   the Ollama provider's `application/x-ndjson` stream lines, which is
//!   already strict JSON per chunk.
//! - HTTP transport is `ureq` with native-tls. We deliberately avoid pulling
//!   tokio so the agent loop stays synchronous and the binary stays small.

use std::collections::BTreeMap;
use std::env;
use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use pi_core::{
    Message, ModelSelection, PiError, PiErrorKind, PiResult, Role, StreamEvent, StreamSink,
    ToolInvocation, ToolSchema, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub mod aliases;
pub mod anthropic;
pub mod aws_event_stream;
pub mod azure;
pub mod bedrock;
pub mod cloudflare;
pub mod copilot;
pub mod faux;
pub mod gemini;
pub mod ollama;
pub mod openai;
pub mod openai_codex_responses;
pub mod openai_responses;
pub mod probe;
pub mod sigv4;
pub mod vertex;

pub use aliases::{resolve_alias, ResolvedSelection};
pub use azure::AzureOpenAiProvider;
pub use bedrock::BedrockProvider;
pub use cloudflare::CloudflareProvider;
pub use copilot::CopilotProvider;
pub use faux::{FauxProvider, FauxTurn};
pub use openai_codex_responses::OpenAiCodexResponsesProvider;
pub use openai_responses::OpenAiResponsesProvider;
pub use probe::{probe_all, ProbeOutcome, ProbeReport};
pub use vertex::VertexProvider;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderInfo {
    pub id: String,
    pub display_name: String,
    pub default_model: String,
    pub supported_models: Vec<String>,
    pub local_first: bool,
    pub requires_api_key_env: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRequest {
    pub model: ModelSelection,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub system_prompt: Option<String>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub stream: bool,
}

impl ProviderRequest {
    pub fn new(model: ModelSelection, messages: Vec<Message>) -> Self {
        Self {
            model,
            messages,
            tools: Vec::new(),
            system_prompt: None,
            max_output_tokens: None,
            temperature: None,
            stream: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderResponse {
    pub message: Message,
    pub events: Vec<String>,
    pub stream_events: Vec<StreamEvent>,
    pub tool_calls: Vec<ToolInvocation>,
    pub usage: Usage,
}

pub trait Provider: Send + Sync {
    fn info(&self) -> ProviderInfo;

    /// Non-streaming completion. Implementations should still populate
    /// `stream_events` with a synthetic sequence (`MessageStart`, `TextDelta`,
    /// `MessageDone`) so consumers always see the same event shape.
    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse>;

    /// Streaming completion. Default replays `complete()` through the sink so
    /// providers can opt-in to real SSE incrementally without breaking the
    /// trait. Override this when SSE is actually wired.
    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        let mut request = request;
        request.stream = false;
        let response = self.complete(request)?;
        for event in &response.stream_events {
            if sink.cancelled() {
                sink.emit(StreamEvent::MessageDone)?;
                return Err(PiError::new(PiErrorKind::Cancelled, "请求已被中断"));
            }
            sink.emit(event.clone())?;
        }
        Ok(response)
    }
}

#[derive(Debug, Default)]
pub struct EchoProvider;

impl Provider for EchoProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            id: "echo".to_string(),
            display_name: "本地 Echo Provider".to_string(),
            default_model: "echo-local".to_string(),
            supported_models: vec!["echo-local".to_string()],
            local_first: true,
            requires_api_key_env: None,
        }
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        if let Some(tool_message) = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::Tool)
        {
            let content = format!("工具结果已返回：{}", tool_message.content);
            let stream_events = text_stream_events(&content);
            return Ok(ProviderResponse {
                message: Message::new(Role::Assistant, content.clone()),
                events: vec![content],
                stream_events,
                tool_calls: Vec::new(),
                usage: Usage::default(),
            });
        }

        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .map(|message| message.content.as_str())
            .unwrap_or("");

        if let Some(call) = parse_echo_tool_call(last_user, &request.tools) {
            return Ok(ProviderResponse {
                message: Message::new(Role::Assistant, String::new()),
                events: Vec::new(),
                stream_events: tool_call_stream_events(std::slice::from_ref(&call)),
                tool_calls: vec![call],
                usage: Usage::default(),
            });
        }

        let content = format!("这是 Pi Rust 的本地响应。已收到你的请求：{last_user}");
        let stream_events = text_stream_events(&content);

        Ok(ProviderResponse {
            message: Message::new(Role::Assistant, content.clone()),
            events: vec![content],
            stream_events,
            tool_calls: Vec::new(),
            usage: Usage::default(),
        })
    }
}

pub use self::anthropic::AnthropicProvider;
pub use self::gemini::GeminiProvider;
pub use self::ollama::OllamaProvider;
pub use self::openai::{OpenAiCompatibleProvider, OpenAiProvider};

fn parse_echo_tool_call(prompt: &str, tools: &[ToolSchema]) -> Option<ToolInvocation> {
    let rest = prompt.strip_prefix("CALL_TOOL ")?;
    let (name, input) = rest.split_once(' ')?;
    if !tools.iter().any(|tool| tool.name == name) {
        return None;
    }

    Some(ToolInvocation {
        id: Some(format!("echo-{name}")),
        name: name.to_string(),
        input: input.to_string(),
    })
}

pub fn text_stream_events(content: &str) -> Vec<StreamEvent> {
    if content.is_empty() {
        return Vec::new();
    }

    vec![
        StreamEvent::MessageStart,
        StreamEvent::TextDelta(content.to_string()),
        StreamEvent::MessageDone,
    ]
}

pub fn tool_call_stream_events(calls: &[ToolInvocation]) -> Vec<StreamEvent> {
    let mut events = vec![StreamEvent::MessageStart];
    for call in calls {
        events.push(StreamEvent::ToolCallDelta {
            id: call.id.clone(),
            name: Some(call.name.clone()),
            input_delta: call.input.clone(),
        });
    }
    events.push(StreamEvent::MessageDone);
    events
}

#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, ProviderInfo>,
}

impl ProviderRegistry {
    pub fn builtin() -> Self {
        let mut registry = Self {
            providers: BTreeMap::new(),
        };
        registry.register(EchoProvider.info());
        registry.register(ollama::ollama_info());
        registry.register(openai::moonshot_info());
        registry.register(openai::deepseek_info());
        registry.register(openai::qwen_info());
        registry.register(openai::zhipu_info());
        registry.register(openai::minimax_info());
        registry.register(openai::openai_info());
        registry.register(anthropic::anthropic_info());
        registry.register(bedrock::bedrock_info());
        registry.register(gemini::gemini_info());
        registry.register(azure::azure_openai_info());
        registry.register(cloudflare::cloudflare_info());
        registry.register(copilot::copilot_info());
        registry.register(openai::openrouter_info());
        registry.register(openai::mistral_info());
        registry.register(openai_responses::openai_responses_info());
        registry.register(openai_codex_responses::openai_codex_responses_info());
        registry.register(vertex::vertex_info());
        registry
    }

    pub fn register(&mut self, info: ProviderInfo) {
        self.providers.insert(info.id.clone(), info);
    }

    pub fn list(&self) -> impl Iterator<Item = &ProviderInfo> {
        self.providers.values()
    }

    pub fn get(&self, id: &str) -> Option<&ProviderInfo> {
        self.providers.get(id)
    }

    pub fn require(&self, id: &str) -> PiResult<&ProviderInfo> {
        self.get(id).ok_or_else(|| {
            PiError::new(
                PiErrorKind::Provider,
                format!("未知 provider：{id}。请运行 `pi --list-providers` 查看可用项。"),
            )
        })
    }
}

use std::sync::{Arc, RwLock};

static TEST_PROVIDERS: once_cell_shim::Lazy<RwLock<Vec<(String, Arc<dyn Provider>)>>> =
    once_cell_shim::Lazy::new(|| RwLock::new(Vec::new()));

/// Register a fully constructed provider under a custom id so the agent can
/// pick it up via `provider_for`. Used by the harness mode (`FauxProvider`).
/// Returns a guard that automatically deregisters when dropped.
pub fn register_test_provider(
    id: impl Into<String>,
    provider: Arc<dyn Provider>,
) -> TestProviderGuard {
    let id = id.into();
    if let Ok(mut list) = TEST_PROVIDERS.write() {
        list.retain(|(existing, _)| existing != &id);
        list.push((id.clone(), provider));
    }
    TestProviderGuard { id }
}

pub struct TestProviderGuard {
    id: String,
}

impl Drop for TestProviderGuard {
    fn drop(&mut self) {
        if let Ok(mut list) = TEST_PROVIDERS.write() {
            list.retain(|(existing, _)| existing != &self.id);
        }
    }
}

fn lookup_test_provider(id: &str) -> Option<Arc<dyn Provider>> {
    TEST_PROVIDERS.read().ok().and_then(|list| {
        list.iter()
            .find(|(existing, _)| existing == id)
            .map(|(_, provider)| provider.clone())
    })
}

pub fn provider_for(selection: &ModelSelection) -> PiResult<Box<dyn Provider>> {
    if let Some(test) = lookup_test_provider(&selection.provider) {
        return Ok(Box::new(SharedProvider(test)));
    }
    match selection.provider.as_str() {
        "echo" => Ok(Box::new(EchoProvider)),
        "ollama" => Ok(Box::new(OllamaProvider)),
        "moonshot" => Ok(Box::new(OpenAiCompatibleProvider::moonshot())),
        "deepseek" => Ok(Box::new(OpenAiCompatibleProvider::deepseek())),
        "qwen" => Ok(Box::new(OpenAiCompatibleProvider::qwen())),
        "zhipu" => Ok(Box::new(OpenAiCompatibleProvider::zhipu())),
        "minimax" => Ok(Box::new(OpenAiCompatibleProvider::minimax())),
        "openai" => Ok(Box::new(OpenAiProvider::new())),
        "anthropic" => Ok(Box::new(AnthropicProvider::new())),
        "bedrock" => Ok(Box::new(BedrockProvider::new())),
        "gemini" => Ok(Box::new(GeminiProvider::new())),
        "azure" => Ok(Box::new(AzureOpenAiProvider::new())),
        "cloudflare" => Ok(Box::new(CloudflareProvider::new())),
        "copilot" => Ok(Box::new(CopilotProvider::new())),
        "openrouter" => Ok(Box::new(OpenAiCompatibleProvider::openrouter())),
        "mistral" => Ok(Box::new(OpenAiCompatibleProvider::mistral())),
        "openai-responses" => Ok(Box::new(OpenAiResponsesProvider::new())),
        "vertex" => Ok(Box::new(VertexProvider::new())),
        other => Err(PiError::new(
            PiErrorKind::Provider,
            format!("provider `{other}` 暂未实现执行路径"),
        )),
    }
}

struct SharedProvider(Arc<dyn Provider>);

impl Provider for SharedProvider {
    fn info(&self) -> ProviderInfo {
        self.0.info()
    }
    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        self.0.complete(request)
    }
    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        self.0.stream(request, sink)
    }
}

mod once_cell_shim {
    use std::sync::OnceLock;
    pub struct Lazy<T>(OnceLock<T>, fn() -> T);
    impl<T> Lazy<T> {
        pub const fn new(init: fn() -> T) -> Self {
            Self(OnceLock::new(), init)
        }
    }
    impl<T> std::ops::Deref for Lazy<T> {
        type Target = T;
        fn deref(&self) -> &T {
            self.0.get_or_init(self.1)
        }
    }
}

// ----- Shared HTTP plumbing ---------------------------------------------------

pub(crate) fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout_read(Duration::from_secs(300))
        .timeout_write(Duration::from_secs(60))
        .user_agent(concat!("pi-rust/", env!("CARGO_PKG_VERSION")))
        .build()
}

pub(crate) fn read_api_key(env_name: &str, provider_id: &str) -> PiResult<String> {
    // 1) Process env variable (power-user override, no I/O).
    if let Ok(value) = env::var(env_name) {
        if !value.is_empty() {
            return Ok(value);
        }
    }
    // 2) pi-auth layered resolver (encrypted file + optional keyring).
    use pi_auth::Resolver as _;
    if let Ok(resolver) = pi_auth::layered_resolver() {
        if let Ok(Some(value)) = resolver.lookup(provider_id, env_name) {
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    Err(PiError::new(
        PiErrorKind::Provider,
        format!(
            "缺少凭证 {env_name}。设置环境变量、或运行 `pi auth set {provider_id}` 写入加密存储。"
        ),
    ))
}

pub(crate) fn post_json(
    agent: &ureq::Agent,
    url: &str,
    body: &Value,
    headers: &[(&str, &str)],
) -> PiResult<Value> {
    let mut req = agent.post(url).set("content-type", "application/json");
    for (key, value) in headers {
        req = req.set(key, value);
    }
    let response = match req.send_json(body) {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let body = response.into_string().unwrap_or_default();
            return Err(PiError::new(
                PiErrorKind::Provider,
                format!("HTTP {status}：{body}"),
            ));
        }
        Err(ureq::Error::Transport(transport)) => {
            return Err(PiError::new(
                PiErrorKind::Network,
                format!("HTTP 传输错误：{transport}"),
            ));
        }
    };
    let text = response
        .into_string()
        .map_err(|err| PiError::new(PiErrorKind::Network, format!("读取 HTTP 响应失败：{err}")))?;
    serde_json::from_str(&text).map_err(|err| {
        PiError::new(
            PiErrorKind::Provider,
            format!("响应不是有效 JSON：{err}; body={text}"),
        )
    })
}

pub(crate) fn post_sse_lines<F>(
    agent: &ureq::Agent,
    url: &str,
    body: &Value,
    headers: &[(&str, &str)],
    mut on_line: F,
) -> PiResult<()>
where
    F: FnMut(&str) -> PiResult<()>,
{
    let mut req = agent
        .post(url)
        .set("content-type", "application/json")
        .set("accept", "text/event-stream");
    for (key, value) in headers {
        req = req.set(key, value);
    }
    let response = match req.send_json(body) {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let body = response.into_string().unwrap_or_default();
            return Err(PiError::new(
                PiErrorKind::Provider,
                format!("HTTP {status}：{body}"),
            ));
        }
        Err(ureq::Error::Transport(transport)) => {
            return Err(PiError::new(
                PiErrorKind::Network,
                format!("HTTP 传输错误：{transport}"),
            ));
        }
    };
    read_sse(response.into_reader(), |event| on_line(event))
}

/// Exposed for benchmarks. Use `read_sse` privately in providers; this is a
/// thin wrapper to avoid leaking `pub(crate)` to crates outside the workspace.
pub fn read_sse_for_bench<R, F>(reader: R, on_event: F) -> PiResult<()>
where
    R: Read,
    F: FnMut(&str) -> PiResult<()>,
{
    read_sse(reader, on_event)
}

pub(crate) fn read_sse<R, F>(reader: R, mut on_event: F) -> PiResult<()>
where
    R: Read,
    F: FnMut(&str) -> PiResult<()>,
{
    let mut buf = BufReader::new(reader);
    let mut data = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        let read = buf.read_line(&mut line)?;
        if read == 0 {
            if !data.is_empty() {
                on_event(data.trim_end())?;
                data.clear();
            }
            return Ok(());
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            if !data.is_empty() {
                let payload = data.trim_end_matches('\n').to_string();
                data.clear();
                on_event(&payload)?;
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest);
        } else if trimmed.starts_with(':') {
            // SSE comment, skip
            continue;
        }
        // Other prefixes (`event:`, `id:`) are unused for our streams.
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OpenAiChatMessage {
    pub role: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub content: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<OpenAiToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OpenAiToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(rename = "type", default = "default_function_type")]
    pub kind: String,
    pub function: OpenAiFunctionCall,
}

fn default_function_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OpenAiFunctionCall {
    pub name: String,
    pub arguments: String,
}

pub(crate) fn messages_to_openai(messages: &[Message]) -> Vec<OpenAiChatMessage> {
    messages
        .iter()
        .map(|message| match message.role {
            Role::Tool => OpenAiChatMessage {
                role: "tool".to_string(),
                content: Value::String(message.content.clone()),
                name: None,
                tool_call_id: message.tool_call_id.clone(),
                tool_calls: Vec::new(),
            },
            Role::Assistant if !message.tool_calls.is_empty() => OpenAiChatMessage {
                role: "assistant".to_string(),
                content: if message.content.is_empty() {
                    Value::Null
                } else {
                    Value::String(message.content.clone())
                },
                name: message.name.clone(),
                tool_call_id: None,
                tool_calls: message
                    .tool_calls
                    .iter()
                    .map(|call| OpenAiToolCall {
                        id: call.id.clone(),
                        kind: "function".to_string(),
                        function: OpenAiFunctionCall {
                            name: call.name.clone(),
                            arguments: call.input.clone(),
                        },
                    })
                    .collect(),
            },
            _ => OpenAiChatMessage {
                role: message.role.as_str().to_string(),
                content: build_openai_content(message),
                name: message.name.clone(),
                tool_call_id: None,
                tool_calls: Vec::new(),
            },
        })
        .collect()
}

fn build_openai_content(message: &Message) -> Value {
    if message.attachments.is_empty() {
        return Value::String(message.content.clone());
    }
    let mut parts: Vec<Value> = Vec::new();
    if !message.content.is_empty() {
        parts.push(json!({"type": "text", "text": message.content}));
    }
    for attachment in &message.attachments {
        if attachment.kind != pi_core::AttachmentKind::Image {
            continue;
        }
        if let Some(url) = attachment.data_url() {
            parts.push(json!({
                "type": "image_url",
                "image_url": {"url": url}
            }));
        }
    }
    Value::Array(parts)
}

pub(crate) fn tools_to_openai(tools: &[ToolSchema]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters_or_default(),
                }
            })
        })
        .collect()
}

pub(crate) fn build_messages_with_system(request: &ProviderRequest) -> Vec<OpenAiChatMessage> {
    let mut messages = Vec::new();
    if let Some(system) = &request.system_prompt {
        messages.push(OpenAiChatMessage {
            role: "system".to_string(),
            content: Value::String(system.clone()),
            name: None,
            tool_call_id: None,
            tool_calls: Vec::new(),
        });
    }
    let has_system = request.messages.iter().any(|m| m.role == Role::System);
    let mapped = messages_to_openai(&request.messages);
    if has_system || messages.is_empty() {
        messages.clear();
        messages.extend(mapped);
    } else {
        messages.extend(mapped);
    }
    messages
}

/// Default `stream` implementation used by providers that drive a `VecSink`
/// from inside `complete` first. Tests rely on this helper.
pub fn replay_through_sink(
    response: ProviderResponse,
    sink: &mut dyn StreamSink,
) -> PiResult<ProviderResponse> {
    for event in &response.stream_events {
        sink.emit(event.clone())?;
    }
    Ok(response)
}

/// Helper that fills `stream_events` from a recorded sink so a streaming
/// implementation can also satisfy callers that look at `stream_events` after
/// the fact.
pub fn capture_to_response(
    events: Vec<StreamEvent>,
    mut response: ProviderResponse,
) -> ProviderResponse {
    response.stream_events = events;
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::Role;

    #[test]
    fn echo_responds_with_streaming_events() {
        let provider = EchoProvider;
        let request = ProviderRequest::new(
            ModelSelection {
                provider: "echo".into(),
                model: "echo-local".into(),
            },
            vec![Message::new(Role::User, "你好")],
        );
        let response = provider.complete(request).expect("complete");
        assert!(!response.stream_events.is_empty());
        assert!(matches!(
            response.stream_events[0],
            StreamEvent::MessageStart
        ));
    }

    #[test]
    fn registry_lists_chinese_providers_first_class() {
        let registry = ProviderRegistry::builtin();
        for required in [
            "echo",
            "ollama",
            "moonshot",
            "deepseek",
            "qwen",
            "zhipu",
            "minimax",
            "openai",
            "anthropic",
            "bedrock",
            "gemini",
            "azure",
            "cloudflare",
            "copilot",
            "openrouter",
            "mistral",
            "openai-responses",
            "vertex",
        ] {
            assert!(registry.get(required).is_some(), "missing: {required}");
        }
    }

    #[test]
    fn build_messages_injects_system_prompt() {
        let mut request = ProviderRequest::new(
            ModelSelection {
                provider: "echo".into(),
                model: "x".into(),
            },
            vec![Message::new(Role::User, "hi")],
        );
        request.system_prompt = Some("你是 Pi".to_string());
        let messages = build_messages_with_system(&request);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[1].role, "user");
    }

    #[test]
    fn sse_parser_splits_events_on_blank_lines() {
        let body = b"data: hello\n\ndata: {\"x\":1}\n\n".to_vec();
        let mut collected = Vec::new();
        read_sse(body.as_slice(), |event| {
            collected.push(event.to_string());
            Ok(())
        })
        .expect("parse");
        assert_eq!(
            collected,
            vec!["hello".to_string(), "{\"x\":1}".to_string()]
        );
    }

    #[test]
    fn streaming_default_replays_synthetic_events() {
        let provider = EchoProvider;
        let request = ProviderRequest::new(
            ModelSelection {
                provider: "echo".into(),
                model: "echo-local".into(),
            },
            vec![Message::new(Role::User, "ping")],
        );
        let mut sink = pi_core::VecSink::default();
        let response = provider.stream(request, &mut sink).expect("stream");
        assert!(matches!(
            sink.events.first(),
            Some(StreamEvent::MessageStart)
        ));
        assert!(matches!(sink.events.last(), Some(StreamEvent::MessageDone)));
        assert!(!response.message.content.is_empty());
    }
}
