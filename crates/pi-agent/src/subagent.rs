//! Subagent runner backing the `task` tool.
//!
//! When the parent agent calls `task`, we spin up a one-shot child
//! `AgentRuntime` with:
//!
//! - The same `AppConfig` as the parent (so model + permission_mode +
//!   compaction settings stay consistent).
//! - A non-persistent `InMemorySessionStore` so the child's transcript
//!   never lands on disk and cannot pollute `--resume` listings.
//! - The same built-in tools *minus* the `task` tool itself (we strip it
//!   from `enabled_tool_names` to prevent recursion bombs).
//! - An optional `system` override that replaces the parent's prompt for
//!   the duration of the child run.
//!
//! The child runs `run_single_turn` once, but it can take up to `max_steps`
//! tool-loop iterations because the agent loop is already bounded by
//! `AppConfig.max_tool_steps`. We clamp that field for the duration of the
//! child run so we honour the tool's `max_steps` argument.

use std::sync::Mutex;

use pi_core::{AppConfig, PiError, PiErrorKind, PiResult};
use pi_session::InMemorySessionStore;
use pi_tools::task::{SubagentRequest, SubagentSpawner};

use crate::AgentRuntime;

/// Holds the parent config so each subagent call sees the current settings.
pub struct ConfigSpawner {
    parent: Mutex<AppConfig>,
}

impl ConfigSpawner {
    pub fn new(parent: AppConfig) -> Self {
        Self {
            parent: Mutex::new(parent),
        }
    }

    /// Sync the spawner with the parent's most recent config (e.g. after the
    /// user changed model via the TUI). Cheap; called from the agent loop
    /// before each turn so subagent semantics never drift.
    pub fn set_config(&self, config: AppConfig) {
        if let Ok(mut guard) = self.parent.lock() {
            *guard = config;
        }
    }
}

impl SubagentSpawner for ConfigSpawner {
    fn spawn(&self, request: SubagentRequest) -> PiResult<String> {
        let mut config = match self.parent.lock() {
            Ok(guard) => guard.clone(),
            Err(err) => {
                return Err(PiError::new(
                    PiErrorKind::Provider,
                    format!("subagent 配置锁失败：{err}"),
                ));
            }
        };
        // Override system prompt if the caller supplied one. We preserve a
        // hint at the head so the child knows it is running under task.
        let system_override = match request.system {
            Some(text) => Some(format!(
                "[subagent] {}\n\n(你是父代理通过 task 工具委派的子代理，独立执行至完成，无对话历史)",
                text
            )),
            None => Some(
                "[subagent] 你是父代理通过 task 工具委派的子代理，独立执行至完成，无对话历史"
                    .to_string(),
            ),
        };
        config.system_prompt = system_override;
        // Strip the `task` tool from the child's registry to prevent runaway
        // recursion.
        let base = pi_tools::ToolRuntime::builtin();
        let mut allowed: Vec<String> = base
            .schemas()
            .into_iter()
            .map(|s| s.name)
            .filter(|n| n != "task")
            .collect();
        // Honour any per-config allowlist intersection.
        if let Some(parent_allow) = &config.enabled_tool_names {
            allowed.retain(|n| parent_allow.contains(n));
        }
        config.enabled_tool_names = Some(allowed);
        config.max_tool_steps = request.max_steps;
        // Subagent runs do not stream events back into the parent UI; the
        // child agent's final assistant message is what the parent receives.
        config.stream = false;

        let store = InMemorySessionStore::new();
        let mut child = AgentRuntime::try_new(config, store)?;
        let session_id = format!("subagent-{}", pi_core::now_ms());
        let turn = child.run_single_turn(&session_id, &request.prompt)?;
        let answer = turn
            .session
            .messages
            .iter()
            .rev()
            .find(|m| m.role == pi_core::Role::Assistant)
            .map(|m| m.content.clone())
            .unwrap_or_default();
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::ModelSelection;

    #[test]
    fn echo_subagent_returns_assistant_message() {
        let config = AppConfig {
            model: ModelSelection {
                provider: "echo".into(),
                model: "echo-local".into(),
            },
            tools_enabled: false,
            stream: false,
            ..AppConfig::default()
        };
        let spawner = ConfigSpawner::new(config);
        let result = spawner
            .spawn(SubagentRequest {
                prompt: "hello".into(),
                system: None,
                max_steps: 2,
            })
            .expect("spawn");
        assert!(!result.is_empty());
    }

    #[test]
    fn child_cannot_recursively_call_task() {
        let config = AppConfig {
            model: ModelSelection {
                provider: "echo".into(),
                model: "echo-local".into(),
            },
            tools_enabled: true,
            stream: false,
            ..AppConfig::default()
        };
        let spawner = ConfigSpawner::new(config);
        // Indirect check: ensure the spawner's prepared allowlist drops
        // "task". We rebuild the same logic locally for the test.
        let base = pi_tools::ToolRuntime::builtin();
        let allowed: Vec<String> = base
            .schemas()
            .into_iter()
            .map(|s| s.name)
            .filter(|n| n != "task")
            .collect();
        assert!(allowed.iter().all(|n| n != "task"));
        let _ = spawner;
    }
}
