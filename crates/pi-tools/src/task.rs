//! `task` tool — delegate a subtask to a child agent instance.
//!
//! Why split this out from `bash`/`webfetch`/etc:
//!
//! The TS pi `task` tool spawns a fresh agent loop with a narrowed system
//! prompt and a hard step cap so the parent agent can fan out into a
//! research / fix / classify subtask without polluting its own conversation
//! window. The child runs in isolation: no shared session id, no inherited
//! transcript, no extra `task` recursion (we strip the `task` tool from the
//! child's tool registry to prevent runaway fan-out).
//!
//! Implementation split:
//!
//! - This file lives in `pi-tools` because every tool is registered through
//!   the `ToolRuntime`. We declare a [`SubagentSpawner`] trait the parent
//!   can plug into; the tool just forwards the prompt and reports back.
//! - The actual subagent loop lives in `pi-agent` (so the trait can borrow
//!   the parent's provider / config / session store). pi-agent registers
//!   the tool after building its runtime.
//!
//! Parity target: `packages/agent/src/tools/task.ts`.

use std::sync::Arc;

use pi_core::{PiError, PiErrorKind, PiResult, ToolSchema};
use pi_permissions::{Capability, PermissionEngine, PermissionRequest};
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolInput, ToolOutput};

const DEFAULT_MAX_STEPS: u32 = 8;
const MAX_STEP_CAP: u32 = 32;

/// Plugged in by pi-agent (or any binary that owns an agent runtime). The
/// returned `String` is the child agent's final assistant message.
pub trait SubagentSpawner: Send + Sync {
    fn spawn(&self, request: SubagentRequest) -> PiResult<String>;
}

#[derive(Debug, Clone)]
pub struct SubagentRequest {
    pub prompt: String,
    pub system: Option<String>,
    pub max_steps: u32,
}

pub struct TaskTool {
    spawner: Arc<dyn SubagentSpawner>,
}

impl TaskTool {
    pub fn new(spawner: Arc<dyn SubagentSpawner>) -> Self {
        Self { spawner }
    }
}

impl std::fmt::Debug for TaskTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskTool").finish_non_exhaustive()
    }
}

#[derive(Debug, Deserialize, Default)]
struct TaskInput {
    prompt: String,
    #[serde(default)]
    system: Option<String>,
    #[serde(default)]
    max_steps: Option<u32>,
}

impl Tool for TaskTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "task".to_string(),
            description:
                "委派一个子任务给独立的子代理；子代理拥有自己的会话和步数上限，不能再调用 task"
                    .to_string(),
            input_shape: "json".to_string(),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "子任务的目标"},
                    "system": {"type": "string", "description": "可选的子代理系统提示词"},
                    "max_steps": {"type": "integer", "minimum": 1, "maximum": MAX_STEP_CAP, "default": DEFAULT_MAX_STEPS}
                },
                "required": ["prompt"],
                "additionalProperties": false
            })),
            mutates: true,
        }
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        let parsed: TaskInput = if input.value.is_object() {
            serde_json::from_value(input.value.clone())?
        } else {
            TaskInput {
                prompt: input.raw.clone(),
                ..TaskInput::default()
            }
        };
        let prompt = parsed.prompt.trim().to_string();
        if prompt.is_empty() {
            return Err(PiError::new(
                PiErrorKind::InvalidInput,
                "task prompt 不能为空",
            ));
        }
        let max_steps = parsed
            .max_steps
            .unwrap_or(DEFAULT_MAX_STEPS)
            .min(MAX_STEP_CAP);
        permissions.require(PermissionRequest {
            capability: Capability::ExtensionHostcall,
            target: "task:subagent".to_string(),
            reason: format!("派发子代理任务（最多 {max_steps} 步）"),
        })?;
        let result = self.spawner.spawn(SubagentRequest {
            prompt,
            system: parsed.system,
            max_steps,
        })?;
        Ok(ToolOutput {
            name: "task".to_string(),
            output: result,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct CapturingSpawner {
        requests: Mutex<Vec<SubagentRequest>>,
        answer: String,
    }

    impl SubagentSpawner for CapturingSpawner {
        fn spawn(&self, request: SubagentRequest) -> PiResult<String> {
            let answer = self.answer.clone();
            if let Ok(mut log) = self.requests.lock() {
                log.push(request);
            }
            Ok(answer)
        }
    }

    fn engine() -> PermissionEngine {
        PermissionEngine::new(pi_permissions::PermissionMode::TrustedWorkspace)
    }

    #[test]
    fn schema_caps_max_steps_and_lists_required_prompt() {
        let dummy = Arc::new(CapturingSpawner {
            requests: Mutex::new(Vec::new()),
            answer: String::new(),
        });
        let tool = TaskTool::new(dummy);
        let schema = tool.schema();
        assert_eq!(schema.name, "task");
        let required = schema
            .parameters
            .as_ref()
            .and_then(|v| v.get("required"))
            .and_then(|v| v.as_array())
            .expect("required");
        assert!(required.iter().any(|v| v.as_str() == Some("prompt")));
    }

    #[test]
    fn forwards_prompt_and_max_steps_into_spawner() {
        let spawner = Arc::new(CapturingSpawner {
            requests: Mutex::new(Vec::new()),
            answer: "result text".to_string(),
        });
        let tool = TaskTool::new(spawner.clone());
        let input = ToolInput {
            value: json!({"prompt": "find bugs", "max_steps": 4, "system": "be terse"}),
            raw: String::new(),
        };
        let out = tool.run(&input, &mut engine()).expect("run");
        assert_eq!(out.output, "result text");
        let log = match spawner.requests.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].prompt, "find bugs");
        assert_eq!(log[0].max_steps, 4);
        assert_eq!(log[0].system.as_deref(), Some("be terse"));
    }

    #[test]
    fn rejects_empty_prompt() {
        let spawner = Arc::new(CapturingSpawner {
            requests: Mutex::new(Vec::new()),
            answer: String::new(),
        });
        let tool = TaskTool::new(spawner);
        let input = ToolInput {
            value: json!({"prompt": "   "}),
            raw: String::new(),
        };
        let err = tool.run(&input, &mut engine()).expect_err("should err");
        assert_eq!(err.kind, PiErrorKind::InvalidInput);
    }
}
