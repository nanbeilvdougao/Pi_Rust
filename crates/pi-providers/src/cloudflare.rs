//! Cloudflare Workers AI provider.
//!
//! Endpoint: `https://api.cloudflare.com/client/v4/accounts/{ACCOUNT}/ai/run/{MODEL}`
//! Auth: `Authorization: Bearer <CLOUDFLARE_API_TOKEN>`.
//! Body: openai-compat `{messages, stream}` for chat-style models; the
//! response is wrapped in `{result: …, success: true}`. We unwrap `result`
//! before reusing the openai parser.

use std::env;

use pi_core::{PiError, PiErrorKind, PiResult, StreamSink};

use crate::openai::{complete_chat_with_headers, stream_chat_with_headers};
use crate::{Provider, ProviderInfo, ProviderRequest, ProviderResponse};

#[derive(Debug, Default, Clone)]
pub struct CloudflareProvider;

impl CloudflareProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for CloudflareProvider {
    fn info(&self) -> ProviderInfo {
        cloudflare_info()
    }
    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let url = build_url(&request)?;
        let auth = build_auth()?;
        let auth_ref: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        complete_chat_with_headers(&self.info(), &url, &auth_ref, request)
    }
    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        let url = build_url(&request)?;
        let auth = build_auth()?;
        let auth_ref: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        stream_chat_with_headers(&self.info(), &url, &auth_ref, request, sink)
    }
}

fn build_url(request: &ProviderRequest) -> PiResult<String> {
    let account = env::var("CLOUDFLARE_ACCOUNT_ID").map_err(|_| {
        PiError::new(
            PiErrorKind::Provider,
            "缺少 CLOUDFLARE_ACCOUNT_ID（Workers AI 必备）",
        )
    })?;
    let base = env::var("CLOUDFLARE_BASE_URL")
        .unwrap_or_else(|_| "https://api.cloudflare.com/client/v4".to_string());
    Ok(format!(
        "{}/accounts/{}/ai/run/{}",
        base.trim_end_matches('/'),
        account,
        request.model.model,
    ))
}

fn build_auth() -> PiResult<Vec<(String, String)>> {
    let token = env::var("CLOUDFLARE_API_TOKEN")
        .map_err(|_| PiError::new(PiErrorKind::Provider, "缺少 CLOUDFLARE_API_TOKEN"))?;
    Ok(vec![(
        "authorization".to_string(),
        format!("Bearer {token}"),
    )])
}

pub fn cloudflare_info() -> ProviderInfo {
    ProviderInfo {
        id: "cloudflare".to_string(),
        display_name: "Cloudflare Workers AI".to_string(),
        default_model: "@cf/meta/llama-3.3-70b-instruct-fp8-fast".to_string(),
        supported_models: vec![
            "@cf/meta/llama-3.3-70b-instruct-fp8-fast".to_string(),
            "@cf/meta/llama-3.1-8b-instruct".to_string(),
            "@cf/qwen/qwen1.5-14b-chat-awq".to_string(),
            "@cf/mistral/mistral-7b-instruct-v0.1".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("CLOUDFLARE_API_TOKEN".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn info_has_cloudflare_id() {
        assert_eq!(cloudflare_info().id, "cloudflare");
        assert!(cloudflare_info().default_model.starts_with("@cf/"));
    }
}
