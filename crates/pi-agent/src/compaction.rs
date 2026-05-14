//! Context compaction.
//!
//! Goal: keep the message window below `context_window_tokens * compaction_threshold`
//! by replacing older messages with a summary produced by the same provider.
//!
//! Algorithm:
//! 1. Estimate current request tokens. If under threshold, return None.
//! 2. Pick a `head` keep-set (system + first user) and a `tail` keep-set (last
//!    `keep_tail_pairs` user/assistant pairs). Everything between becomes the
//!    summary input.
//! 3. Ask the provider for a single summary message. We deliberately do not
//!    expose tools to the summarizer so it cannot wander.
//! 4. Replace the middle slice with a synthetic `system` message that carries
//!    the summary.
//!
//! We never compact tool messages on their own — they always travel with their
//! triggering assistant turn. This avoids dangling `tool_use_id` references.

use pi_core::{estimate_messages_tokens, AppConfig, Message, ModelSelection, PiResult, Role};
use pi_providers::{Provider, ProviderRequest};

/// Number of user/assistant pairs we always keep at the tail of the conversation
/// before falling back to compaction.
const KEEP_TAIL_PAIRS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionReport {
    pub before: u32,
    pub after: u32,
}

pub fn maybe_compact(
    messages: &mut Vec<Message>,
    provider: &dyn Provider,
    config: &AppConfig,
    system_prompt: Option<&str>,
) -> PiResult<Option<CompactionReport>> {
    let before = estimate_messages_tokens(messages);
    let threshold = (config.context_window_tokens as f32 * config.compaction_threshold) as u32;
    if before <= threshold {
        return Ok(None);
    }

    // Find the boundary indices.
    let total = messages.len();
    if total < KEEP_TAIL_PAIRS * 2 + 2 {
        return Ok(None);
    }
    let head_end = first_non_system_index(messages).min(total);
    let tail_start = trim_to_last_pairs(messages, KEEP_TAIL_PAIRS);
    if head_end >= tail_start {
        return Ok(None);
    }

    let middle: Vec<Message> = messages[head_end..tail_start].to_vec();
    if middle.is_empty() {
        return Ok(None);
    }
    let summary = summarize(&middle, provider, &config.model, system_prompt)?;
    let mut replacement = Message::new(Role::System, format!("[summary] {summary}"));
    replacement.timestamp_ms = pi_core::now_ms();

    messages.splice(head_end..tail_start, std::iter::once(replacement));
    let after = estimate_messages_tokens(messages);
    Ok(Some(CompactionReport { before, after }))
}

fn first_non_system_index(messages: &[Message]) -> usize {
    let mut idx = 0;
    while idx < messages.len() && matches!(messages[idx].role, Role::System) {
        idx += 1;
    }
    // Also keep the first user message so the assistant retains the initial intent.
    if idx < messages.len() && matches!(messages[idx].role, Role::User) {
        idx + 1
    } else {
        idx
    }
}

fn trim_to_last_pairs(messages: &[Message], pairs: usize) -> usize {
    if messages.is_empty() {
        return 0;
    }
    let mut idx = messages.len();
    let mut user_seen = 0usize;
    while idx > 0 {
        idx -= 1;
        if matches!(messages[idx].role, Role::User) {
            user_seen += 1;
            if user_seen > pairs {
                return idx + 1;
            }
        }
    }
    0
}

fn summarize(
    middle: &[Message],
    provider: &dyn Provider,
    model: &ModelSelection,
    parent_system: Option<&str>,
) -> PiResult<String> {
    let mut system = String::from(
        "你是 Pi 的压缩器。请把下面这段对话压缩成不超过 300 字的简体中文摘要，\
        保留关键事实、决策、未完成任务和重要的工具调用结果。不要列出系统消息。",
    );
    if let Some(parent) = parent_system {
        system.push_str("\n父系统提示：\n");
        system.push_str(parent);
    }

    let mut transcript = String::new();
    for message in middle {
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
            format!("请压缩以下对话：\n\n{transcript}"),
        )],
        tools: Vec::new(),
        system_prompt: Some(system),
        max_output_tokens: Some(1024),
        temperature: Some(0.2),
        stream: false,
    };

    let response = provider.complete(request)?;
    Ok(response.message.content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{AppConfig, Message, ModelSelection, PermissionModeKind, Role};
    use pi_providers::{Provider, ProviderInfo, ProviderResponse};

    struct Stub;

    impl Stub {
        fn new() -> Self {
            Self
        }
    }

    impl Provider for Stub {
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                id: "stub".into(),
                display_name: "stub".into(),
                default_model: "x".into(),
                supported_models: vec!["x".into()],
                local_first: true,
                requires_api_key_env: None,
            }
        }
        fn complete(&self, _request: ProviderRequest) -> PiResult<ProviderResponse> {
            let mut response = ProviderResponse {
                message: Message::new(Role::Assistant, "摘要：xxx"),
                events: Vec::new(),
                stream_events: Vec::new(),
                tool_calls: Vec::new(),
                usage: pi_core::Usage::default(),
            };
            response.events.push("摘要：xxx".to_string());
            Ok(response)
        }
    }

    #[test]
    fn no_op_under_threshold() {
        let provider = Stub::new();
        let config = AppConfig {
            context_window_tokens: 8000,
            compaction_threshold: 0.9,
            permission_mode: PermissionModeKind::TrustedWorkspace,
            ..AppConfig::default()
        };
        let mut messages = vec![
            Message::new(Role::User, "hi"),
            Message::new(Role::Assistant, "ok"),
        ];
        let report = maybe_compact(&mut messages, &provider, &config, None).expect("compact");
        assert!(report.is_none());
    }

    #[test]
    fn replaces_middle_when_over_threshold() {
        let provider = Stub::new();
        let model = ModelSelection {
            provider: "stub".into(),
            model: "x".into(),
        };
        let config = AppConfig {
            model: model.clone(),
            context_window_tokens: 8,
            compaction_threshold: 0.5,
            ..AppConfig::default()
        };
        let mut messages = Vec::new();
        messages.push(Message::new(Role::System, "sys"));
        messages.push(Message::new(Role::User, "first goal"));
        for _ in 0..20 {
            messages.push(Message::new(Role::Assistant, "thoughts thoughts thoughts"));
            messages.push(Message::new(Role::User, "follow-up follow-up"));
        }
        let before = estimate_messages_tokens(&messages);
        let report = maybe_compact(&mut messages, &provider, &config, None)
            .expect("compact")
            .expect("report");
        assert!(report.before == before);
        assert!(messages
            .iter()
            .any(|m| m.role == Role::System && m.content.starts_with("[summary]")));
        assert!(report.after < report.before);
    }
}
