//! OpenAI Responses API provider.
//!
//! The Responses API (`POST /v1/responses`) is the official successor to
//! Chat Completions. Differences we care about:
//!
//! - Request body uses `model` + `input` (an array of items) instead of
//!   `messages`. We map every `Message` to a single `{role, content: [...]}`
//!   item; multi-modal attachments become `{type:"input_image", image_url}`.
//! - Response body returns `output: [{type:"message", content:[...]}, …]`
//!   plus `usage: {input_tokens, output_tokens, total_tokens}`.
//! - Streaming uses SSE with typed events:
//!   `response.output_text.delta`, `response.completed`, etc.
//!
//! Same Bearer auth as chat-completions: `OPENAI_API_KEY` env var.

use std::env;

use pi_core::{
    Message, PiError, PiErrorKind, PiResult, Role, StreamEvent, StreamSink, ToolInvocation, Usage,
};
use serde_json::{json, Value};

use crate::{
    http_agent, post_json, post_sse_lines, read_api_key, text_stream_events,
    tool_call_stream_events, Provider, ProviderInfo, ProviderRequest, ProviderResponse,
};

#[derive(Debug, Default, Clone)]
pub struct OpenAiResponsesProvider;

impl OpenAiResponsesProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for OpenAiResponsesProvider {
    fn info(&self) -> ProviderInfo {
        openai_responses_info()
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let key = read_api_key("OPENAI_API_KEY", "openai-responses")?;
        let url = endpoint();
        let body = build_request_body(&request, false);
        let auth = format!("Bearer {key}");
        let response = post_json(
            &http_agent(),
            &url,
            &body,
            &[("authorization", auth.as_str())],
        )?;
        parse_response(response)
    }

    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        let key = read_api_key("OPENAI_API_KEY", "openai-responses")?;
        let url = endpoint();
        let body = build_request_body(&request, true);
        let auth = format!("Bearer {key}");

        sink.emit(StreamEvent::MessageStart)?;
        let mut text_buf = String::new();
        let mut tool_calls: Vec<ToolInvocation> = Vec::new();
        let mut usage = Usage::default();
        let mut errored: Option<PiError> = None;

        post_sse_lines(
            &http_agent(),
            &url,
            &body,
            &[("authorization", auth.as_str())],
            |line| {
                if sink.cancelled() {
                    return Err(PiError::new(PiErrorKind::Cancelled, "已取消"));
                }
                let value: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(err) => {
                        errored = Some(PiError::new(
                            PiErrorKind::Provider,
                            format!("Responses 流式块解析失败：{err}; chunk={line}"),
                        ));
                        return Ok(());
                    }
                };
                let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match event_type {
                    "response.output_text.delta" => {
                        if let Some(delta) = value.get("delta").and_then(|v| v.as_str()) {
                            if !delta.is_empty() {
                                text_buf.push_str(delta);
                                sink.emit(StreamEvent::TextDelta(delta.to_string()))?;
                            }
                        }
                    }
                    "response.reasoning_summary_text.delta" => {
                        if let Some(delta) = value.get("delta").and_then(|v| v.as_str()) {
                            sink.emit(StreamEvent::ThinkingDelta(delta.to_string()))?;
                        }
                    }
                    "response.output_item.added" => {
                        if let Some(item) = value.get("item") {
                            if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                                let call = parse_function_call(item);
                                if !call.name.is_empty() {
                                    sink.emit(StreamEvent::ToolCallDelta {
                                        id: call.id.clone(),
                                        name: Some(call.name.clone()),
                                        input_delta: call.input.clone(),
                                    })?;
                                    tool_calls.push(call);
                                }
                            }
                        }
                    }
                    "response.completed" => {
                        if let Some(response) = value.get("response") {
                            if let Some(u) = response.get("usage") {
                                usage = parse_usage(u);
                                sink.emit(StreamEvent::UsageDelta(usage.clone()))?;
                            }
                        }
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

        let mut message = Message::new(Role::Assistant, text_buf.clone());
        message.tool_calls = tool_calls.clone();
        let events = if text_buf.is_empty() {
            Vec::new()
        } else {
            vec![text_buf.clone()]
        };
        let stream_events = if tool_calls.is_empty() {
            text_stream_events(&text_buf)
        } else {
            tool_call_stream_events(&tool_calls)
        };
        Ok(ProviderResponse {
            message,
            events,
            stream_events,
            tool_calls,
            usage,
        })
    }
}

fn endpoint() -> String {
    let base =
        env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    format!("{}/responses", base.trim_end_matches('/'))
}

fn build_request_body(request: &ProviderRequest, stream: bool) -> Value {
    let mut input: Vec<Value> = Vec::new();
    if let Some(system) = &request.system_prompt {
        input.push(json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        match message.role {
            Role::System => input.push(json!({"role": "system", "content": message.content})),
            Role::Tool => input.push(json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id.clone().unwrap_or_default(),
                "output": message.content,
            })),
            Role::Assistant => {
                let mut content: Vec<Value> = Vec::new();
                if !message.content.is_empty() {
                    content.push(json!({"type": "output_text", "text": message.content}));
                }
                for call in &message.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": call.id.clone().unwrap_or_default(),
                        "name": call.name,
                        "arguments": call.input,
                    }));
                }
                if !content.is_empty() {
                    input.push(json!({"role": "assistant", "content": content}));
                }
            }
            Role::User => {
                let mut content: Vec<Value> = Vec::new();
                if !message.content.is_empty() {
                    content.push(json!({"type": "input_text", "text": message.content}));
                }
                for attachment in &message.attachments {
                    if attachment.kind != pi_core::AttachmentKind::Image {
                        continue;
                    }
                    if let Some(url) = attachment.data_url() {
                        content.push(json!({"type": "input_image", "image_url": url}));
                    }
                }
                input.push(json!({"role": "user", "content": content}));
            }
        }
    }
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters_or_default(),
            })
        })
        .collect();
    let mut body = json!({
        "model": request.model.model,
        "input": input,
        "stream": stream,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if let Some(temp) = request.temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(max) = request.max_output_tokens {
        body["max_output_tokens"] = json!(max);
    }
    body
}

fn parse_response(value: Value) -> PiResult<ProviderResponse> {
    let mut text = String::new();
    let mut tool_calls: Vec<ToolInvocation> = Vec::new();
    if let Some(output) = value.get("output").and_then(|v| v.as_array()) {
        for item in output {
            match item.get("type").and_then(|v| v.as_str()) {
                Some("message") => {
                    if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                        for block in content {
                            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                                text.push_str(t);
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let call = parse_function_call(item);
                    if !call.name.is_empty() {
                        tool_calls.push(call);
                    }
                }
                _ => {}
            }
        }
    }
    let usage = value.get("usage").map(parse_usage).unwrap_or_default();
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

fn parse_function_call(item: &Value) -> ToolInvocation {
    let name = item
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let arguments = item
        .get("arguments")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let input = serde_json::from_str::<Value>(&arguments)
        .ok()
        .and_then(|v| {
            v.get("input")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or(arguments);
    ToolInvocation { id, name, input }
}

fn parse_usage(value: &Value) -> Usage {
    Usage {
        prompt_tokens: value
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        completion_tokens: value
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        total_tokens: value
            .get("total_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        cache_read_tokens: value
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32,
        cache_write_tokens: 0,
    }
}

pub fn openai_responses_info() -> ProviderInfo {
    ProviderInfo {
        id: "openai-responses".to_string(),
        display_name: "OpenAI Responses API".to_string(),
        default_model: "gpt-4.1".to_string(),
        supported_models: vec![
            "gpt-4.1".to_string(),
            "gpt-4o".to_string(),
            "gpt-4o-mini".to_string(),
            "o4-mini".to_string(),
            "o3".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("OPENAI_API_KEY".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::ModelSelection;

    #[test]
    fn body_uses_input_array_not_messages() {
        let req = ProviderRequest::new(
            ModelSelection {
                provider: "openai-responses".into(),
                model: "gpt-4.1".into(),
            },
            vec![Message::new(Role::User, "你好")],
        );
        let body = build_request_body(&req, true);
        assert_eq!(body["stream"], true);
        assert_eq!(body["model"], "gpt-4.1");
        assert!(body.get("messages").is_none());
        assert!(body["input"].is_array());
    }

    #[test]
    fn parses_response_output_text() {
        let raw = serde_json::json!({
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "Hello"}]
            }],
            "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
        });
        let response = parse_response(raw).expect("parse");
        assert_eq!(response.message.content, "Hello");
        assert_eq!(response.usage.total_tokens, 4);
    }
}
