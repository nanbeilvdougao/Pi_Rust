//! Local Ollama provider. Uses the `/api/chat` JSON-streaming endpoint
//! (newline-delimited JSON, not SSE) for low-latency local inference.

use std::env;

use pi_core::{
    Message, PiError, PiErrorKind, PiResult, Role, StreamEvent, StreamSink, ToolInvocation, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    http_agent, text_stream_events, tool_call_stream_events, Provider, ProviderInfo,
    ProviderRequest, ProviderResponse,
};

#[derive(Debug, Default, Clone)]
pub struct OllamaProvider;

impl OllamaProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for OllamaProvider {
    fn info(&self) -> ProviderInfo {
        ollama_info()
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let url = ollama_url("/api/chat")?;
        let body = build_body(&request, false);
        let agent = http_agent();
        let response = match agent.post(&url).send_json(body) {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let text = response.into_string().unwrap_or_default();
                return Err(PiError::new(
                    PiErrorKind::Provider,
                    format!("Ollama 请求失败：HTTP {status}：{text}"),
                ));
            }
            Err(ureq::Error::Transport(transport)) => {
                return Err(PiError::new(
                    PiErrorKind::Network,
                    format!("Ollama 传输错误：{transport}"),
                ));
            }
        };
        let text = response.into_string().map_err(|err| {
            PiError::new(PiErrorKind::Network, format!("读取 Ollama 响应失败：{err}"))
        })?;
        let parsed: ChatResponse = serde_json::from_str(&text).map_err(|err| {
            PiError::new(
                PiErrorKind::Provider,
                format!("Ollama 响应解析失败：{err}; body={text}"),
            )
        })?;

        let content = parsed.message.content.unwrap_or_default();
        let tool_calls: Vec<ToolInvocation> = parsed
            .message
            .tool_calls
            .into_iter()
            .map(|call| ToolInvocation {
                id: None,
                name: call.function.name,
                input: arguments_to_input(&call.function.arguments),
            })
            .collect();

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
            usage: Usage {
                prompt_tokens: parsed.prompt_eval_count.unwrap_or(0),
                completion_tokens: parsed.eval_count.unwrap_or(0),
                total_tokens: parsed.prompt_eval_count.unwrap_or(0)
                    + parsed.eval_count.unwrap_or(0),
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        })
    }

    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        use std::io::{BufRead, BufReader};

        let url = ollama_url("/api/chat")?;
        let body = build_body(&request, true);
        let agent = http_agent();
        let response = match agent.post(&url).send_json(body) {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let text = response.into_string().unwrap_or_default();
                return Err(PiError::new(
                    PiErrorKind::Provider,
                    format!("Ollama 请求失败：HTTP {status}：{text}"),
                ));
            }
            Err(ureq::Error::Transport(transport)) => {
                return Err(PiError::new(
                    PiErrorKind::Network,
                    format!("Ollama 传输错误：{transport}"),
                ));
            }
        };

        sink.emit(StreamEvent::MessageStart)?;
        let reader = BufReader::new(response.into_reader());
        let mut text_buf = String::new();
        let mut tool_calls: Vec<ToolInvocation> = Vec::new();
        let mut usage = Usage::default();
        for line in reader.lines() {
            if sink.cancelled() {
                return Err(PiError::new(PiErrorKind::Cancelled, "已取消"));
            }
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let chunk: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if let Some(content) = chunk
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|v| v.as_str())
            {
                if !content.is_empty() {
                    text_buf.push_str(content);
                    sink.emit(StreamEvent::TextDelta(content.to_string()))?;
                }
            }
            if let Some(calls) = chunk
                .get("message")
                .and_then(|m| m.get("tool_calls"))
                .and_then(|v| v.as_array())
            {
                for call in calls {
                    let name = call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let arguments = call
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    let input = arguments_value_to_input(&arguments);
                    let invocation = ToolInvocation {
                        id: None,
                        name: name.clone(),
                        input: input.clone(),
                    };
                    sink.emit(StreamEvent::ToolCallDelta {
                        id: None,
                        name: Some(name),
                        input_delta: input,
                    })?;
                    tool_calls.push(invocation);
                }
            }
            if chunk.get("done").and_then(|v| v.as_bool()) == Some(true) {
                usage.prompt_tokens = chunk
                    .get("prompt_eval_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                usage.completion_tokens = chunk
                    .get("eval_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32;
                usage.total_tokens = usage.prompt_tokens + usage.completion_tokens;
                sink.emit(StreamEvent::UsageDelta(usage.clone()))?;
                break;
            }
        }
        sink.emit(StreamEvent::MessageDone)?;

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

fn ollama_url(path: &str) -> PiResult<String> {
    let base = env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
    Ok(format!("{}{}", base.trim_end_matches('/'), path))
}

fn build_body(request: &ProviderRequest, stream: bool) -> Value {
    let mut messages: Vec<Value> = Vec::new();
    if let Some(system) = &request.system_prompt {
        messages.push(json!({"role":"system","content": system}));
    }
    for message in &request.messages {
        match message.role {
            Role::Tool => messages.push(json!({
                "role": "tool",
                "content": message.content,
                "tool_call_id": message.tool_call_id.clone(),
            })),
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
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters_or_default(),
                }
            })
        })
        .collect();
    let mut body = json!({
        "model": request.model.model,
        "messages": messages,
        "stream": stream,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    body
}

fn arguments_to_input(arguments: &Value) -> String {
    match arguments {
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(parsed) => parsed
                .get("input")
                .and_then(|v| v.as_str())
                .map(|v| v.to_string())
                .unwrap_or_else(|| s.clone()),
            Err(_) => s.clone(),
        },
        _ => arguments_value_to_input(arguments),
    }
}

fn arguments_value_to_input(arguments: &Value) -> String {
    if let Some(input) = arguments.get("input").and_then(|v| v.as_str()) {
        return input.to_string();
    }
    serde_json::to_string(arguments).unwrap_or_default()
}

#[derive(Debug, Deserialize, Serialize)]
struct ChatResponse {
    #[serde(default)]
    message: ChatMessage,
    #[serde(default)]
    eval_count: Option<u32>,
    #[serde(default)]
    prompt_eval_count: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize, Default)]
struct ChatMessage {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatToolCall>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ChatToolCall {
    function: ChatToolFunction,
}

#[derive(Debug, Deserialize, Serialize)]
struct ChatToolFunction {
    name: String,
    #[serde(default)]
    arguments: Value,
}

pub fn ollama_info() -> ProviderInfo {
    ProviderInfo {
        id: "ollama".to_string(),
        display_name: "Ollama 本地模型".to_string(),
        default_model: "qwen2.5:7b".to_string(),
        supported_models: vec![
            "qwen2.5:7b".to_string(),
            "qwen2.5:3b".to_string(),
            "qwen2.5-coder:7b".to_string(),
            "llama3.1:8b".to_string(),
            "deepseek-r1:7b".to_string(),
            "deepseek-coder:6.7b".to_string(),
        ],
        local_first: true,
        requires_api_key_env: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{Message, ModelSelection, Role};

    #[test]
    fn body_has_chat_shape() {
        let request = ProviderRequest {
            model: ModelSelection {
                provider: "ollama".into(),
                model: "qwen2.5:7b".into(),
            },
            messages: vec![Message::new(Role::User, "你好")],
            tools: Vec::new(),
            system_prompt: Some("你是 Pi".into()),
            max_output_tokens: None,
            temperature: None,
            stream: true,
        };
        let body = build_body(&request, true);
        assert_eq!(body["model"], "qwen2.5:7b");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "你好");
        assert_eq!(body["stream"], true);
    }
}
