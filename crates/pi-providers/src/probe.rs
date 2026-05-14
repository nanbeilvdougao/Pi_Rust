//! Provider liveness probes for `pi doctor --probe`.
//!
//! Each probe sends the cheapest credentialed endpoint we can reach without
//! incurring tokens. We deliberately avoid running an actual completion —
//! the goal is "are the URL + key + network reachable", not "does the model
//! work end-to-end". Probes are read-only.
//!
//! Probe outcomes:
//! - `Ok` — endpoint reachable and credentials accepted.
//! - `AuthFailed` — endpoint reachable, credentials rejected (401/403).
//! - `Unreachable` — DNS/transport failure or timeout.
//! - `MissingCredential` — no key found in env or pi-auth store.
//! - `Unsupported` — provider does not have a probe endpoint yet.

use std::env;
use std::time::Duration;

use crate::{http_agent, ProviderInfo, ProviderRegistry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    Ok,
    AuthFailed(String),
    Unreachable(String),
    MissingCredential,
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct ProbeReport {
    pub provider: String,
    pub outcome: ProbeOutcome,
}

pub fn probe_all() -> Vec<ProbeReport> {
    let registry = ProviderRegistry::builtin();
    let mut reports = Vec::new();
    for provider in registry.list() {
        let outcome = probe_one(provider);
        reports.push(ProbeReport {
            provider: provider.id.clone(),
            outcome,
        });
    }
    reports
}

fn probe_one(info: &ProviderInfo) -> ProbeOutcome {
    match info.id.as_str() {
        "echo" => ProbeOutcome::Ok,
        "ollama" => probe_ollama(),
        "openai" => probe_openai_compat("OPENAI_API_KEY", "https://api.openai.com/v1/models"),
        "moonshot" => probe_openai_compat("MOONSHOT_API_KEY", "https://api.moonshot.cn/v1/models"),
        "deepseek" => probe_openai_compat("DEEPSEEK_API_KEY", "https://api.deepseek.com/models"),
        "qwen" => probe_openai_compat(
            "DASHSCOPE_API_KEY",
            "https://dashscope.aliyuncs.com/compatible-mode/v1/models",
        ),
        "zhipu" => probe_openai_compat(
            "ZHIPU_API_KEY",
            "https://open.bigmodel.cn/api/paas/v4/models",
        ),
        "anthropic" => probe_anthropic(),
        "gemini" => probe_gemini(),
        // No documented free probe surface for MiniMax yet.
        "minimax" => ProbeOutcome::Unsupported,
        _ => ProbeOutcome::Unsupported,
    }
}

fn probe_ollama() -> ProbeOutcome {
    let url = env::var("OLLAMA_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
    let agent = http_agent_with_timeout(Duration::from_secs(3));
    let url = format!("{}/api/tags", url.trim_end_matches('/'));
    match agent.get(&url).call() {
        Ok(_) => ProbeOutcome::Ok,
        Err(ureq::Error::Status(status, _)) if status == 401 || status == 403 => {
            ProbeOutcome::AuthFailed(format!("HTTP {status}"))
        }
        Err(ureq::Error::Status(status, _)) => ProbeOutcome::Unreachable(format!("HTTP {status}")),
        Err(ureq::Error::Transport(err)) => ProbeOutcome::Unreachable(err.to_string()),
    }
}

fn probe_openai_compat(env_name: &str, url: &str) -> ProbeOutcome {
    let key = match credential(env_name) {
        Some(value) => value,
        None => return ProbeOutcome::MissingCredential,
    };
    let agent = http_agent_with_timeout(Duration::from_secs(5));
    let auth = format!("Bearer {key}");
    match agent.get(url).set("authorization", &auth).call() {
        Ok(_) => ProbeOutcome::Ok,
        Err(ureq::Error::Status(401 | 403, _)) => ProbeOutcome::AuthFailed("401/403".to_string()),
        Err(ureq::Error::Status(status, _)) => ProbeOutcome::Unreachable(format!("HTTP {status}")),
        Err(ureq::Error::Transport(err)) => ProbeOutcome::Unreachable(err.to_string()),
    }
}

fn probe_anthropic() -> ProbeOutcome {
    let key = match credential("ANTHROPIC_API_KEY") {
        Some(value) => value,
        None => return ProbeOutcome::MissingCredential,
    };
    let base =
        env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".to_string());
    let url = format!("{}/v1/models", base.trim_end_matches('/'));
    let agent = http_agent_with_timeout(Duration::from_secs(5));
    match agent
        .get(&url)
        .set("x-api-key", &key)
        .set("anthropic-version", "2023-06-01")
        .call()
    {
        Ok(_) => ProbeOutcome::Ok,
        Err(ureq::Error::Status(401 | 403, _)) => ProbeOutcome::AuthFailed("401/403".to_string()),
        Err(ureq::Error::Status(status, _)) => ProbeOutcome::Unreachable(format!("HTTP {status}")),
        Err(ureq::Error::Transport(err)) => ProbeOutcome::Unreachable(err.to_string()),
    }
}

fn probe_gemini() -> ProbeOutcome {
    let key = match credential("GEMINI_API_KEY").or_else(|| credential("GOOGLE_API_KEY")) {
        Some(value) => value,
        None => return ProbeOutcome::MissingCredential,
    };
    let base = env::var("GEMINI_BASE_URL")
        .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string());
    let url = format!("{}/v1beta/models?key={key}", base.trim_end_matches('/'));
    let agent = http_agent_with_timeout(Duration::from_secs(5));
    match agent.get(&url).call() {
        Ok(_) => ProbeOutcome::Ok,
        Err(ureq::Error::Status(401 | 403, _)) => ProbeOutcome::AuthFailed("401/403".to_string()),
        Err(ureq::Error::Status(status, _)) => ProbeOutcome::Unreachable(format!("HTTP {status}")),
        Err(ureq::Error::Transport(err)) => ProbeOutcome::Unreachable(err.to_string()),
    }
}

fn credential(env_name: &str) -> Option<String> {
    if let Ok(value) = env::var(env_name) {
        if !value.is_empty() {
            return Some(value);
        }
    }
    use pi_auth::Resolver as _;
    pi_auth::layered_resolver()
        .ok()
        .and_then(|r| r.lookup("", env_name).ok().flatten())
}

fn http_agent_with_timeout(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout_read(timeout)
        .timeout_write(timeout)
        .user_agent(concat!("pi-rust/", env!("CARGO_PKG_VERSION"), " probe"))
        .build()
}

#[allow(dead_code)]
fn fallback_use(_: &ureq::Agent) {
    let _ = http_agent();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Provider;

    #[test]
    fn echo_probe_is_ok() {
        let outcome = probe_one(&crate::EchoProvider.info());
        assert_eq!(outcome, ProbeOutcome::Ok);
    }

    #[test]
    fn echo_via_registry_is_supported() {
        let reports = probe_all();
        let echo = reports.iter().find(|r| r.provider == "echo").unwrap();
        assert_eq!(echo.outcome, ProbeOutcome::Ok);
    }

    #[test]
    fn minimax_probe_is_unsupported_by_design() {
        let outcome = probe_one(&crate::openai::minimax_info());
        assert_eq!(outcome, ProbeOutcome::Unsupported);
    }
}
