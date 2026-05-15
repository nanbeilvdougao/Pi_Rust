//! AWS Bedrock provider.
//!
//! Routes Anthropic-on-Bedrock (`anthropic.claude-*`). Two endpoints:
//!
//! - `invokeModel` — non-streaming, classic Anthropic body shape.
//! - `invokeModelWithResponseStream` — streaming over AWS's binary
//!   `application/vnd.amazon.eventstream` framing. Each event has a small
//!   header block plus a JSON payload of shape `{"bytes": "<base64>"}`,
//!   where the base64 decodes into the same Anthropic SSE event JSON we
//!   already understand (`message_start`, `content_block_delta`,
//!   `message_delta`, `message_stop`, …). We parse the binary frames with
//!   `crate::aws_event_stream` and map them into `StreamEvent`s.
//!
//! Credentials come from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` /
//! `AWS_SESSION_TOKEN` env vars (mirroring the AWS CLI). Region defaults
//! to `us-east-1` unless `AWS_REGION` is set.
//!
//! Bedrock model id is the literal value the user passes via `--model`.

use std::collections::BTreeMap;
use std::env;

use pi_core::{
    base64_decode, Message, PiError, PiErrorKind, PiResult, Role, StreamEvent, StreamSink,
    ToolInvocation, Usage,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::aws_event_stream::{parse as parse_event_stream, EventStreamMessage};
use crate::sigv4::SigV4Request;
use crate::{
    http_agent, text_stream_events, tool_call_stream_events, Provider, ProviderInfo,
    ProviderRequest, ProviderResponse,
};

#[derive(Debug, Default, Clone)]
pub struct BedrockProvider;

impl BedrockProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for BedrockProvider {
    fn info(&self) -> ProviderInfo {
        bedrock_info()
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let creds = load_credentials()?;
        let region = env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_string());
        let host = format!("bedrock-runtime.{region}.amazonaws.com");
        let model_id = uri_segment_encode(&request.model.model);
        let path = format!("/model/{model_id}/invoke");
        let body = build_anthropic_body(&request);
        let body_bytes = serde_json::to_vec(&body)?;

        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        let signed = SigV4Request {
            method: "POST",
            host: &host,
            path: &path,
            query: "",
            headers,
            body: &body_bytes,
            region: &region,
            service: "bedrock",
            access_key: &creds.access_key,
            secret_key: &creds.secret_key,
            session_token: creds.session_token.as_deref(),
        }
        .sign();

        let url = format!("https://{host}{path}");
        let mut req = http_agent().post(&url);
        for (k, v) in &signed.headers {
            // ureq sets `host` automatically; skip ours to avoid duplicates.
            if k == "host" {
                continue;
            }
            req = req.set(k, v);
        }
        let response = req.send_bytes(&body_bytes).map_err(|err| {
            PiError::new(
                PiErrorKind::Network,
                format!("Bedrock invokeModel 失败：{err}"),
            )
        })?;
        let text = response
            .into_string()
            .map_err(|err| PiError::new(PiErrorKind::Network, err.to_string()))?;
        parse_anthropic_response(&text)
    }
    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        let creds = load_credentials()?;
        let region = env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".to_string());
        let host = format!("bedrock-runtime.{region}.amazonaws.com");
        let model_id = uri_segment_encode(&request.model.model);
        let path = format!("/model/{model_id}/invoke-with-response-stream");
        let body = build_anthropic_body(&request);
        let body_bytes = serde_json::to_vec(&body)?;

        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        headers.insert(
            "x-amzn-bedrock-accept".to_string(),
            "application/vnd.amazon.eventstream".to_string(),
        );
        let signed = SigV4Request {
            method: "POST",
            host: &host,
            path: &path,
            query: "",
            headers,
            body: &body_bytes,
            region: &region,
            service: "bedrock",
            access_key: &creds.access_key,
            secret_key: &creds.secret_key,
            session_token: creds.session_token.as_deref(),
        }
        .sign();

        let url = format!("https://{host}{path}");
        let mut req = http_agent().post(&url);
        for (k, v) in &signed.headers {
            if k == "host" {
                continue;
            }
            req = req.set(k, v);
        }
        let response = match req.send_bytes(&body_bytes) {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let body = response.into_string().unwrap_or_default();
                return Err(PiError::new(
                    PiErrorKind::Provider,
                    format!("Bedrock invokeModelWithResponseStream HTTP {status}：{body}"),
                ));
            }
            Err(ureq::Error::Transport(err)) => {
                return Err(PiError::new(
                    PiErrorKind::Network,
                    format!("Bedrock invokeModelWithResponseStream 传输错误：{err}"),
                ));
            }
        };

        sink.emit(StreamEvent::MessageStart)?;
        let mut text_buf = String::new();
        let mut tool_calls: Vec<ToolInvocation> = Vec::new();
        let mut tool_input_buf: BTreeMap<usize, String> = BTreeMap::new();
        let mut tool_index_to_id: BTreeMap<usize, String> = BTreeMap::new();
        let mut usage = Usage::default();
        let mut errored: Option<PiError> = None;

        parse_event_stream(response.into_reader(), |msg| {
            if sink.cancelled() {
                return Err(PiError::new(PiErrorKind::Cancelled, "已取消"));
            }
            if let Err(err) = handle_bedrock_message(
                &msg,
                sink,
                &mut text_buf,
                &mut tool_calls,
                &mut tool_input_buf,
                &mut tool_index_to_id,
                &mut usage,
            ) {
                errored = Some(err);
            }
            Ok(())
        })?;
        sink.emit(StreamEvent::MessageDone)?;
        if let Some(err) = errored {
            return Err(err);
        }
        // Stitch partial tool input deltas into final invocations.
        for (idx, input) in tool_input_buf {
            if let Some(call) = tool_calls.iter_mut().find(|c| {
                tool_index_to_id
                    .get(&idx)
                    .map(|id| c.id.as_deref() == Some(id.as_str()))
                    .unwrap_or(false)
            }) {
                call.input = input;
            }
        }

        let stream_events = if tool_calls.is_empty() {
            text_stream_events(&text_buf)
        } else {
            tool_call_stream_events(&tool_calls)
        };
        let events = if text_buf.is_empty() {
            Vec::new()
        } else {
            vec![text_buf.clone()]
        };
        let mut message = Message::new(Role::Assistant, text_buf);
        message.tool_calls = tool_calls.clone();
        Ok(ProviderResponse {
            message,
            events,
            stream_events,
            tool_calls,
            usage,
        })
    }
}

/// Decode the embedded base64 chunk inside a Bedrock event-stream message
/// and emit the corresponding StreamEvent. Mirrors Anthropic's SSE event
/// taxonomy so callers can reuse the same downstream handlers.
fn handle_bedrock_message(
    msg: &EventStreamMessage,
    sink: &mut dyn StreamSink,
    text_buf: &mut String,
    tool_calls: &mut Vec<ToolInvocation>,
    tool_input_buf: &mut BTreeMap<usize, String>,
    tool_index_to_id: &mut BTreeMap<usize, String>,
    usage: &mut Usage,
) -> PiResult<()> {
    let message_type = msg.header(":message-type").unwrap_or("event");
    if message_type == "exception" {
        let text = String::from_utf8_lossy(&msg.payload).to_string();
        return Err(PiError::new(
            PiErrorKind::Provider,
            format!("Bedrock 流式异常：{text}"),
        ));
    }
    let outer: Value = serde_json::from_slice(&msg.payload).map_err(|err| {
        PiError::new(
            PiErrorKind::Provider,
            format!("Bedrock 流式 payload 解析失败：{err}"),
        )
    })?;
    let inner_bytes = outer.get("bytes").and_then(|v| v.as_str()).ok_or_else(|| {
        PiError::new(
            PiErrorKind::Provider,
            "Bedrock 流式 payload 缺少 bytes 字段",
        )
    })?;
    let decoded = base64_decode(inner_bytes).ok_or_else(|| {
        PiError::new(
            PiErrorKind::Provider,
            "Bedrock 流式 payload base64 解码失败",
        )
    })?;
    let event: Value = serde_json::from_slice(&decoded).map_err(|err| {
        PiError::new(
            PiErrorKind::Provider,
            format!("Bedrock 流式事件解析失败：{err}"),
        )
    })?;
    let kind = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "content_block_start" => {
            if let Some(block) = event.get("content_block") {
                if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    let idx = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    tool_calls.push(ToolInvocation {
                        id: Some(id.clone()),
                        name: name.clone(),
                        input: String::new(),
                    });
                    tool_index_to_id.insert(idx, id.clone());
                    sink.emit(StreamEvent::ToolCallDelta {
                        id: Some(id),
                        name: Some(name),
                        input_delta: String::new(),
                    })?;
                }
            }
        }
        "content_block_delta" => {
            if let Some(delta) = event.get("delta") {
                let delta_type = delta.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match delta_type {
                    "text_delta" => {
                        if let Some(text) = delta.get("text").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                text_buf.push_str(text);
                                sink.emit(StreamEvent::TextDelta(text.to_string()))?;
                            }
                        }
                    }
                    "thinking_delta" => {
                        if let Some(text) = delta.get("thinking").and_then(|v| v.as_str()) {
                            sink.emit(StreamEvent::ThinkingDelta(text.to_string()))?;
                        }
                    }
                    "input_json_delta" => {
                        if let Some(partial) = delta.get("partial_json").and_then(|v| v.as_str()) {
                            let idx =
                                event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                            tool_input_buf.entry(idx).or_default().push_str(partial);
                            let id = tool_index_to_id.get(&idx).cloned();
                            sink.emit(StreamEvent::ToolCallDelta {
                                id,
                                name: None,
                                input_delta: partial.to_string(),
                            })?;
                        }
                    }
                    _ => {}
                }
            }
        }
        "message_delta" => {
            if let Some(u) = event.get("usage") {
                if let Some(out) = u.get("output_tokens").and_then(|v| v.as_u64()) {
                    usage.completion_tokens = out as u32;
                    usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
                    sink.emit(StreamEvent::UsageDelta(usage.clone()))?;
                }
            }
        }
        "message_start" => {
            if let Some(u) = event.pointer("/message/usage") {
                if let Some(input) = u.get("input_tokens").and_then(|v| v.as_u64()) {
                    usage.prompt_tokens = input as u32;
                }
            }
        }
        "message_stop" | "content_block_stop" | "ping" => {}
        _ => {}
    }
    Ok(())
}

#[derive(Debug)]
struct AwsCredentials {
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
}

fn load_credentials() -> PiResult<AwsCredentials> {
    let access_key = env::var("AWS_ACCESS_KEY_ID").map_err(|_| {
        PiError::new(
            PiErrorKind::Provider,
            "缺少 AWS_ACCESS_KEY_ID（Bedrock 需要 AWS 凭证）",
        )
    })?;
    let secret_key = env::var("AWS_SECRET_ACCESS_KEY").map_err(|_| {
        PiError::new(
            PiErrorKind::Provider,
            "缺少 AWS_SECRET_ACCESS_KEY（Bedrock 需要 AWS 凭证）",
        )
    })?;
    Ok(AwsCredentials {
        access_key,
        secret_key,
        session_token: env::var("AWS_SESSION_TOKEN").ok(),
    })
}

fn build_anthropic_body(request: &ProviderRequest) -> Value {
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
            Role::Tool => messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                    "content": message.content,
                }]
            })),
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
            _ => messages.push(json!({
                "role": message.role.as_str(),
                "content": message.content,
            })),
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
        "anthropic_version": "bedrock-2023-05-31",
        "max_tokens": request.max_output_tokens.unwrap_or(4096),
        "messages": messages,
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

fn parse_anthropic_response(text: &str) -> PiResult<ProviderResponse> {
    #[derive(Debug, Deserialize)]
    struct BedrockMessage {
        #[serde(default)]
        content: Vec<ContentBlock>,
        #[serde(default)]
        usage: Option<BedrockUsage>,
    }
    #[derive(Debug, Deserialize)]
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
        Thinking,
    }
    #[derive(Debug, Deserialize, Default)]
    struct BedrockUsage {
        #[serde(default)]
        input_tokens: Option<u32>,
        #[serde(default)]
        output_tokens: Option<u32>,
    }

    let parsed: BedrockMessage = serde_json::from_str(text).map_err(|err| {
        PiError::new(
            PiErrorKind::Provider,
            format!("Bedrock 响应解析失败：{err}; body={text}"),
        )
    })?;
    let mut content = String::new();
    let mut tool_calls: Vec<ToolInvocation> = Vec::new();
    for block in parsed.content {
        match block {
            ContentBlock::Text { text } => content.push_str(&text),
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
            ContentBlock::Thinking => {}
        }
    }
    let usage = parsed
        .usage
        .map(|u| Usage {
            prompt_tokens: u.input_tokens.unwrap_or(0),
            completion_tokens: u.output_tokens.unwrap_or(0),
            total_tokens: u.input_tokens.unwrap_or(0) + u.output_tokens.unwrap_or(0),
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        })
        .unwrap_or_default();
    let stream_events = if tool_calls.is_empty() {
        text_stream_events(&content)
    } else {
        tool_call_stream_events(&tool_calls)
    };
    let events = if content.is_empty() {
        Vec::new()
    } else {
        vec![content.clone()]
    };
    let mut message = Message::new(Role::Assistant, content);
    message.tool_calls = tool_calls.clone();
    Ok(ProviderResponse {
        message,
        events,
        stream_events,
        tool_calls,
        usage,
    })
}

fn uri_segment_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{:02X}", b)
            }
        })
        .collect()
}

pub fn bedrock_info() -> ProviderInfo {
    ProviderInfo {
        id: "bedrock".to_string(),
        display_name: "AWS Bedrock (Anthropic)".to_string(),
        default_model: "anthropic.claude-3-5-sonnet-20241022-v2:0".to_string(),
        supported_models: vec![
            "anthropic.claude-3-5-sonnet-20241022-v2:0".to_string(),
            "anthropic.claude-3-5-haiku-20241022-v1:0".to_string(),
            "anthropic.claude-3-opus-20240229-v1:0".to_string(),
            "anthropic.claude-3-haiku-20240307-v1:0".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("AWS_ACCESS_KEY_ID".to_string()),
    }
}

// Use `StreamEvent` re-export so the default trait impl path stays clean even
// when this module is the only one referencing it inside `complete`.
#[allow(dead_code)]
fn _stream_event_anchor(_e: &StreamEvent) {}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{Message, ModelSelection, Role};

    #[test]
    fn body_routes_system_into_anthropic_field() {
        let request = ProviderRequest {
            model: ModelSelection {
                provider: "bedrock".to_string(),
                model: "anthropic.claude-3-5-sonnet-20241022-v2:0".to_string(),
            },
            messages: vec![Message::new(Role::User, "你好")],
            tools: Vec::new(),
            system_prompt: Some("be terse".to_string()),
            max_output_tokens: Some(123),
            temperature: Some(0.3),
            stream: false,
        };
        let body = build_anthropic_body(&request);
        assert_eq!(body["anthropic_version"], "bedrock-2023-05-31");
        assert_eq!(body["max_tokens"], 123);
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn handle_bedrock_message_routes_text_delta_via_base64_payload() {
        struct CapturingSink {
            text: String,
            tool_deltas: Vec<String>,
        }
        impl pi_core::StreamSink for CapturingSink {
            fn emit(&mut self, event: pi_core::StreamEvent) -> PiResult<()> {
                match event {
                    pi_core::StreamEvent::TextDelta(t) => self.text.push_str(&t),
                    pi_core::StreamEvent::ToolCallDelta { input_delta, .. } => {
                        self.tool_deltas.push(input_delta)
                    }
                    _ => {}
                }
                Ok(())
            }
            fn cancelled(&self) -> bool {
                false
            }
        }
        let inner = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hello "}
        });
        let inner_b = pi_core::base64_encode(&serde_json::to_vec(&inner).unwrap());
        let outer = serde_json::json!({"bytes": inner_b}).to_string();
        let msg = EventStreamMessage {
            headers: vec![(":message-type".to_string(), "event".to_string())],
            payload: outer.into_bytes(),
        };
        let mut sink = CapturingSink {
            text: String::new(),
            tool_deltas: Vec::new(),
        };
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut buf = BTreeMap::new();
        let mut id_map = BTreeMap::new();
        let mut usage = Usage::default();
        super::handle_bedrock_message(
            &msg,
            &mut sink,
            &mut text,
            &mut calls,
            &mut buf,
            &mut id_map,
            &mut usage,
        )
        .expect("handle");
        assert_eq!(text, "hello ");
        assert_eq!(sink.text, "hello ");
    }

    #[test]
    fn handle_bedrock_message_routes_tool_use_blocks() {
        struct NullSink;
        impl pi_core::StreamSink for NullSink {
            fn emit(&mut self, _e: pi_core::StreamEvent) -> PiResult<()> {
                Ok(())
            }
            fn cancelled(&self) -> bool {
                false
            }
        }
        let start = serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "tool_use", "id": "call_1", "name": "ls"}
        });
        let delta = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{\"path\":\".\""}
        });
        let delta2 = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "}"}
        });
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut buf = BTreeMap::new();
        let mut id_map = BTreeMap::new();
        let mut usage = Usage::default();
        for inner in [start, delta, delta2] {
            let inner_b = pi_core::base64_encode(&serde_json::to_vec(&inner).unwrap());
            let outer = serde_json::json!({"bytes": inner_b}).to_string();
            let msg = EventStreamMessage {
                headers: vec![(":message-type".to_string(), "event".to_string())],
                payload: outer.into_bytes(),
            };
            super::handle_bedrock_message(
                &msg,
                &mut NullSink,
                &mut text,
                &mut calls,
                &mut buf,
                &mut id_map,
                &mut usage,
            )
            .expect("handle");
        }
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "ls");
        assert_eq!(calls[0].id.as_deref(), Some("call_1"));
        assert_eq!(buf.get(&0).map(|s| s.as_str()), Some("{\"path\":\".\"}"));
    }

    #[test]
    fn parses_text_and_tool_use_response() {
        let raw = r#"{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"call_1","name":"ls","input":{"input":"."}}],"usage":{"input_tokens":4,"output_tokens":5}}"#;
        let response = parse_anthropic_response(raw).expect("parse");
        assert_eq!(response.message.content, "hi");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "ls");
        assert_eq!(response.tool_calls[0].input, ".");
        assert_eq!(response.usage.prompt_tokens, 4);
        assert_eq!(response.usage.completion_tokens, 5);
    }
}
