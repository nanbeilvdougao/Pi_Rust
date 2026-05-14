//! GitHub Copilot provider.
//!
//! Copilot's chat endpoint is openai-compat in body shape but requires a
//! GitHub-issued bearer (`GITHUB_COPILOT_TOKEN`) plus a set of editor
//! identification headers Microsoft uses to gate access:
//!
//! - `Editor-Version: pi-rust/<version>`
//! - `Editor-Plugin-Version: pi-rust/<version>`
//! - `Copilot-Integration-Id: vscode-chat`
//!
//! Endpoint defaults to `https://api.githubcopilot.com/chat/completions`;
//! override with `COPILOT_BASE_URL`.

use std::env;

use pi_core::{PiError, PiErrorKind, PiResult, StreamSink};

use crate::openai::{complete_chat_with_headers, stream_chat_with_headers};
use crate::{Provider, ProviderInfo, ProviderRequest, ProviderResponse};

#[derive(Debug, Default, Clone)]
pub struct CopilotProvider;

impl CopilotProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Provider for CopilotProvider {
    fn info(&self) -> ProviderInfo {
        copilot_info()
    }
    fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
        let url = endpoint();
        let auth = build_headers()?;
        let auth_ref: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        complete_chat_with_headers(&self.info(), &url, &auth_ref, request)
    }
    fn stream(
        &self,
        request: ProviderRequest,
        sink: &mut dyn StreamSink,
    ) -> PiResult<ProviderResponse> {
        let url = endpoint();
        let auth = build_headers()?;
        let auth_ref: Vec<(&str, &str)> =
            auth.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        stream_chat_with_headers(&self.info(), &url, &auth_ref, request, sink)
    }
}

fn endpoint() -> String {
    env::var("COPILOT_BASE_URL")
        .unwrap_or_else(|_| "https://api.githubcopilot.com/chat/completions".to_string())
}

fn build_headers() -> PiResult<Vec<(String, String)>> {
    let token = env::var("GITHUB_COPILOT_TOKEN").map_err(|_| {
        PiError::new(
            PiErrorKind::Provider,
            "缺少 GITHUB_COPILOT_TOKEN（用 `gh auth token` 或 Copilot CLI 拿到）",
        )
    })?;
    Ok(vec![
        ("authorization".to_string(), format!("Bearer {token}")),
        (
            "editor-version".to_string(),
            concat!("pi-rust/", env!("CARGO_PKG_VERSION")).to_string(),
        ),
        (
            "editor-plugin-version".to_string(),
            concat!("pi-rust/", env!("CARGO_PKG_VERSION")).to_string(),
        ),
        (
            "copilot-integration-id".to_string(),
            "vscode-chat".to_string(),
        ),
    ])
}

pub fn copilot_info() -> ProviderInfo {
    ProviderInfo {
        id: "copilot".to_string(),
        display_name: "GitHub Copilot".to_string(),
        default_model: "gpt-4o".to_string(),
        supported_models: vec![
            "gpt-4o".to_string(),
            "gpt-4.1".to_string(),
            "claude-3.5-sonnet".to_string(),
            "gemini-2.0-flash-001".to_string(),
        ],
        local_first: false,
        requires_api_key_env: Some("GITHUB_COPILOT_TOKEN".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn info_has_copilot_id() {
        assert_eq!(copilot_info().id, "copilot");
    }
}
