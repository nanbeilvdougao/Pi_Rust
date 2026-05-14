//! Model alias resolution.
//!
//! TS pi accepts short names like `sonnet`, `opus`, `gpt4`, `qwen` and maps
//! them to canonical provider + model pairs. This module mirrors that surface:
//!
//! - `resolve_alias("sonnet")` → `("anthropic", "claude-sonnet-4-6")`.
//! - `resolve_alias("anthropic/claude-opus-4-7")` → the explicit pair.
//! - `resolve_alias("deepseek-chat")` → finds the registry entry whose
//!   `supported_models` contains it.
//!
//! Resolution order:
//! 1. Exact match against the curated alias table (high signal).
//! 2. `provider/model` literal.
//! 3. Provider id, returning its `default_model`.
//! 4. Model id, picking the first registry entry that lists it.
//! 5. Fuzzy match: case-insensitive substring against the alias table only
//!    (we do not fuzzy-match model ids — too easy to land on the wrong vendor).
//!
//! The function is pure and has no side effects so the CLI / TUI / SDK all
//! share the same behavior.

use pi_core::{ModelSelection, PiError, PiErrorKind, PiResult};

use crate::{ProviderInfo, ProviderRegistry};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSelection {
    pub model: ModelSelection,
    /// True when the input did not literally name `provider/model` — useful
    /// for the TUI to surface "resolved sonnet → claude-sonnet-4-6".
    pub via_alias: bool,
    /// The alias key that matched, if any.
    pub alias: Option<&'static str>,
}

/// Curated alias table. Keep entries lowercase; the resolver lowercases input
/// before lookup.
const ALIASES: &[(&str, &str, &str)] = &[
    // Anthropic
    ("sonnet", "anthropic", "claude-sonnet-4-6"),
    ("claude-sonnet", "anthropic", "claude-sonnet-4-6"),
    ("opus", "anthropic", "claude-opus-4-7"),
    ("claude-opus", "anthropic", "claude-opus-4-7"),
    ("haiku", "anthropic", "claude-haiku-4-5-20251001"),
    ("claude-haiku", "anthropic", "claude-haiku-4-5-20251001"),
    ("claude", "anthropic", "claude-sonnet-4-6"),
    // OpenAI
    ("gpt4", "openai", "gpt-4.1"),
    ("gpt-4", "openai", "gpt-4.1"),
    ("gpt4o", "openai", "gpt-4o"),
    ("4o", "openai", "gpt-4o"),
    ("4omini", "openai", "gpt-4o-mini"),
    ("gpt", "openai", "gpt-4o-mini"),
    ("o4mini", "openai", "o4-mini"),
    // DeepSeek
    ("deepseek", "deepseek", "deepseek-chat"),
    ("deepseek-r1", "deepseek", "deepseek-reasoner"),
    ("r1", "deepseek", "deepseek-reasoner"),
    ("coder", "deepseek", "deepseek-coder"),
    // Moonshot
    ("moonshot", "moonshot", "moonshot-v1-8k"),
    ("kimi", "moonshot", "kimi-k2-0905-preview"),
    ("k2", "moonshot", "kimi-k2-0905-preview"),
    // Qwen
    ("qwen", "qwen", "qwen-plus"),
    ("qwen-max", "qwen", "qwen-max"),
    ("qwen3", "qwen", "qwen3-coder"),
    ("qwen-coder", "qwen", "qwen2.5-coder-32b-instruct"),
    // Zhipu GLM
    ("glm", "zhipu", "glm-4-plus"),
    ("glm4", "zhipu", "glm-4-plus"),
    ("codegeex", "zhipu", "codegeex-4"),
    // MiniMax
    ("minimax", "minimax", "abab6.5s-chat"),
    ("m1", "minimax", "MiniMax-M1"),
    // Gemini
    ("gemini", "gemini", "gemini-2.5-flash"),
    ("flash", "gemini", "gemini-2.5-flash"),
    ("gemini-pro", "gemini", "gemini-2.5-pro"),
    // Ollama
    ("ollama", "ollama", "qwen2.5:7b"),
    ("local", "ollama", "qwen2.5:7b"),
    // Azure / Vertex / Copilot / OpenRouter / Mistral / Cloudflare
    ("azure", "azure", "gpt-4o"),
    ("azure-gpt4", "azure", "gpt-4.1"),
    ("vertex", "vertex", "gemini-2.5-flash"),
    ("vertex-pro", "vertex", "gemini-2.5-pro"),
    ("copilot", "copilot", "gpt-4o"),
    ("openrouter", "openrouter", "anthropic/claude-3.5-sonnet"),
    ("mistral", "mistral", "mistral-large-latest"),
    ("codestral", "mistral", "codestral-latest"),
    (
        "cloudflare",
        "cloudflare",
        "@cf/meta/llama-3.3-70b-instruct-fp8-fast",
    ),
    (
        "cf-llama",
        "cloudflare",
        "@cf/meta/llama-3.3-70b-instruct-fp8-fast",
    ),
    ("responses", "openai-responses", "gpt-4.1"),
    ("o3", "openai-responses", "o3"),
];

pub fn resolve_alias(input: &str) -> PiResult<ResolvedSelection> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(PiError::new(PiErrorKind::InvalidInput, "模型别名不能为空"));
    }
    let lower = trimmed.to_ascii_lowercase();

    // 1. Exact alias.
    if let Some((alias, provider, model)) = ALIASES.iter().find(|(a, _, _)| *a == lower.as_str()) {
        return Ok(ResolvedSelection {
            model: ModelSelection {
                provider: (*provider).to_string(),
                model: (*model).to_string(),
            },
            via_alias: true,
            alias: Some(*alias),
        });
    }

    // 2. provider/model literal.
    if let Some((provider, model)) = trimmed.split_once('/') {
        return Ok(ResolvedSelection {
            model: ModelSelection {
                provider: provider.to_string(),
                model: model.to_string(),
            },
            via_alias: false,
            alias: None,
        });
    }

    // 3 / 4. Match registry entries.
    let registry = ProviderRegistry::builtin();
    if let Some(provider) = registry.get(trimmed) {
        return Ok(ResolvedSelection {
            model: ModelSelection {
                provider: provider.id.clone(),
                model: provider.default_model.clone(),
            },
            via_alias: true,
            alias: None,
        });
    }
    // Prefer the canonical first-party providers when multiple expose the
    // same model id (e.g. `gpt-4o` lives in both `openai` and `azure`).
    let priority = [
        "openai",
        "anthropic",
        "gemini",
        "moonshot",
        "deepseek",
        "qwen",
        "zhipu",
        "minimax",
        "ollama",
        "mistral",
        "openrouter",
        "openai-responses",
        "azure",
        "bedrock",
        "vertex",
        "cloudflare",
        "copilot",
    ];
    let mut ordered: Vec<&ProviderInfo> = registry.list().collect();
    ordered.sort_by_key(|p| {
        priority
            .iter()
            .position(|id| *id == p.id.as_str())
            .unwrap_or(usize::MAX)
    });
    for provider in ordered {
        if provider
            .supported_models
            .iter()
            .any(|model| model == trimmed)
        {
            return Ok(ResolvedSelection {
                model: ModelSelection {
                    provider: provider.id.clone(),
                    model: trimmed.to_string(),
                },
                via_alias: true,
                alias: None,
            });
        }
    }

    // 5. Fuzzy fallback against the alias table.
    if let Some((alias, provider, model)) = ALIASES
        .iter()
        .find(|(a, _, _)| a.contains(lower.as_str()) || lower.contains(*a))
    {
        return Ok(ResolvedSelection {
            model: ModelSelection {
                provider: (*provider).to_string(),
                model: (*model).to_string(),
            },
            via_alias: true,
            alias: Some(*alias),
        });
    }

    Err(PiError::new(
        PiErrorKind::Provider,
        format!(
            "无法解析模型别名 `{input}`。运行 `pi --list-models` 查看可用模型，或 `pi --list-aliases` 查看快捷方式。"
        ),
    ))
}

/// Return every alias as `(alias, provider, model)` so the CLI's
/// `--list-aliases` can render them.
pub fn aliases_table() -> &'static [(&'static str, &'static str, &'static str)] {
    ALIASES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_alias_resolves_to_canonical_pair() {
        let r = resolve_alias("sonnet").expect("resolve");
        assert_eq!(r.model.provider, "anthropic");
        assert_eq!(r.model.model, "claude-sonnet-4-6");
        assert!(r.via_alias);
        assert_eq!(r.alias, Some("sonnet"));
    }

    #[test]
    fn case_insensitive() {
        let r = resolve_alias("Opus").expect("resolve");
        assert_eq!(r.model.model, "claude-opus-4-7");
    }

    #[test]
    fn explicit_provider_slash_model() {
        let r = resolve_alias("anthropic/claude-opus-4-7").expect("resolve");
        assert_eq!(r.model.provider, "anthropic");
        assert_eq!(r.model.model, "claude-opus-4-7");
        assert!(!r.via_alias);
    }

    #[test]
    fn provider_id_yields_default_model() {
        let r = resolve_alias("anthropic").expect("resolve");
        assert_eq!(r.model.provider, "anthropic");
        assert_eq!(r.model.model, "claude-sonnet-4-6");
    }

    #[test]
    fn model_id_finds_owning_provider() {
        let r = resolve_alias("gpt-4o").expect("resolve");
        assert_eq!(r.model.provider, "openai");
        assert_eq!(r.model.model, "gpt-4o");
    }

    #[test]
    fn unknown_alias_returns_actionable_error() {
        let err = resolve_alias("totally-fake").unwrap_err();
        assert!(err.message.contains("list-models"));
    }

    #[test]
    fn empty_input_is_rejected() {
        assert!(resolve_alias("").is_err());
        assert!(resolve_alias("   ").is_err());
    }
}
