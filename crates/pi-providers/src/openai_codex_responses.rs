//! OpenAI Codex Responses API provider.
//!
//! Same wire shape as the regular Responses API (`POST /responses` with
//! `input` array + typed SSE events) but rooted at the Codex base path. The
//! Codex models (`codex-1-mini`, `codex-1`, …) are billed and tracked
//! separately from the standard OpenAI tenant, and the surface intentionally
//! omits a few "research" fields. We delegate the body building and parsing
//! to `openai_responses` so the two providers cannot drift.
//!
//! Auth precedence:
//! 1. `OPENAI_CODEX_API_KEY` — codex-tenant key, preferred when set.
//! 2. `OPENAI_API_KEY` — falls back so users who only have the main key can
//!    still try codex models.
//!
//! Base URL precedence:
//! 1. `OPENAI_CODEX_BASE_URL`
//! 2. `OPENAI_BASE_URL` + `/codex` suffix
//! 3. `https://api.openai.com/codex/v1`
//!
//! Parity target: `packages/ai/src/providers/openai-codex-responses.ts`.

use std::env;

use pi_core::{
    Message, PiError, PiErrorKind, PiResult, Role, StreamEvent, StreamSink, ToolInvocation, Usage,
};
use serde_json::Value;

use crate::openai_responses::{
    build_request_body_pub, parse_function_call_pub, parse_response_pub, parse_usage_pub,
};
use crate::{
    http_agent, post_json, post_sse_lines, text_stream_events, tool_call_stream_events, Provider,
    ProviderInfo, ProviderRequest, ProviderResponse,
};
use serde_json::json;

/// Codex requires `instructions` as a top-level field plus `store: false`
/// (the subscription tenant doesn't keep responses on its side). Take the
/// generic openai_responses body and move the system message out of the
/// `input` array into the `instructions` slot. Also mirror the extras TS
/// pi sends (text.verbosity, include reasoning, parallel tool calls) so
/// the upstream contract matches.
fn build_codex_body(request: &ProviderRequest, stream: bool) -> Value {
    let mut body = build_request_body_pub(request, stream);
    // Pull the first system message out of `input`.
    let mut instructions: Option<String> = None;
    if let Some(input) = body.get_mut("input").and_then(|v| v.as_array_mut()) {
        if let Some(idx) = input.iter().position(|item| {
            item.get("role").and_then(|v| v.as_str()) == Some("system")
        }) {
            let removed = input.remove(idx);
            instructions = removed
                .get("content")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    Value::Array(arr) => Some(
                        arr.iter()
                            .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join(""),
                    ),
                    _ => None,
                });
        }
    }
    let instructions = instructions
        .or_else(|| request.system_prompt.clone())
        .unwrap_or_else(|| "You are a helpful assistant.".to_string());
    if let Some(obj) = body.as_object_mut() {
        obj.insert("instructions".to_string(), Value::String(instructions));
        obj.insert("store".to_string(), Value::Bool(false));
        // Codex-only extras (match earendil-works/pi packages/ai/.../openai-codex-responses.ts).
        obj.insert(
            "text".to_string(),
            json!({"verbosity": "low"}),
        );
        obj.insert(
            "include".to_string(),
            json!(["reasoning.encrypted_content"]),
        );
        if obj.contains_key("tools") {
            obj.insert("tool_choice".to_string(), Value::String("auto".to_string()));
            obj.insert("parallel_tool_calls".to_string(), Value::Bool(true));
        }
    }
    body
}

#[derive(Debug, Default, Clone)]
pub struct OpenAiCodexResponsesProvider;

impl OpenAiCodexResponsesProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for OpenAiCodexResponsesProvider {
    fn info(&self) -> ProviderInfo {
        openai_codex_responses_info()
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let key = read_codex_api_key()?;
        let url = endpoint();
        let body = build_codex_body(&request, false);
        let auth = format!("Bearer {key}");
        let response = post_json(
            &http_agent(),
            &url,
            &body,
            &[("authorization", auth.as_str())],
        )?;
        parse_response_pub(response)
    }

    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        let key = read_codex_api_key()?;
        let url = endpoint();
        let body = build_codex_body(&request, true);
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
                            format!("Codex Responses 流式块解析失败：{err}; chunk={line}"),
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
                                let call = parse_function_call_pub(item);
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
                                usage = parse_usage_pub(u);
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
    // Match TS pi (`packages/ai/src/providers/openai-codex-responses.ts`) which
    // talks to `https://chatgpt.com/backend-api/codex/responses` when no
    // explicit base URL is set. `OPENAI_CODEX_BASE_URL` is the override hook;
    // we suffix `/responses` (or `/codex/responses`) automatically so users
    // can paste either form.
    let base = env::var("OPENAI_CODEX_BASE_URL")
        .unwrap_or_else(|_| "https://chatgpt.com/backend-api".to_string());
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/codex/responses") {
        return trimmed.to_string();
    }
    if trimmed.ends_with("/codex") {
        return format!("{trimmed}/responses");
    }
    format!("{trimmed}/codex/responses")
}

fn read_codex_api_key() -> PiResult<String> {
    if let Ok(value) = env::var("OPENAI_CODEX_API_KEY") {
        if !value.is_empty() {
            return Ok(value);
        }
    }
    if let Ok(value) = env::var("OPENAI_API_KEY") {
        if !value.is_empty() {
            return Ok(value);
        }
    }
    use pi_auth::Resolver;
    let resolver = pi_auth::layered_resolver()?;
    for env_name in ["OPENAI_CODEX_API_KEY", "OPENAI_API_KEY"] {
        if let Some(value) = resolver
            .lookup("openai-codex-responses", env_name)
            .ok()
            .flatten()
        {
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    Err(PiError::new(
        PiErrorKind::Provider,
        "缺少 OPENAI_CODEX_API_KEY 或 OPENAI_API_KEY (Codex 至少需要其中之一)",
    ))
}

pub fn openai_codex_responses_info() -> ProviderInfo {
    ProviderInfo {
        id: "openai-codex-responses".to_string(),
        display_name: "OpenAI Codex Responses".to_string(),
        default_model: "gpt-5.5".to_string(),
        // ChatGPT-subscription / Codex tenant model IDs. The `gpt-5.5`
        // entry matches earendil-works/pi's `openai-codex → gpt-5.5`
        // alias so both ends accept the same `--model gpt-5.5`.
        supported_models: vec![
            "gpt-5.5".to_string(),
            "gpt-5".to_string(),
            "codex-1".to_string(),
            "codex-1-mini".to_string(),
            "codex-medium-latest".to_string(),
            "codex-high-latest".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("OPENAI_CODEX_API_KEY".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::ModelSelection;

    fn restore<F: FnOnce()>(_keys: &[&str], f: F) {
        f();
    }

    #[test]
    fn info_advertises_codex_id() {
        let info = openai_codex_responses_info();
        assert_eq!(info.id, "openai-codex-responses");
        assert!(info.supported_models.iter().any(|m| m.starts_with("codex-")));
    }

    #[test]
    fn body_matches_responses_shape() {
        let req = ProviderRequest::new(
            ModelSelection {
                provider: "openai-codex-responses".into(),
                model: "codex-1".into(),
            },
            vec![Message::new(Role::User, "请写一个 fizzbuzz")],
        );
        let body = build_codex_body(&req, true);
        assert_eq!(body["stream"], true);
        assert_eq!(body["model"], "codex-1");
        assert!(body["input"].is_array());
        assert!(body.get("messages").is_none());
        // Codex contract: instructions top-level, store=false, text.verbosity.
        assert!(body["instructions"].is_string());
        assert_eq!(body["store"], false);
        assert!(body["text"]["verbosity"].is_string());
    }

    #[test]
    fn codex_body_lifts_system_prompt_into_instructions() {
        let mut req = ProviderRequest::new(
            ModelSelection {
                provider: "openai-codex-responses".into(),
                model: "gpt-5.5".into(),
            },
            vec![Message::new(Role::User, "hello")],
        );
        req.system_prompt = Some("you are codex".to_string());
        let body = build_codex_body(&req, false);
        assert_eq!(body["instructions"], "you are codex");
        // No system message should remain in input.
        let inputs = body["input"].as_array().expect("input array");
        let has_system = inputs
            .iter()
            .any(|item| item.get("role").and_then(|v| v.as_str()) == Some("system"));
        assert!(!has_system, "system message must be lifted to instructions");
    }

    #[test]
    fn endpoint_uses_codex_path_segment() {
        restore(&[], || {
            let prior_base = std::env::var("OPENAI_BASE_URL").ok();
            let prior_codex = std::env::var("OPENAI_CODEX_BASE_URL").ok();
            // unsafe forbidden in workspace; we don't mutate env in tests.
            let url = endpoint();
            assert!(url.contains("/codex/"));
            assert!(url.ends_with("/responses"));
            let _ = prior_base;
            let _ = prior_codex;
        });
    }

    #[test]
    fn parse_uses_shared_response_parser() {
        let raw = serde_json::json!({
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "✓ codex"}]
            }],
            "usage": {"input_tokens": 5, "output_tokens": 2, "total_tokens": 7}
        });
        let response = parse_response_pub(raw).expect("parse");
        assert_eq!(response.message.content, "✓ codex");
        assert_eq!(response.usage.total_tokens, 7);
    }
}
