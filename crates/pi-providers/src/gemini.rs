//! Google Gemini provider (`generativelanguage.googleapis.com/v1beta`).
//! Supports both `:generateContent` and `:streamGenerateContent?alt=sse`.

use std::env;

use pi_core::{
    Message, PiError, PiErrorKind, PiResult, Role, StreamEvent, StreamSink, ToolInvocation, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    http_agent, post_json, post_sse_lines, text_stream_events, tool_call_stream_events, Provider,
    ProviderInfo, ProviderRequest, ProviderResponse,
};

const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com";

#[derive(Debug, Default, Clone)]
pub struct GeminiProvider;

impl GeminiProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for GeminiProvider {
    fn info(&self) -> ProviderInfo {
        gemini_info()
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let api_key = env::var("GEMINI_API_KEY")
            .or_else(|_| env::var("GOOGLE_API_KEY"))
            .map_err(|_| {
                PiError::new(
                    PiErrorKind::Provider,
                    "缺少环境变量 GEMINI_API_KEY 或 GOOGLE_API_KEY".to_string(),
                )
            })?;
        let endpoint = format!(
            "{}/v1beta/models/{}:generateContent?key={}",
            base_url(),
            request.model.model,
            api_key,
        );
        let body = build_body(&request);
        let response = post_json(&http_agent(), &endpoint, &body, &[])?;
        parse_response(response)
    }

    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        let api_key = env::var("GEMINI_API_KEY")
            .or_else(|_| env::var("GOOGLE_API_KEY"))
            .map_err(|_| {
                PiError::new(
                    PiErrorKind::Provider,
                    "缺少环境变量 GEMINI_API_KEY 或 GOOGLE_API_KEY".to_string(),
                )
            })?;
        let endpoint = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
            base_url(),
            request.model.model,
            api_key,
        );
        let body = build_body(&request);
        let agent = http_agent();
        sink.emit(StreamEvent::MessageStart)?;
        let mut text_buf = String::new();
        let mut tool_calls: Vec<ToolInvocation> = Vec::new();
        let mut usage = Usage::default();
        let mut errored: Option<PiError> = None;

        post_sse_lines(&agent, &endpoint, &body, &[], |line| {
            if sink.cancelled() {
                return Err(PiError::new(PiErrorKind::Cancelled, "已取消"));
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(err) => {
                    errored = Some(PiError::new(
                        PiErrorKind::Provider,
                        format!("Gemini 流式块解析失败：{err}; chunk={line}"),
                    ));
                    return Ok(());
                }
            };
            accumulate_chunk_public(&value, sink, &mut text_buf, &mut tool_calls, &mut usage)
        })?;
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

fn base_url() -> String {
    env::var("GEMINI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE.to_string())
}

pub fn build_body_public(request: &ProviderRequest) -> Value {
    build_body(request)
}

pub fn parse_response_public(value: Value) -> PiResult<ProviderResponse> {
    parse_response(value)
}

pub fn accumulate_chunk_public(
    value: &Value,
    sink: &mut dyn pi_core::StreamSink,
    text_buf: &mut String,
    tool_calls: &mut Vec<ToolInvocation>,
    usage: &mut Usage,
) -> PiResult<()> {
    if let Some(candidate) = value
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
    {
        if let Some(parts) = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
        {
            for part in parts {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        text_buf.push_str(text);
                        sink.emit(StreamEvent::TextDelta(text.to_string()))?;
                    }
                }
                if let Some(call) = part.get("functionCall") {
                    let name = call
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = call.get("args").cloned().unwrap_or(Value::Null);
                    let input = args
                        .get("input")
                        .and_then(|v| v.as_str())
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| serde_json::to_string(&args).unwrap_or_default());
                    sink.emit(StreamEvent::ToolCallDelta {
                        id: None,
                        name: Some(name.clone()),
                        input_delta: input.clone(),
                    })?;
                    tool_calls.push(ToolInvocation {
                        id: None,
                        name,
                        input,
                    });
                }
            }
        }
    }
    if let Some(metadata) = value.get("usageMetadata") {
        usage.prompt_tokens = metadata
            .get("promptTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        usage.completion_tokens = metadata
            .get("candidatesTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;
        usage.total_tokens = metadata
            .get("totalTokenCount")
            .and_then(|v| v.as_u64())
            .unwrap_or(usage.prompt_tokens as u64 + usage.completion_tokens as u64)
            as u32;
        sink.emit(StreamEvent::UsageDelta(usage.clone()))?;
    }
    Ok(())
}

fn build_body(request: &ProviderRequest) -> Value {
    let mut contents: Vec<Value> = Vec::new();
    let mut system_instruction: Option<String> = request.system_prompt.clone();
    for message in &request.messages {
        match message.role {
            Role::System => {
                let merged = match system_instruction {
                    Some(existing) => format!("{existing}\n\n{}", message.content),
                    None => message.content.clone(),
                };
                system_instruction = Some(merged);
            }
            Role::Tool => contents.push(json!({
                "role": "user",
                "parts": [{
                    "functionResponse": {
                        "name": message.name.clone().unwrap_or_default(),
                        "response": {"content": message.content},
                    }
                }],
            })),
            Role::Assistant => {
                let mut parts = Vec::new();
                if !message.content.is_empty() {
                    parts.push(json!({"text": message.content}));
                }
                for call in &message.tool_calls {
                    let args: Value = serde_json::from_str(&call.input)
                        .unwrap_or_else(|_| json!({"input": call.input}));
                    parts.push(json!({
                        "functionCall": {
                            "name": call.name,
                            "args": args,
                        }
                    }));
                }
                contents.push(json!({"role":"model","parts": parts}));
            }
            Role::User => {
                let mut parts: Vec<Value> = Vec::new();
                if !message.content.is_empty() {
                    parts.push(json!({"text": message.content}));
                }
                for attachment in &message.attachments {
                    if attachment.kind != pi_core::AttachmentKind::Image {
                        continue;
                    }
                    match &attachment.data {
                        pi_core::AttachmentData::Base64 { data } => {
                            parts.push(json!({
                                "inlineData": {
                                    "mimeType": attachment.mime_type,
                                    "data": data,
                                }
                            }));
                        }
                        pi_core::AttachmentData::Url { url } => {
                            parts.push(json!({
                                "fileData": {
                                    "mimeType": attachment.mime_type,
                                    "fileUri": url,
                                }
                            }));
                        }
                    }
                }
                if parts.is_empty() {
                    parts.push(json!({"text": ""}));
                }
                contents.push(json!({"role": "user", "parts": parts}));
            }
        }
    }

    let tools = if request.tools.is_empty() {
        Value::Null
    } else {
        json!([{
            "functionDeclarations": request
                .tools
                .iter()
                .map(|tool| json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters_or_default(),
                }))
                .collect::<Vec<_>>()
        }])
    };

    let mut body = json!({"contents": contents});
    if let Some(prompt) = system_instruction {
        body["systemInstruction"] = json!({"parts": [{"text": prompt}]});
    }
    if !tools.is_null() {
        body["tools"] = tools;
    }
    if let Some(max) = request.max_output_tokens {
        body["generationConfig"] = json!({"maxOutputTokens": max});
    }
    if let Some(temp) = request.temperature {
        body["generationConfig"] = json!({"temperature": temp});
    }
    body
}

fn parse_response(value: Value) -> PiResult<ProviderResponse> {
    let parsed: GenerateResponse = serde_json::from_value(value.clone()).map_err(|err| {
        PiError::new(
            PiErrorKind::Provider,
            format!("Gemini 响应解析失败：{err}; body={value}"),
        )
    })?;

    let mut text = String::new();
    let mut tool_calls: Vec<ToolInvocation> = Vec::new();
    if let Some(candidate) = parsed.candidates.into_iter().next() {
        for part in candidate.content.parts {
            match part {
                ContentPart::Text { text: t } => text.push_str(&t),
                ContentPart::FunctionCall { name, args } => {
                    let input = args
                        .get("input")
                        .and_then(|v| v.as_str())
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| serde_json::to_string(&args).unwrap_or_default());
                    tool_calls.push(ToolInvocation {
                        id: None,
                        name,
                        input,
                    });
                }
            }
        }
    }

    let usage = parsed
        .usage_metadata
        .map(|m| Usage {
            prompt_tokens: m.prompt_token_count.unwrap_or(0),
            completion_tokens: m.candidates_token_count.unwrap_or(0),
            total_tokens: m.total_token_count.unwrap_or(0),
            cache_read_tokens: 0,
            cache_write_tokens: 0,
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

#[derive(Debug, Deserialize, Serialize)]
struct GenerateResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Candidate {
    #[serde(default)]
    content: ContentEnvelope,
}

#[derive(Debug, Deserialize, Serialize, Default)]
struct ContentEnvelope {
    #[serde(default)]
    parts: Vec<ContentPart>,
}

#[derive(Debug)]
enum ContentPart {
    Text { text: String },
    FunctionCall { name: String, args: Value },
}

mod content_part_serde {
    use super::*;
    use serde::de::{self, Deserializer, MapAccess, Visitor};
    use serde::ser::SerializeMap;
    use std::fmt;

    impl<'de> serde::Deserialize<'de> for super::ContentPart {
        fn deserialize<D>(deserializer: D) -> Result<super::ContentPart, D::Error>
        where
            D: Deserializer<'de>,
        {
            struct PartVisitor;
            impl<'de> Visitor<'de> for PartVisitor {
                type Value = super::ContentPart;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a Gemini content part")
                }
                fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
                where
                    A: MapAccess<'de>,
                {
                    let mut text: Option<String> = None;
                    let mut name: Option<String> = None;
                    let mut args: Value = Value::Null;
                    while let Some(key) = map.next_key::<String>()? {
                        match key.as_str() {
                            "text" => text = Some(map.next_value()?),
                            "functionCall" => {
                                let body: Value = map.next_value()?;
                                name = body
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                args = body.get("args").cloned().unwrap_or(Value::Null);
                            }
                            _ => {
                                let _: serde::de::IgnoredAny = map.next_value()?;
                            }
                        }
                    }
                    if let Some(text) = text {
                        Ok(super::ContentPart::Text { text })
                    } else if let Some(name) = name {
                        Ok(super::ContentPart::FunctionCall { name, args })
                    } else {
                        Err(de::Error::custom("unsupported content part"))
                    }
                }
            }
            deserializer.deserialize_map(PartVisitor)
        }
    }

    impl serde::Serialize for super::ContentPart {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let mut map = serializer.serialize_map(Some(1))?;
            match self {
                super::ContentPart::Text { text } => map.serialize_entry("text", text)?,
                super::ContentPart::FunctionCall { name, args } => {
                    let body = serde_json::json!({ "name": name, "args": args });
                    map.serialize_entry("functionCall", &body)?
                }
            }
            map.end()
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Default)]
struct UsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: Option<u32>,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: Option<u32>,
    #[serde(default, rename = "totalTokenCount")]
    total_token_count: Option<u32>,
}

pub fn gemini_info() -> ProviderInfo {
    ProviderInfo {
        id: "gemini".to_string(),
        display_name: "Google Gemini".to_string(),
        default_model: "gemini-2.5-flash".to_string(),
        supported_models: vec![
            "gemini-2.5-flash".to_string(),
            "gemini-2.5-pro".to_string(),
            "gemini-1.5-flash".to_string(),
            "gemini-1.5-pro".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("GEMINI_API_KEY".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{Message, ModelSelection, Role};

    #[test]
    fn body_contains_system_instruction_and_user_part() {
        let request = ProviderRequest {
            model: ModelSelection {
                provider: "gemini".to_string(),
                model: "gemini-2.5-flash".to_string(),
            },
            messages: vec![Message::new(Role::User, "你好")],
            tools: Vec::new(),
            system_prompt: Some("be terse".to_string()),
            max_output_tokens: None,
            temperature: None,
            stream: false,
        };
        let body = build_body(&request);
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be terse");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "你好");
    }

    #[test]
    fn parses_response_with_function_call_and_usage() {
        let raw = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "ok"},
                        {"functionCall": {"name": "ls", "args": {"input": "."}}}
                    ]
                }
            }],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4, "totalTokenCount": 7}
        });
        let response = parse_response(raw).expect("parse");
        assert_eq!(response.message.content, "ok");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].name, "ls");
        assert_eq!(response.tool_calls[0].input, ".");
        assert_eq!(response.usage.total_tokens, 7);
    }
}
