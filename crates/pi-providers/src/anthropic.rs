//! Anthropic Messages API provider with SSE streaming, tool use, and thinking
//! blocks. Mirrors the TS `@anthropic-ai/sdk` shape closely enough that
//! cross-provider conformance tests can compare it against OpenAI on the
//! `pi-agent` boundary.

use std::env;

use pi_core::{
    Message, PiError, PiErrorKind, PiResult, Role, StreamEvent, StreamSink, ToolInvocation, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    http_agent, post_json, post_sse_lines, read_api_key, text_stream_events,
    tool_call_stream_events, Provider, ProviderInfo, ProviderRequest, ProviderResponse,
};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE: &str = "https://api.anthropic.com";

#[derive(Debug, Default, Clone)]
pub struct AnthropicProvider;

impl AnthropicProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for AnthropicProvider {
    fn info(&self) -> ProviderInfo {
        anthropic_info()
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let api_key = read_api_key("ANTHROPIC_API_KEY", "anthropic")?;
        let body = build_messages_body(&request, false);
        let endpoint = format!(
            "{}/v1/messages",
            env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_BASE.to_string())
                .trim_end_matches('/')
        );
        let response = post_json(
            &http_agent(),
            &endpoint,
            &body,
            &[
                ("x-api-key", api_key.as_str()),
                ("anthropic-version", ANTHROPIC_VERSION),
            ],
        )?;
        parse_messages_response(response)
    }

    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        let api_key = read_api_key("ANTHROPIC_API_KEY", "anthropic")?;
        let body = build_messages_body(&request, true);
        let endpoint = format!(
            "{}/v1/messages",
            env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_BASE.to_string())
                .trim_end_matches('/')
        );
        let agent = http_agent();
        let auth = api_key.clone();

        sink.emit(StreamEvent::MessageStart)?;
        let mut text_buf = String::new();
        let mut tool_builders: Vec<ToolCallBuilder> = Vec::new();
        let mut usage = Usage::default();
        let mut active_block: Option<ActiveBlock> = None;
        let mut errored: Option<PiError> = None;

        post_sse_lines(
            &agent,
            &endpoint,
            &body,
            &[
                ("x-api-key", auth.as_str()),
                ("anthropic-version", ANTHROPIC_VERSION),
            ],
            |line| {
                if sink.cancelled() {
                    return Err(PiError::new(PiErrorKind::Cancelled, "已取消"));
                }
                let value: Value = match serde_json::from_str(line) {
                    Ok(value) => value,
                    Err(err) => {
                        errored = Some(PiError::new(
                            PiErrorKind::Provider,
                            format!("流式块解析失败：{err}; chunk={line}"),
                        ));
                        return Ok(());
                    }
                };
                let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

                match event_type {
                    "content_block_start" => {
                        let index = value
                            .get("index")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as usize)
                            .unwrap_or(0);
                        let block_type = value
                            .get("content_block")
                            .and_then(|cb| cb.get("type"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if block_type == "tool_use" {
                            let id = value
                                .get("content_block")
                                .and_then(|cb| cb.get("id"))
                                .and_then(|v| v.as_str())
                                .map(|v| v.to_string());
                            let name = value
                                .get("content_block")
                                .and_then(|cb| cb.get("name"))
                                .and_then(|v| v.as_str())
                                .map(|v| v.to_string());
                            while tool_builders.len() <= index {
                                tool_builders.push(ToolCallBuilder::default());
                            }
                            tool_builders[index].id = id;
                            tool_builders[index].name = name;
                            active_block = Some(ActiveBlock::ToolUse(index));
                        } else if block_type == "text" {
                            active_block = Some(ActiveBlock::Text);
                        } else if block_type == "thinking" {
                            active_block = Some(ActiveBlock::Thinking);
                        }
                    }
                    "content_block_delta" => {
                        let delta = match value.get("delta") {
                            Some(delta) => delta,
                            None => return Ok(()),
                        };
                        let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        match delta_type {
                            "text_delta" => {
                                if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                                    text_buf.push_str(text);
                                    sink.emit(StreamEvent::TextDelta(text.to_string()))?;
                                }
                            }
                            "thinking_delta" => {
                                if let Some(text) = delta.get("thinking").and_then(|v| v.as_str()) {
                                    sink.emit(StreamEvent::ThinkingDelta(text.to_string()))?;
                                }
                            }
                            "input_json_delta" => {
                                if let (Some(json_str), Some(ActiveBlock::ToolUse(index))) = (
                                    delta.get("partial_json").and_then(|v| v.as_str()),
                                    active_block.as_ref().cloned(),
                                ) {
                                    if let Some(builder) = tool_builders.get_mut(index) {
                                        builder.arguments.push_str(json_str);
                                        sink.emit(StreamEvent::ToolCallDelta {
                                            id: builder.id.clone(),
                                            name: builder.name.clone(),
                                            input_delta: json_str.to_string(),
                                        })?;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        active_block = None;
                    }
                    "message_delta" => {
                        if let Some(u) = value.get("usage") {
                            update_usage(&mut usage, u);
                            sink.emit(StreamEvent::UsageDelta(usage.clone()))?;
                        }
                    }
                    "message_stop" => {}
                    "error" => {
                        let message = value
                            .get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("Anthropic 流式错误")
                            .to_string();
                        errored = Some(PiError::new(PiErrorKind::Provider, message));
                    }
                    _ => {}
                }
                Ok(())
            },
        )?;

        sink.emit(StreamEvent::MessageDone)?;
        if let Some(err) = errored {
            return Err(err);
        }

        let invocations: Vec<ToolInvocation> = tool_builders
            .into_iter()
            .filter_map(|builder| {
                let name = builder.name?;
                let input = if builder.arguments.is_empty() {
                    String::new()
                } else {
                    extract_input_field(&builder.arguments).unwrap_or(builder.arguments)
                };
                Some(ToolInvocation {
                    id: builder.id,
                    name,
                    input,
                })
            })
            .collect();

        let mut message = Message::new(Role::Assistant, text_buf.clone());
        message.tool_calls = invocations.clone();
        let events = if text_buf.is_empty() {
            Vec::new()
        } else {
            vec![text_buf.clone()]
        };
        let stream_events = if invocations.is_empty() {
            text_stream_events(&text_buf)
        } else {
            tool_call_stream_events(&invocations)
        };

        Ok(ProviderResponse {
            message,
            events,
            stream_events,
            tool_calls: invocations,
            usage,
        })
    }
}

#[derive(Debug, Clone)]
enum ActiveBlock {
    Text,
    Thinking,
    ToolUse(usize),
}

#[derive(Default, Debug, Clone)]
struct ToolCallBuilder {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

fn extract_input_field(arguments: &str) -> Option<String> {
    let value: Value = serde_json::from_str(arguments).ok()?;
    value
        .get("input")
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
}

fn build_messages_body(request: &ProviderRequest, stream: bool) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    let mut system = request.system_prompt.clone();

    for message in &request.messages {
        match message.role {
            Role::System => {
                let merged = match system {
                    Some(existing) => format!("{existing}\n\n{}", message.content),
                    None => message.content.clone(),
                };
                system = Some(merged);
            }
            Role::Tool => {
                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                        "content": message.content,
                    }],
                }));
            }
            Role::Assistant if !message.tool_calls.is_empty() => {
                let mut blocks: Vec<Value> = Vec::new();
                if !message.content.is_empty() {
                    blocks.push(json!({"type":"text","text": message.content}));
                }
                for call in &message.tool_calls {
                    let input: Value = serde_json::from_str(&call.input)
                        .unwrap_or_else(|_| json!({"input": call.input}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id.clone().unwrap_or_default(),
                        "name": call.name,
                        "input": input,
                    }));
                }
                messages.push(json!({"role":"assistant","content":blocks}));
            }
            _ => {
                if message.attachments.is_empty() {
                    messages.push(json!({
                        "role": message.role.as_str(),
                        "content": message.content,
                    }));
                } else {
                    let mut blocks: Vec<Value> = Vec::new();
                    if !message.content.is_empty() {
                        blocks.push(json!({"type": "text", "text": message.content}));
                    }
                    for attachment in &message.attachments {
                        if attachment.kind != pi_core::AttachmentKind::Image {
                            continue;
                        }
                        match &attachment.data {
                            pi_core::AttachmentData::Base64 { data } => {
                                blocks.push(json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": attachment.mime_type,
                                        "data": data,
                                    }
                                }));
                            }
                            pi_core::AttachmentData::Url { url } => {
                                blocks.push(json!({
                                    "type": "image",
                                    "source": {"type": "url", "url": url},
                                }));
                            }
                        }
                    }
                    messages.push(json!({
                        "role": message.role.as_str(),
                        "content": blocks,
                    }));
                }
            }
        }
    }

    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters_or_default(),
            })
        })
        .collect();

    let mut body = json!({
        "model": request.model.model,
        "max_tokens": request.max_output_tokens.unwrap_or(4096),
        "messages": messages,
        "stream": stream,
    });
    if let Some(prompt) = system {
        body["system"] = json!(prompt);
    }
    if let Some(temp) = request.temperature {
        body["temperature"] = json!(temp);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    body
}

fn parse_messages_response(value: Value) -> PiResult<ProviderResponse> {
    let response: MessagesResponse = serde_json::from_value(value.clone()).map_err(|err| {
        PiError::new(
            PiErrorKind::Provider,
            format!("Anthropic 响应解析失败：{err}; body={value}"),
        )
    })?;

    let mut text = String::new();
    let mut tool_calls: Vec<ToolInvocation> = Vec::new();
    for block in response.content {
        match block {
            ContentBlock::Text { text: t } => text.push_str(&t),
            ContentBlock::ToolUse { id, name, input } => {
                let input_str = input
                    .get("input")
                    .and_then(|v| v.as_str())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| serde_json::to_string(&input).unwrap_or_default());
                tool_calls.push(ToolInvocation {
                    id: Some(id),
                    name,
                    input: input_str,
                });
            }
            ContentBlock::Thinking { .. } => {}
        }
    }

    let usage = response
        .usage
        .map(|u| Usage {
            prompt_tokens: u.input_tokens.unwrap_or(0),
            completion_tokens: u.output_tokens.unwrap_or(0),
            total_tokens: u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0),
            cache_read_tokens: u.cache_read_input_tokens.unwrap_or(0),
            cache_write_tokens: u.cache_creation_input_tokens.unwrap_or(0),
        })
        .unwrap_or_default();

    let stream_events = if tool_calls.is_empty() {
        text_stream_events(&text)
    } else {
        tool_call_stream_events(&tool_calls)
    };
    let events = if text.is_empty() {
        Vec::new()
    } else {
        vec![text.clone()]
    };
    let mut message = Message::new(Role::Assistant, text);
    message.tool_calls = tool_calls.clone();

    Ok(ProviderResponse {
        message,
        events,
        stream_events,
        tool_calls,
        usage,
    })
}

fn update_usage(usage: &mut Usage, raw: &Value) {
    if let Some(v) = raw.get("input_tokens").and_then(|v| v.as_u64()) {
        usage.prompt_tokens = v as u32;
    }
    if let Some(v) = raw.get("output_tokens").and_then(|v| v.as_u64()) {
        usage.completion_tokens = v as u32;
    }
    if let Some(v) = raw.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
        usage.cache_read_tokens = v as u32;
    }
    if let Some(v) = raw
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
    {
        usage.cache_write_tokens = v as u32;
    }
    usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
}

#[derive(Debug, Deserialize, Serialize)]
struct MessagesResponse {
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Thinking {
        thinking: String,
    },
}

#[derive(Debug, Deserialize, Serialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
}

pub fn anthropic_info() -> ProviderInfo {
    ProviderInfo {
        id: "anthropic".to_string(),
        display_name: "Anthropic Claude".to_string(),
        default_model: "claude-sonnet-4-6".to_string(),
        supported_models: vec![
            "claude-sonnet-4-6".to_string(),
            "claude-opus-4-7".to_string(),
            "claude-haiku-4-5-20251001".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{Message, ModelSelection, Role};

    #[test]
    fn request_includes_system_and_tool_results() {
        let mut tool_message = Message::new(Role::Tool, "result");
        tool_message.tool_call_id = Some("call_1".to_string());
        let request = ProviderRequest {
            model: ModelSelection {
                provider: "anthropic".to_string(),
                model: "claude-sonnet-4-6".to_string(),
            },
            messages: vec![Message::new(Role::User, "你好"), tool_message],
            tools: Vec::new(),
            system_prompt: Some("be terse".to_string()),
            max_output_tokens: None,
            temperature: None,
            stream: false,
        };
        let body = build_messages_body(&request, false);
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["messages"][1]["content"][0]["type"], "tool_result");
        assert_eq!(body["messages"][1]["content"][0]["tool_use_id"], "call_1");
    }

    #[test]
    fn parses_text_and_tool_use_response() {
        let raw = json!({
            "content": [
                {"type": "text", "text": "Hi"},
                {"type": "tool_use", "id": "call_1", "name": "ls", "input": {"path": "."}}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let response = parse_messages_response(raw).expect("parse");
        assert_eq!(response.message.content, "Hi");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "ls");
        assert_eq!(response.usage.prompt_tokens, 10);
        assert_eq!(response.usage.completion_tokens, 5);
    }
}
