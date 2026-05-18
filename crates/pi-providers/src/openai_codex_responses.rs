//! OpenAI Codex Responses API provider.
//!
//! Same wire shape as the regular Responses API (`POST /responses` with
//! `input` array + typed SSE events) but rooted at the Codex base path. The
//! Codex models (`gpt-5.5`, `codex-1-mini`, …) are billed and tracked
//! separately from the standard OpenAI API tenant. The default path uses the
//! ChatGPT subscription backend and therefore requires ChatGPT OAuth tokens,
//! not a regular `OPENAI_API_KEY`.
//!
//! Auth precedence:
//! 1. `OPENAI_CODEX_API_KEY` / `OPENAI_CODEX_ACCESS_TOKEN` — explicit bearer
//!    override for debugging.
//! 2. `pi auth login openai-codex` credentials stored in `~/.pi-rust/auth.enc`.
//! 3. Existing upstream TypeScript pi OAuth credentials in
//!    `~/.pi/agent/auth.json`.
//!
//! Base URL precedence:
//! 1. `OPENAI_CODEX_BASE_URL`
//! 2. `https://chatgpt.com/backend-api`
//!
//! Parity target: `packages/ai/src/providers/openai-codex-responses.ts`.

use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, fs};

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

const CODEX_PROVIDER_ID: &str = "openai-codex";
const CODEX_ACCESS_ENV: &str = "OPENAI_CODEX_ACCESS_TOKEN";
const CODEX_REFRESH_ENV: &str = "OPENAI_CODEX_ACCESS_TOKEN_REFRESH";
const CODEX_EXPIRES_ENV: &str = "OPENAI_CODEX_ACCESS_TOKEN_EXPIRES_AT";
const CODEX_ACCOUNT_ENV: &str = "OPENAI_CODEX_ACCOUNT_ID";

#[derive(Debug, Clone)]
struct CodexAuth {
    access_token: String,
    account_id: String,
}

#[derive(Debug, Clone)]
struct StoredCodexAuth {
    access_token: String,
    refresh_token: Option<String>,
    expires_at_unix: Option<u64>,
    account_id: Option<String>,
}

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
        if let Some(idx) = input
            .iter()
            .position(|item| item.get("role").and_then(|v| v.as_str()) == Some("system"))
        {
            let removed = input.remove(idx);
            instructions = removed.get("content").and_then(|v| match v {
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
        obj.insert("text".to_string(), json!({"verbosity": "low"}));
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
        let auth = resolve_codex_auth()?;
        let url = endpoint();
        let body = build_codex_body(&request, false);
        let headers = codex_headers(&auth);
        let header_refs = header_refs(&headers);
        let response = post_json(&http_agent(), &url, &body, &header_refs)?;
        parse_response_pub(response)
    }

    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        let auth = resolve_codex_auth()?;
        let url = endpoint();
        let body = build_codex_body(&request, true);
        let headers = codex_headers(&auth);
        let header_refs = header_refs(&headers);

        sink.emit(StreamEvent::MessageStart)?;
        let mut text_buf = String::new();
        let mut tool_calls: Vec<ToolInvocation> = Vec::new();
        let mut usage = Usage::default();
        let mut errored: Option<PiError> = None;

        post_sse_lines(&http_agent(), &url, &body, &header_refs, |line| {
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
                "response.function_call_arguments.delta" => {
                    if let Some(delta) = value.get("delta").and_then(|v| v.as_str()) {
                        if let Some(call) = find_tool_call_mut(&mut tool_calls, &value) {
                            call.input.push_str(delta);
                            sink.emit(StreamEvent::ToolCallDelta {
                                id: call.id.clone(),
                                name: None,
                                input_delta: delta.to_string(),
                            })?;
                        }
                    }
                }
                "response.function_call_arguments.done" => {
                    if let Some(arguments) = value.get("arguments").and_then(|v| v.as_str()) {
                        if let Some(call) = find_tool_call_mut(&mut tool_calls, &value) {
                            call.input = normalize_tool_arguments(arguments);
                        }
                    }
                }
                "response.output_item.done" => {
                    if let Some(item) = value.get("item") {
                        if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                            let call = parse_function_call_pub(item);
                            if !call.name.is_empty() {
                                upsert_tool_call(&mut tool_calls, call);
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

fn find_tool_call_mut<'a>(
    tool_calls: &'a mut Vec<ToolInvocation>,
    event: &Value,
) -> Option<&'a mut ToolInvocation> {
    let event_id = event
        .get("call_id")
        .or_else(|| event.get("item_id"))
        .or_else(|| event.get("id"))
        .and_then(|v| v.as_str());
    if let Some(id) = event_id {
        if let Some(idx) = tool_calls
            .iter()
            .position(|call| call.id.as_deref() == Some(id))
        {
            return tool_calls.get_mut(idx);
        }
    }
    if tool_calls.len() == 1 {
        return tool_calls.last_mut();
    }
    tool_calls
        .iter_mut()
        .rev()
        .find(|call| call.input.is_empty())
}

fn upsert_tool_call(tool_calls: &mut Vec<ToolInvocation>, call: ToolInvocation) {
    if let Some(idx) = tool_calls
        .iter()
        .position(|existing| existing.id == call.id && existing.name == call.name)
    {
        tool_calls[idx] = call;
    } else {
        tool_calls.push(call);
    }
}

fn normalize_tool_arguments(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get("input")
                .and_then(|input| input.as_str())
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| arguments.to_string())
}

fn resolve_codex_auth() -> PiResult<CodexAuth> {
    if let Some(token) = env_value("OPENAI_CODEX_API_KEY").or_else(|| env_value(CODEX_ACCESS_ENV)) {
        let account_id = env_value(CODEX_ACCOUNT_ENV)
            .map(Ok)
            .unwrap_or_else(|| pi_auth::oauth::openai_codex_account_id(&token))?;
        return Ok(CodexAuth {
            access_token: token,
            account_id,
        });
    }

    use pi_auth::Resolver;
    let mut resolver = pi_auth::layered_resolver()?;
    let stored_access = resolver
        .lookup(CODEX_PROVIDER_ID, CODEX_ACCESS_ENV)?
        .or_else(|| {
            resolver
                .lookup(CODEX_PROVIDER_ID, "OPENAI_CODEX_API_KEY")
                .ok()
                .flatten()
        });
    let stored_refresh = resolver.lookup(CODEX_PROVIDER_ID, CODEX_REFRESH_ENV)?;
    let ts_pi_auth = if stored_access.is_none() && stored_refresh.is_none() {
        load_ts_pi_codex_auth()?
    } else {
        None
    };

    let access = stored_access.or_else(|| {
        ts_pi_auth
            .as_ref()
            .map(|auth| auth.access_token.clone())
            .filter(|token| !token.is_empty())
    });
    let refresh = stored_refresh.or_else(|| {
        ts_pi_auth
            .as_ref()
            .and_then(|auth| auth.refresh_token.clone())
            .filter(|token| !token.is_empty())
    });
    let needs_refresh = stored_token_needs_refresh(&resolver)?
        || ts_pi_auth
            .as_ref()
            .and_then(|auth| auth.expires_at_unix)
            .map(|expires| now_unix().saturating_add(60) >= expires)
            .unwrap_or(false);

    let mut access_token = match access {
        Some(token) if !token.is_empty() && !needs_refresh => token,
        _ => {
            let refresh_token = refresh.ok_or_else(|| {
                PiError::new(
                    PiErrorKind::Provider,
                    "缺少 OpenAI Codex OAuth 凭据。请运行 `pi auth login openai-codex` 或在原版 pi 中 `/login`，不要使用 OPENAI_API_KEY。",
                )
            })?;
            refresh_codex_token(&mut resolver, &refresh_token)?
        }
    };

    if access_token.is_empty() {
        return Err(PiError::new(
            PiErrorKind::Provider,
            "OpenAI Codex OAuth access token 为空。请重新运行 `pi auth login openai-codex`。",
        ));
    }

    let account_id = match resolver.lookup(CODEX_PROVIDER_ID, CODEX_ACCOUNT_ENV)? {
        Some(id) if !id.is_empty() => id,
        _ if ts_pi_auth
            .as_ref()
            .and_then(|auth| auth.account_id.as_deref())
            .is_some() =>
        {
            ts_pi_auth
                .as_ref()
                .and_then(|auth| auth.account_id.clone())
                .unwrap_or_default()
        }
        _ => {
            let id = pi_auth::oauth::openai_codex_account_id(&access_token)?;
            resolver.store(CODEX_PROVIDER_ID, CODEX_ACCOUNT_ENV, &id)?;
            id
        }
    };

    Ok(CodexAuth {
        access_token: std::mem::take(&mut access_token),
        account_id,
    })
}

fn load_ts_pi_codex_auth() -> PiResult<Option<StoredCodexAuth>> {
    let home = match env::var("HOME") {
        Ok(home) => home,
        Err(_) => return Ok(None),
    };
    let path = std::path::PathBuf::from(home)
        .join(".pi")
        .join("agent")
        .join("auth.json");
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).map_err(|err| {
        PiError::new(
            PiErrorKind::Io,
            format!("读取原版 pi auth 文件 {} 失败：{err}", path.display()),
        )
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|err| {
        PiError::new(
            PiErrorKind::Config,
            format!("解析原版 pi auth 文件 {} 失败：{err}", path.display()),
        )
    })?;
    let Some(credential) = value.get(CODEX_PROVIDER_ID) else {
        return Ok(None);
    };
    if credential.get("type").and_then(|v| v.as_str()) != Some("oauth") {
        return Ok(None);
    }
    let Some(access_token) = credential
        .get("access")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
    else {
        return Ok(None);
    };
    let refresh_token = credential
        .get("refresh")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    let expires_at_unix = credential
        .get("expires")
        .and_then(|v| v.as_u64())
        .map(|raw| {
            if raw > 10_000_000_000 {
                raw / 1000
            } else {
                raw
            }
        });
    let account_id = credential
        .get("accountId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(ToString::to_string);
    Ok(Some(StoredCodexAuth {
        access_token,
        refresh_token,
        expires_at_unix,
        account_id,
    }))
}

fn env_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn stored_token_needs_refresh(resolver: &impl pi_auth::Resolver) -> PiResult<bool> {
    let Some(raw) = resolver.lookup(CODEX_PROVIDER_ID, CODEX_EXPIRES_ENV)? else {
        return Ok(false);
    };
    let expires_at = raw.parse::<u64>().map_err(|err| {
        PiError::new(
            PiErrorKind::Config,
            format!("{CODEX_EXPIRES_ENV} 不是有效 UNIX 时间戳：{err}"),
        )
    })?;
    Ok(now_unix().saturating_add(60) >= expires_at)
}

fn refresh_codex_token(
    resolver: &mut impl pi_auth::Resolver,
    refresh_token: &str,
) -> PiResult<String> {
    let config = pi_auth::oauth::openai_codex_config();
    let tokens = pi_auth::oauth::refresh(&config, refresh_token)?;
    resolver.store(CODEX_PROVIDER_ID, CODEX_ACCESS_ENV, &tokens.access_token)?;
    if let Some(refresh) = tokens.refresh_token.as_deref() {
        resolver.store(CODEX_PROVIDER_ID, CODEX_REFRESH_ENV, refresh)?;
    }
    if let Some(expires_at) = tokens.expires_at_unix {
        resolver.store(
            CODEX_PROVIDER_ID,
            CODEX_EXPIRES_ENV,
            &expires_at.to_string(),
        )?;
    }
    let account_id = pi_auth::oauth::openai_codex_account_id(&tokens.access_token)?;
    resolver.store(CODEX_PROVIDER_ID, CODEX_ACCOUNT_ENV, &account_id)?;
    Ok(tokens.access_token)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn codex_headers(auth: &CodexAuth) -> Vec<(&'static str, String)> {
    vec![
        (
            "authorization",
            format!("Bearer {}", auth.access_token.as_str()),
        ),
        ("chatgpt-account-id", auth.account_id.clone()),
        ("originator", "pi".to_string()),
        ("openai-beta", "responses=experimental".to_string()),
    ]
}

fn header_refs<'a>(headers: &'a [(&'static str, String)]) -> Vec<(&'a str, &'a str)> {
    headers
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect()
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
        requires_api_key_env: Some(CODEX_ACCESS_ENV.to_string()),
    }
}

pub fn openai_codex_info() -> ProviderInfo {
    let mut info = openai_codex_responses_info();
    info.id = CODEX_PROVIDER_ID.to_string();
    info.display_name = "OpenAI Codex (ChatGPT Subscription)".to_string();
    info
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
        assert!(info
            .supported_models
            .iter()
            .any(|m| m.starts_with("codex-")));
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
