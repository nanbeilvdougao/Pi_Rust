//! Parallel branch summarization.
//!
//! When a user fans out into multiple subagents (or when compaction encounters
//! a conversation with clear topical boundaries), we summarize each branch
//! independently and concurrently, then merge them in order. The linear
//! summarizer in [`compaction`](super::compaction) operates on a single slice
//! — this module is the multi-branch counterpart, matching the TS pi
//! `packages/agent/src/branch-summarizer.ts` shape.
//!
//! Design choices:
//!
//! - **No tokio.** Each branch runs on its own `std::thread::spawn` so we
//!   stay consistent with the workspace's sync-first stance. The thread
//!   count is capped at `max_concurrency` (default 4) to keep provider
//!   pressure under control.
//! - **Order-preserving merge.** Even though branches finish out of order,
//!   the final summary list reflects the user-supplied order so cross-branch
//!   references in the final message remain coherent.
//! - **Failure containment.** A failure in one branch yields an `Err`
//!   entry for that branch but does not poison the others. Callers decide
//!   whether to fall back to the un-summarized text.
//!
//! The provider trait requires `Send + Sync`, and we pass it as
//! `Arc<dyn Provider>` so every thread can clone the handle cheaply.

use std::sync::{mpsc, Arc};
use std::thread;

use pi_core::{Message, ModelSelection, PiError, PiErrorKind, PiResult, Role};
use pi_providers::{Provider, ProviderRequest};

/// One branch of conversation to summarize.
#[derive(Debug, Clone)]
pub struct Branch {
    pub id: String,
    pub messages: Vec<Message>,
}

/// Result of summarizing a single branch.
#[derive(Debug, Clone)]
pub struct BranchSummaryEntry {
    pub id: String,
    pub summary: PiResult<String>,
}

/// Configuration for the parallel summarizer.
#[derive(Debug, Clone)]
pub struct BranchSummarizerConfig {
    /// Max worker threads. 0 falls back to `1` so the API never deadlocks.
    pub max_concurrency: usize,
    /// Optional override for the global system prompt fed to each branch.
    pub system_prompt: Option<String>,
    /// Maximum tokens per summary. Defaults to 512 when None.
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature. Defaults to 0.2.
    pub temperature: Option<f32>,
}

impl Default for BranchSummarizerConfig {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            system_prompt: None,
            max_output_tokens: Some(512),
            temperature: Some(0.2),
        }
    }
}

pub struct BranchSummarizer {
    provider: Arc<dyn Provider>,
    model: ModelSelection,
    config: BranchSummarizerConfig,
}

impl BranchSummarizer {
    pub fn new(provider: Arc<dyn Provider>, model: ModelSelection) -> Self {
        Self {
            provider,
            model,
            config: BranchSummarizerConfig::default(),
        }
    }

    pub fn with_config(mut self, config: BranchSummarizerConfig) -> Self {
        self.config = config;
        self
    }

    /// Summarize each branch in parallel; return one entry per input in the
    /// original order.
    pub fn summarize(&self, branches: Vec<Branch>) -> Vec<BranchSummaryEntry> {
        if branches.is_empty() {
            return Vec::new();
        }
        let concurrency = self.config.max_concurrency.max(1).min(branches.len());
        let total = branches.len();
        let (tx, rx) = mpsc::channel::<(usize, BranchSummaryEntry)>();
        let provider = Arc::clone(&self.provider);
        let model = self.model.clone();
        let system_prompt = self.config.system_prompt.clone();
        let max_output_tokens = self.config.max_output_tokens;
        let temperature = self.config.temperature;

        // Simple bounded worker pool: chunk the input across concurrency threads.
        let mut chunks: Vec<Vec<(usize, Branch)>> = (0..concurrency).map(|_| Vec::new()).collect();
        for (idx, branch) in branches.into_iter().enumerate() {
            chunks[idx % concurrency].push((idx, branch));
        }

        let mut handles = Vec::with_capacity(concurrency);
        for chunk in chunks {
            if chunk.is_empty() {
                continue;
            }
            let tx = tx.clone();
            let provider = Arc::clone(&provider);
            let model = model.clone();
            let system_prompt = system_prompt.clone();
            let handle = thread::spawn(move || {
                for (idx, branch) in chunk {
                    let result = summarize_branch(
                        &branch,
                        provider.as_ref(),
                        &model,
                        system_prompt.as_deref(),
                        max_output_tokens,
                        temperature,
                    );
                    let _ = tx.send((
                        idx,
                        BranchSummaryEntry {
                            id: branch.id,
                            summary: result,
                        },
                    ));
                }
            });
            handles.push(handle);
        }
        drop(tx);

        let mut buffer: Vec<Option<BranchSummaryEntry>> = (0..total).map(|_| None).collect();
        for (idx, entry) in rx {
            if idx < buffer.len() {
                buffer[idx] = Some(entry);
            }
        }
        for handle in handles {
            let _ = handle.join();
        }
        buffer
            .into_iter()
            .map(|entry| {
                entry.unwrap_or_else(|| BranchSummaryEntry {
                    id: String::new(),
                    summary: Err(PiError::new(
                        PiErrorKind::Provider,
                        "branch summarizer dropped result",
                    )),
                })
            })
            .collect()
    }
}

fn summarize_branch(
    branch: &Branch,
    provider: &dyn Provider,
    model: &ModelSelection,
    parent_system: Option<&str>,
    max_output_tokens: Option<u32>,
    temperature: Option<f32>,
) -> PiResult<String> {
    if branch.messages.is_empty() {
        return Ok(String::new());
    }
    let mut system = String::from(
        "你是 Pi 的分支摘要器。请把下面这段对话压缩成不超过 200 字的简体中文摘要，\
        保留关键事实、决策、未完成任务，但不要把分支 id 写进正文。",
    );
    if let Some(parent) = parent_system {
        system.push_str("\n父系统提示：\n");
        system.push_str(parent);
    }

    let mut transcript = String::new();
    transcript.push_str("分支 id: ");
    transcript.push_str(&branch.id);
    transcript.push_str("\n\n");
    for message in &branch.messages {
        transcript.push('[');
        transcript.push_str(message.role.as_str());
        transcript.push_str("] ");
        transcript.push_str(&message.content);
        transcript.push_str("\n\n");
    }

    let request = ProviderRequest {
        model: model.clone(),
        messages: vec![Message::new(
            Role::User,
            format!("请摘要以下分支：\n\n{transcript}"),
        )],
        tools: Vec::new(),
        system_prompt: Some(system),
        max_output_tokens,
        temperature,
        stream: false,
    };
    let response = provider.complete(request)?;
    Ok(response.message.content)
}

/// Collapse a sequence of branch summaries into a single system message
/// suitable for pre-pending to a fresh conversation.
pub fn merge_summaries(entries: &[BranchSummaryEntry]) -> String {
    let mut out = String::new();
    out.push_str("[branch-summary]\n");
    for entry in entries {
        out.push_str("- ");
        out.push_str(&entry.id);
        out.push_str(": ");
        match &entry.summary {
            Ok(text) => out.push_str(text.trim()),
            Err(err) => {
                out.push_str("(摘要失败: ");
                out.push_str(&err.to_string());
                out.push(')');
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::Usage;
    use pi_providers::{ProviderInfo, ProviderResponse};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingProvider {
        counter: AtomicUsize,
    }

    impl Provider for CountingProvider {
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                id: "counting".into(),
                display_name: "Counting".into(),
                default_model: "stub".into(),
                supported_models: vec!["stub".into()],
                local_first: true,
                requires_api_key_env: None,
            }
        }
        fn complete(&self, request: ProviderRequest) -> PiResult<ProviderResponse> {
            let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
            let prompt = request
                .messages
                .first()
                .map(|m| m.content.clone())
                .unwrap_or_default();
            let trimmed: String = prompt.chars().take(20).collect();
            Ok(ProviderResponse {
                message: Message::new(Role::Assistant, format!("summary#{n}({trimmed}…)")),
                events: Vec::new(),
                stream_events: Vec::new(),
                tool_calls: Vec::new(),
                usage: Usage::default(),
            })
        }
    }

    fn model() -> ModelSelection {
        ModelSelection {
            provider: "counting".into(),
            model: "stub".into(),
        }
    }

    fn provider() -> Arc<dyn Provider> {
        Arc::new(CountingProvider {
            counter: AtomicUsize::new(0),
        })
    }

    #[test]
    fn empty_input_returns_empty() {
        let summarizer = BranchSummarizer::new(provider(), model());
        let out = summarizer.summarize(Vec::new());
        assert!(out.is_empty());
    }

    #[test]
    fn preserves_input_order_under_parallel_execution() {
        let summarizer = BranchSummarizer::new(provider(), model()).with_config(
            BranchSummarizerConfig {
                max_concurrency: 4,
                ..BranchSummarizerConfig::default()
            },
        );
        let branches: Vec<Branch> = (0..8)
            .map(|i| Branch {
                id: format!("b{i}"),
                messages: vec![Message::new(Role::User, format!("topic {i}"))],
            })
            .collect();
        let out = summarizer.summarize(branches.clone());
        assert_eq!(out.len(), branches.len());
        for (i, entry) in out.iter().enumerate() {
            assert_eq!(entry.id, format!("b{i}"));
            assert!(entry.summary.is_ok());
        }
    }

    #[test]
    fn merge_summaries_renders_one_line_per_branch() {
        let entries = vec![
            BranchSummaryEntry {
                id: "a".into(),
                summary: Ok("first".into()),
            },
            BranchSummaryEntry {
                id: "b".into(),
                summary: Ok("second".into()),
            },
        ];
        let merged = merge_summaries(&entries);
        assert!(merged.contains("[branch-summary]"));
        assert!(merged.contains("- a: first"));
        assert!(merged.contains("- b: second"));
    }

    #[test]
    fn merge_summaries_surfaces_errors() {
        let entries = vec![BranchSummaryEntry {
            id: "z".into(),
            summary: Err(PiError::new(PiErrorKind::Provider, "503")),
        }];
        let merged = merge_summaries(&entries);
        assert!(merged.contains("摘要失败"));
        assert!(merged.contains("503"));
    }

    #[test]
    fn concurrency_zero_is_treated_as_one() {
        let summarizer = BranchSummarizer::new(provider(), model()).with_config(
            BranchSummarizerConfig {
                max_concurrency: 0,
                ..BranchSummarizerConfig::default()
            },
        );
        let out = summarizer.summarize(vec![Branch {
            id: "only".into(),
            messages: vec![Message::new(Role::User, "x")],
        }]);
        assert_eq!(out.len(), 1);
    }
}
