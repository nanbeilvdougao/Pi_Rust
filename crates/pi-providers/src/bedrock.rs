//! AWS Bedrock provider.
//!
//! Routes Anthropic-on-Bedrock today (`anthropic.claude-*`), with the
//! `invokeModel` non-streaming endpoint. Streaming via
//! `invokeModelWithResponseStream` uses AWS's binary event-stream framing
//! which would require a 400-line parser; we fall back to the trait's
//! default `stream` (replaying captured events) so the agent loop and TUI
//! still get well-formed `StreamEvent`s — just not per-token deltas.
//!
//! Credentials come from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` /
//! `AWS_SESSION_TOKEN` env vars (mirroring the AWS CLI). Region defaults
//! to `us-east-1` unless `AWS_REGION` is set.
//!
//! Bedrock model id is the literal value the user passes via `--model`.

use std::collections::BTreeMap;
use std::env;

use pi_core::{Message, PiError, PiErrorKind, PiResult, Role, StreamEvent, ToolInvocation, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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
    // stream() inherits the default implementation: emits the captured
    // events through the sink. AWS event-stream framing is out of scope
    // until upstream demand justifies a 400-line binary parser.
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
        Thinking {
            thinking: String,
        },
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
            ContentBlock::Thinking { .. } => {}
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
