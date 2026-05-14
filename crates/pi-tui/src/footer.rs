//! Status footer data provider.
//!
//! Pulls git branch + dirty flag from `pi_agent::source_info`, formats a
//! token-usage progress bar relative to the configured context window, and
//! returns a single `FooterData` struct the renderer turns into spans.
//!
//! The renderer used to bake all this directly into one format string; the
//! struct lets us add fields (last error class, provider health) without
//! reflowing the call site, and makes the unit tests cleaner.

use pi_agent::source_info::SourceInfo;
use pi_core::Usage;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterData {
    pub status_line: String,
    pub provider_line: String,
    pub git_line: Option<String>,
    pub token_bar: TokenBar,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenBar {
    pub used: u32,
    pub window: u32,
    /// 0..=20, suitable for a 20-cell bar in the TUI.
    pub filled_cells: u8,
    pub bar_text: String,
}

impl TokenBar {
    pub fn new(used: u32, window: u32) -> Self {
        let cells = if window == 0 {
            0u8
        } else {
            let raw = (used as f32 / window as f32) * 20.0;
            raw.clamp(0.0, 20.0).round() as u8
        };
        let mut bar = String::with_capacity(22);
        bar.push('[');
        for i in 0..20 {
            if i < cells {
                bar.push('█');
            } else {
                bar.push('·');
            }
        }
        bar.push(']');
        Self {
            used,
            window,
            filled_cells: cells,
            bar_text: bar,
        }
    }
}

pub fn build(
    status: &str,
    provider: &str,
    model: &str,
    usage: &Usage,
    window_tokens: u32,
    source: &SourceInfo,
    last_error: Option<&str>,
) -> FooterData {
    let token_bar = TokenBar::new(usage.total_tokens, window_tokens);
    let provider_line = format!(
        "provider={provider} model={model} | tokens in={} out={} total={}",
        usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
    );
    let git_line = source.git.as_ref().map(|git| {
        let branch = git
            .branch
            .clone()
            .unwrap_or_else(|| "(detached)".to_string());
        let commit = git
            .commit
            .as_deref()
            .map(|c| &c[..c.len().min(8)])
            .unwrap_or("");
        let dirty = if git.dirty { " *" } else { "" };
        format!("git {branch}@{commit}{dirty}")
    });
    FooterData {
        status_line: status.to_string(),
        provider_line,
        git_line,
        token_bar,
        last_error: last_error.map(|s| s.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_agent::source_info::{GitInfo, ProjectManager, SourceInfo};

    #[test]
    fn token_bar_fills_proportionally() {
        let bar = TokenBar::new(50_000, 100_000);
        assert_eq!(bar.filled_cells, 10);
        assert_eq!(bar.bar_text.chars().filter(|c| *c == '█').count(), 10);
    }

    #[test]
    fn token_bar_handles_zero_window() {
        let bar = TokenBar::new(123, 0);
        assert_eq!(bar.filled_cells, 0);
    }

    #[test]
    fn token_bar_clamps_overflow() {
        let bar = TokenBar::new(999_999, 1_000);
        assert_eq!(bar.filled_cells, 20);
    }

    #[test]
    fn build_includes_git_line_when_source_has_git() {
        let source = SourceInfo {
            git: Some(GitInfo {
                branch: Some("main".into()),
                commit: Some("abcdef1234567890".into()),
                remote: None,
                dirty: true,
            }),
            project: vec![ProjectManager::Cargo],
        };
        let usage = Usage {
            prompt_tokens: 100,
            completion_tokens: 200,
            total_tokens: 300,
            ..Usage::default()
        };
        let data = build(
            "就绪",
            "anthropic",
            "claude-sonnet-4-6",
            &usage,
            128_000,
            &source,
            None,
        );
        assert!(data
            .git_line
            .as_deref()
            .unwrap()
            .contains("main@abcdef12 *"));
        assert!(data.provider_line.contains("provider=anthropic"));
    }
}
