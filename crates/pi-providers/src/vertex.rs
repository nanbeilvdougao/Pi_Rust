//! Google Vertex AI provider.
//!
//! Endpoint:
//! `{ENDPOINT}/v1/projects/{PROJECT}/locations/{REGION}/publishers/google/models/{MODEL}:{ACTION}`
//! where ACTION is `streamGenerateContent?alt=sse` for streaming or
//! `generateContent` for non-streaming. The request/response bodies are the
//! same shape Gemini uses (we delegate to gemini's body builders and
//! parser), so the only Vertex-specific bits are:
//!
//! - URL composition (project + region + model id).
//! - Auth: `Authorization: Bearer <VERTEX_ACCESS_TOKEN>`. The user obtains
//!   the token via `gcloud auth print-access-token` or by configuring
//!   `GOOGLE_APPLICATION_CREDENTIALS` and refreshing externally.

use std::env;

use pi_core::{
    Message, PiError, PiErrorKind, PiResult, Role, StreamEvent, StreamSink, ToolInvocation, Usage,
};
use serde_json::Value;

use crate::{
    http_agent, post_json, post_sse_lines, text_stream_events, tool_call_stream_events, Provider,
    ProviderInfo, ProviderRequest, ProviderResponse,
};

#[derive(Debug, Default, Clone)]
pub struct VertexProvider;

impl VertexProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for VertexProvider {
    fn info(&self) -> ProviderInfo {
        vertex_info()
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let url = build_url(&request, false)?;
        let auth = build_auth()?;
        let auth_ref: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let body = crate::gemini::build_body_public(&request);
        let value = post_json(&http_agent(), &url, &body, &auth_ref)?;
        crate::gemini::parse_response_public(value)
    }

    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        let url = build_url(&request, true)?;
        let auth = build_auth()?;
        let auth_ref: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let body = crate::gemini::build_body_public(&request);
        sink.emit(StreamEvent::MessageStart)?;
        let mut text_buf = String::new();
        let mut tool_calls: Vec<ToolInvocation> = Vec::new();
        let mut usage = Usage::default();
        let mut errored: Option<PiError> = None;
        post_sse_lines(&http_agent(), &url, &body, &auth_ref, |line| {
            if sink.cancelled() {
                return Err(PiError::new(PiErrorKind::Cancelled, "已取消"));
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(err) => {
                    errored = Some(PiError::new(
                        PiErrorKind::Provider,
                        format!("Vertex 流式块解析失败：{err}; chunk={line}"),
                    ));
                    return Ok(());
                }
            };
            crate::gemini::accumulate_chunk_public(
                &value,
                sink,
                &mut text_buf,
                &mut tool_calls,
                &mut usage,
            )
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

fn build_url(request: &ProviderRequest, stream: bool) -> PiResult<String> {
    let project = env::var("VERTEX_PROJECT")
        .or_else(|_| env::var("GOOGLE_CLOUD_PROJECT"))
        .map_err(|_| {
            PiError::new(
                PiErrorKind::Provider,
                "缺少 VERTEX_PROJECT 或 GOOGLE_CLOUD_PROJECT",
            )
        })?;
    let region = env::var("VERTEX_REGION").unwrap_or_else(|_| "us-central1".to_string());
    let base = env::var("VERTEX_BASE_URL")
        .unwrap_or_else(|_| format!("https://{region}-aiplatform.googleapis.com"));
    let action = if stream {
        "streamGenerateContent?alt=sse"
    } else {
        "generateContent"
    };
    Ok(format!(
        "{}/v1/projects/{}/locations/{}/publishers/google/models/{}:{}",
        base.trim_end_matches('/'),
        project,
        region,
        request.model.model,
        action,
    ))
}

fn build_auth() -> PiResult<Vec<(String, String)>> {
    let token = env::var("VERTEX_ACCESS_TOKEN")
        .or_else(|_| env::var("GOOGLE_ACCESS_TOKEN"))
        .map_err(|_| {
            PiError::new(
                PiErrorKind::Provider,
                "缺少 VERTEX_ACCESS_TOKEN（运行 `gcloud auth print-access-token` 获取）",
            )
        })?;
    Ok(vec![(
        "authorization".to_string(),
        format!("Bearer {token}"),
    )])
}

pub fn vertex_info() -> ProviderInfo {
    ProviderInfo {
        id: "vertex".to_string(),
        display_name: "Google Vertex AI".to_string(),
        default_model: "gemini-2.5-flash".to_string(),
        supported_models: vec![
            "gemini-2.5-flash".to_string(),
            "gemini-2.5-pro".to_string(),
            "gemini-1.5-pro".to_string(),
            "claude-3-5-sonnet@20241022".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("VERTEX_ACCESS_TOKEN".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn info_has_vertex_id() {
        assert_eq!(vertex_info().id, "vertex");
    }
}
