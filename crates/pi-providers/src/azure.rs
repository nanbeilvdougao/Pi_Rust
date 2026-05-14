//! Azure OpenAI provider.
//!
//! Endpoint shape:
//! `{AZURE_OPENAI_ENDPOINT}/openai/deployments/{deployment}/chat/completions?api-version={ver}`
//!
//! Auth: `api-key: <key>` by default. Set `AZURE_OPENAI_USE_AAD=1` and
//! provide `AZURE_OPENAI_AAD_TOKEN` (e.g. from
//! `az account get-access-token`) to use `Authorization: Bearer …` instead.
//!
//! - `AZURE_OPENAI_DEPLOYMENT` overrides selection.model.
//! - `AZURE_OPENAI_API_VERSION` defaults to `2024-10-21`.

use std::env;

use pi_core::{PiError, PiErrorKind, PiResult, StreamSink};

use crate::openai::{complete_chat_with_headers, stream_chat_with_headers};
use crate::{Provider, ProviderInfo, ProviderRequest, ProviderResponse};

#[derive(Debug, Default, Clone)]
pub struct AzureOpenAiProvider;

impl AzureOpenAiProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for AzureOpenAiProvider {
    fn info(&self) -> ProviderInfo {
        azure_openai_info()
    }

    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let url = build_url(&request)?;
        let auth = build_auth_header()?;
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
        let auth = build_auth_header()?;
        let auth_ref: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        stream_chat_with_headers(&self.info(), &url, &auth_ref, request, sink)
    }
}

fn build_url(request: &ProviderRequest) -> PiResult<String> {
    let base = env::var("AZURE_OPENAI_ENDPOINT").map_err(|_| {
        PiError::new(
            PiErrorKind::Provider,
            "缺少 AZURE_OPENAI_ENDPOINT（Azure 需要租户 endpoint）",
        )
    })?;
    let deployment =
        env::var("AZURE_OPENAI_DEPLOYMENT").unwrap_or_else(|_| request.model.model.clone());
    let api_version =
        env::var("AZURE_OPENAI_API_VERSION").unwrap_or_else(|_| "2024-10-21".to_string());
    Ok(format!(
        "{}/openai/deployments/{}/chat/completions?api-version={}",
        base.trim_end_matches('/'),
        deployment,
        api_version
    ))
}

fn build_auth_header() -> PiResult<Vec<(String, String)>> {
    let mut headers = Vec::new();
    if env::var("AZURE_OPENAI_USE_AAD")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        let bearer = env::var("AZURE_OPENAI_AAD_TOKEN").map_err(|_| {
            PiError::new(
                PiErrorKind::Provider,
                "AZURE_OPENAI_USE_AAD=1 时需要 AZURE_OPENAI_AAD_TOKEN",
            )
        })?;
        headers.push(("authorization".to_string(), format!("Bearer {bearer}")));
    } else {
        let key = env::var("AZURE_OPENAI_API_KEY").map_err(|_| {
            PiError::new(
                PiErrorKind::Provider,
                "缺少 AZURE_OPENAI_API_KEY（或 AZURE_OPENAI_USE_AAD=1 走 AAD）",
            )
        })?;
        headers.push(("api-key".to_string(), key));
    }
    Ok(headers)
}

pub fn azure_openai_info() -> ProviderInfo {
    ProviderInfo {
        id: "azure".to_string(),
        display_name: "Azure OpenAI".to_string(),
        default_model: "gpt-4o".to_string(),
        supported_models: vec![
            "gpt-4o".to_string(),
            "gpt-4o-mini".to_string(),
            "gpt-4.1".to_string(),
            "o4-mini".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("AZURE_OPENAI_API_KEY".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_has_azure_id() {
        assert_eq!(azure_openai_info().id, "azure");
    }
}
