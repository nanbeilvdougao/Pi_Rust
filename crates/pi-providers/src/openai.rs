//! OpenAI-compatible Chat Completions clients.
//!
//! Used by:
//! - real OpenAI (`api.openai.com`)
//! - Moonshot (月之暗面), DeepSeek, Qwen (DashScope), Zhipu GLM, MiniMax
//!
//! The wire format is identical for non-streaming and SSE streaming, so the
//! same parsing path serves all of them. Differences live only in the URL,
//! the API-key environment variable, and the default model list.

use std::env;

use pi_core::{
    Message, PiError, PiErrorKind, PiResult, Role, StreamEvent, StreamSink, ToolInvocation, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    build_messages_with_system, http_agent, post_json, post_sse_lines, read_api_key,
    text_stream_events, tool_call_stream_events, tools_to_openai, Provider, ProviderInfo,
    ProviderRequest, ProviderResponse,
};

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleProvider {
    info: ProviderInfo,
    endpoint: String,
    api_key_env: String,
}

impl OpenAiCompatibleProvider {
    pub fn moonshot() -> Self {
        Self {
            info: moonshot_info(),
            endpoint: "https://api.moonshot.cn/v1/chat/completions".to_string(),
            api_key_env: "MOONSHOT_API_KEY".to_string(),
        }
    }

    pub fn deepseek() -> Self {
        Self {
            info: deepseek_info(),
            endpoint: "https://api.deepseek.com/chat/completions".to_string(),
            api_key_env: "DEEPSEEK_API_KEY".to_string(),
        }
    }

    pub fn qwen() -> Self {
        Self {
            info: qwen_info(),
            endpoint: "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions"
                .to_string(),
            api_key_env: "DASHSCOPE_API_KEY".to_string(),
        }
    }

    pub fn zhipu() -> Self {
        Self {
            info: zhipu_info(),
            endpoint: "https://open.bigmodel.cn/api/paas/v4/chat/completions".to_string(),
            api_key_env: "ZHIPU_API_KEY".to_string(),
        }
    }

    pub fn minimax() -> Self {
        Self {
            info: minimax_info(),
            endpoint: "https://api.minimax.chat/v1/text/chatcompletion_v2".to_string(),
            api_key_env: "MINIMAX_API_KEY".to_string(),
        }
    }
}

impl Provider for OpenAiCompatibleProvider {
    fn info(&self) -> ProviderInfo {
        self.info.clone()
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        complete_chat(&self.info, &self.endpoint, &self.api_key_env, request)
    }

    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        stream_chat(&self.info, &self.endpoint, &self.api_key_env, request, sink)
    }
}

#[derive(Debug, Clone, Default)]
pub struct OpenAiProvider;

impl OpenAiProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for OpenAiProvider {
    fn info(&self) -> ProviderInfo {
        openai_info()
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let endpoint =
            env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let endpoint = format!("{}/chat/completions", endpoint.trim_end_matches('/'));
        let info = self.info();
        complete_chat(&info, &endpoint, "OPENAI_API_KEY", request)
    }

    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        let endpoint =
            env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let endpoint = format!("{}/chat/completions", endpoint.trim_end_matches('/'));
        let info = self.info();
        stream_chat(&info, &endpoint, "OPENAI_API_KEY", request, sink)
    }
}

fn complete_chat(
    info: &ProviderInfo,
    endpoint: &str,
    api_key_env: &str,
    request: ProviderRequest,
) -> PiResult<ProviderResponse> {
    let api_key = read_api_key(api_key_env, &info.id)?;
    let body = build_request_body(&request, false);
    let agent = http_agent();
    let auth = format!("Bearer {api_key}");
    let response = post_json(&agent, endpoint, &body, &[("authorization", auth.as_str())])?;
    let parsed: ChatResponse = serde_json::from_value(response.clone()).map_err(|err| {
        PiError::new(
            PiErrorKind::Provider,
            format!("{} 响应解析失败：{err}; body={response}", info.id),
        )
    })?;
    let (message, tool_calls, usage) = extract_message(parsed)?;
    let stream_events = if tool_calls.is_empty() {
        text_stream_events(&message.content)
    } else {
        tool_call_stream_events(&tool_calls)
    };
    let events = if message.content.is_empty() {
        Vec::new()
    } else {
        vec![message.content.clone()]
    };

    Ok(ProviderResponse {
        message,
        events,
        stream_events,
        tool_calls,
        usage,
    })
}

fn stream_chat(
    info: &ProviderInfo,
    endpoint: &str,
    api_key_env: &str,
    request: ProviderRequest,
    sink: &mut dyn StreamSink,
) -> PiResult<ProviderResponse> {
    let api_key = read_api_key(api_key_env, &info.id)?;
    let body = build_request_body(&request, true);
    let agent = http_agent();
    let auth = format!("Bearer {api_key}");

    sink.emit(StreamEvent::MessageStart)?;

    let mut text_buf = String::new();
    let mut tool_calls: Vec<ToolCallBuilder> = Vec::new();
    let mut usage = Usage::default();
    let mut errored: Option<PiError> = None;

    post_sse_lines(
        &agent,
        endpoint,
        &body,
        &[("authorization", auth.as_str())],
        |line| {
            if sink.cancelled() {
                return Err(PiError::new(PiErrorKind::Cancelled, "已取消"));
            }
            if line.trim() == "[DONE]" {
                return Ok(());
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

            if let Some(choice) = value
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|arr| arr.first())
            {
                if let Some(delta) = choice.get("delta") {
                    if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                        if !content.is_empty() {
                            text_buf.push_str(content);
                            sink.emit(StreamEvent::TextDelta(content.to_string()))?;
                        }
                    }
                    if let Some(reasoning) = delta.get("reasoning_content").and_then(|v| v.as_str())
                    {
                        if !reasoning.is_empty() {
                            sink.emit(StreamEvent::ThinkingDelta(reasoning.to_string()))?;
                        }
                    }
                    if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                        for call in calls {
                            let index = call
                                .get("index")
                                .and_then(|i| i.as_u64())
                                .map(|i| i as usize)
                                .unwrap_or(0);
                            while tool_calls.len() <= index {
                                tool_calls.push(ToolCallBuilder::default());
                            }
                            let builder = &mut tool_calls[index];
                            if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
                                builder.id = Some(id.to_string());
                            }
                            if let Some(function) = call.get("function") {
                                if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
                                    if !name.is_empty() {
                                        builder.name = Some(name.to_string());
                                    }
                                }
                                if let Some(args) =
                                    function.get("arguments").and_then(|v| v.as_str())
                                {
                                    if !args.is_empty() {
                                        builder.arguments.push_str(args);
                                        sink.emit(StreamEvent::ToolCallDelta {
                                            id: builder.id.clone(),
                                            name: builder.name.clone(),
                                            input_delta: args.to_string(),
                                        })?;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some(usage_value) = value.get("usage") {
                if let Some(u) = parse_openai_usage(usage_value) {
                    usage = u;
                    sink.emit(StreamEvent::UsageDelta(usage.clone()))?;
                }
            }
            Ok(())
        },
    )?;

    sink.emit(StreamEvent::MessageDone)?;

    if let Some(err) = errored {
        return Err(err);
    }

    let invocations: Vec<ToolInvocation> = tool_calls
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

    let mut response_message = Message::new(Role::Assistant, text_buf.clone());
    response_message.tool_calls = invocations.clone();
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
        message: response_message,
        events,
        stream_events,
        tool_calls: invocations,
        usage,
    })
}

fn parse_openai_usage(value: &Value) -> Option<Usage> {
    let prompt_tokens = value.get("prompt_tokens")?.as_u64().unwrap_or(0) as u32;
    let completion_tokens = value
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let total_tokens = value
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let cache_read_tokens = value
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cache_read_tokens,
        cache_write_tokens: 0,
    })
}

#[derive(Default, Debug, Clone)]
struct ToolCallBuilder {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

fn build_request_body(request: &ProviderRequest, stream: bool) -> Value {
    let messages = build_messages_with_system(request);
    let tools = tools_to_openai(&request.tools);
    let mut body = json!({
        "model": request.model.model,
        "messages": messages,
        "stream": stream,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
        body["tool_choice"] = json!("auto");
    }
    if let Some(temp) = request.temperature {
        body["temperature"] = json!(temp);
    }
    if let Some(max) = request.max_output_tokens {
        body["max_tokens"] = json!(max);
    }
    if stream {
        body["stream_options"] = json!({ "include_usage": true });
    }
    body
}

#[derive(Debug, Deserialize, Serialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ChatChoice {
    #[serde(default)]
    message: ChatChoiceMessage,
}

#[derive(Debug, Deserialize, Serialize, Default)]
struct ChatChoiceMessage {
    #[serde(default)]
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatToolCall>,
    #[serde(default)]
    reasoning_content: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ChatToolCall {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    kind: String,
    function: ChatToolFunction,
}

#[derive(Debug, Deserialize, Serialize)]
struct ChatToolFunction {
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
}

fn extract_message(parsed: ChatResponse) -> PiResult<(Message, Vec<ToolInvocation>, Usage)> {
    let choice = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| PiError::new(PiErrorKind::Provider, "chat 响应中没有 choices"))?;
    let content = choice.message.content.unwrap_or_default();
    let tool_calls: Vec<ToolInvocation> = choice
        .message
        .tool_calls
        .into_iter()
        .map(|call| ToolInvocation {
            id: call.id,
            name: call.function.name,
            input: extract_input_field(&call.function.arguments).unwrap_or(call.function.arguments),
        })
        .collect();

    let usage = parsed
        .usage
        .map(|usage| Usage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        })
        .unwrap_or_default();

    let mut message = Message::new(Role::Assistant, content);
    message.tool_calls = tool_calls.clone();
    Ok((message, tool_calls, usage))
}

fn extract_input_field(arguments: &str) -> Option<String> {
    let value: Value = serde_json::from_str(arguments).ok()?;
    value
        .get("input")
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
}

pub fn openai_info() -> ProviderInfo {
    ProviderInfo {
        id: "openai".to_string(),
        display_name: "OpenAI".to_string(),
        default_model: "gpt-4o-mini".to_string(),
        supported_models: vec![
            "gpt-4o-mini".to_string(),
            "gpt-4o".to_string(),
            "gpt-4.1".to_string(),
            "gpt-4.1-mini".to_string(),
            "o4-mini".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("OPENAI_API_KEY".to_string()),
    }
}

pub fn moonshot_info() -> ProviderInfo {
    ProviderInfo {
        id: "moonshot".to_string(),
        display_name: "Moonshot 月之暗面".to_string(),
        default_model: "moonshot-v1-8k".to_string(),
        supported_models: vec![
            "moonshot-v1-8k".to_string(),
            "moonshot-v1-32k".to_string(),
            "moonshot-v1-128k".to_string(),
            "kimi-k2-0905-preview".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("MOONSHOT_API_KEY".to_string()),
    }
}

pub fn deepseek_info() -> ProviderInfo {
    ProviderInfo {
        id: "deepseek".to_string(),
        display_name: "DeepSeek".to_string(),
        default_model: "deepseek-chat".to_string(),
        supported_models: vec![
            "deepseek-chat".to_string(),
            "deepseek-reasoner".to_string(),
            "deepseek-coder".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("DEEPSEEK_API_KEY".to_string()),
    }
}

pub fn qwen_info() -> ProviderInfo {
    ProviderInfo {
        id: "qwen".to_string(),
        display_name: "通义千问".to_string(),
        default_model: "qwen-plus".to_string(),
        supported_models: vec![
            "qwen-plus".to_string(),
            "qwen-turbo".to_string(),
            "qwen-max".to_string(),
            "qwen2.5-coder-32b-instruct".to_string(),
            "qwen3-coder".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("DASHSCOPE_API_KEY".to_string()),
    }
}

pub fn zhipu_info() -> ProviderInfo {
    ProviderInfo {
        id: "zhipu".to_string(),
        display_name: "智谱 GLM".to_string(),
        default_model: "glm-4-plus".to_string(),
        supported_models: vec![
            "glm-4-plus".to_string(),
            "glm-4-air".to_string(),
            "glm-4-flash".to_string(),
            "codegeex-4".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("ZHIPU_API_KEY".to_string()),
    }
}

pub fn minimax_info() -> ProviderInfo {
    ProviderInfo {
        id: "minimax".to_string(),
        display_name: "MiniMax".to_string(),
        default_model: "abab6.5s-chat".to_string(),
        supported_models: vec![
            "abab6.5s-chat".to_string(),
            "abab6.5g-chat".to_string(),
            "MiniMax-M1".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("MINIMAX_API_KEY".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{Message, Role, ToolSchema};

    #[test]
    fn request_body_includes_tools_when_present() {
        let request = ProviderRequest {
            model: pi_core::ModelSelection {
                provider: "deepseek".to_string(),
                model: "deepseek-chat".to_string(),
            },
            messages: vec![Message::new(Role::User, "hi")],
            tools: vec![ToolSchema {
                name: "ls".to_string(),
                description: "list".to_string(),
                input_shape: "path".to_string(),
                parameters: None,
                mutates: false,
            }],
            system_prompt: Some("be terse".to_string()),
            max_output_tokens: Some(128),
            temperature: Some(0.2),
            stream: true,
        };

        let body = build_request_body(&request, true);
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["stream"], true);
        let temp = body["temperature"]
            .as_f64()
            .expect("temperature is a number");
        assert!((temp - 0.2).abs() < 1e-6);
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["tools"][0]["function"]["name"], "ls");
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn parses_non_streaming_response_with_tool_call() {
        let raw = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "ls", "arguments": "{\"input\":\".\"}"}
                    }]
                }
            }],
            "usage": { "prompt_tokens": 4, "completion_tokens": 0, "total_tokens": 4 }
        });
        let parsed: ChatResponse = serde_json::from_value(raw).expect("parse");
        let (message, calls, usage) = extract_message(parsed).expect("extract");
        assert_eq!(message.role, Role::Assistant);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "ls");
        assert_eq!(calls[0].input, ".");
        assert_eq!(usage.prompt_tokens, 4);
    }
}
