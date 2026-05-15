//! Host-side MCP callbacks.
//!
//! MCP defines two server→client surfaces that the host MUST implement for
//! a parity-feature-complete client:
//!
//! - `sampling/createMessage` — the server asks the host to run an LLM call.
//!   We satisfy it by spawning an ephemeral provider request against the
//!   parent agent's `AppConfig`, then return `{role:"assistant", content:…}`.
//! - `notifications/progress` — the server pushes incremental progress
//!   updates while a long-running tool is in flight. We forward them to a
//!   shared `Vec<Event>` the agent merges into the turn's event stream.
//!
//! These dispatchers stay tiny so they can plug into any [`McpManager`]
//! without dragging the full agent runtime through generics.

use std::sync::{Arc, Mutex};

use pi_core::{AppConfig, Event, Message, PiError, PiErrorKind, PiResult, Role};
use pi_mcp::{ProgressHandler, SamplingHandler};
use pi_providers::{provider_for, ProviderRequest};
use serde_json::{json, Value};

/// Run inbound `sampling/createMessage` requests against the parent agent's
/// provider. We deliberately use `complete()` (not `stream()`) so the
/// response shape is fully formed before we hand it back.
pub struct AgentSamplingHandler {
    config: Mutex<AppConfig>,
}

impl AgentSamplingHandler {
    pub fn new(config: AppConfig) -> Self {
        Self {
            config: Mutex::new(config),
        }
    }

    pub fn set_config(&self, config: AppConfig) {
        if let Ok(mut guard) = self.config.lock() {
            *guard = config;
        }
    }
}

impl SamplingHandler for AgentSamplingHandler {
    fn create_message(&self, params: Value) -> PiResult<Value> {
        let config = self
            .config
            .lock()
            .map_err(|err| {
                PiError::new(PiErrorKind::Provider, format!("sampling 配置锁失败：{err}"))
            })?
            .clone();
        // Parse MCP `messages` -> `pi-core::Message` list.
        let mut messages: Vec<Message> = Vec::new();
        if let Some(items) = params.get("messages").and_then(|v| v.as_array()) {
            for item in items {
                let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                let text = item
                    .pointer("/content/text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let role = match role {
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    "system" => Role::System,
                    _ => Role::User,
                };
                messages.push(Message::new(role, text));
            }
        }
        if messages.is_empty() {
            return Err(PiError::new(
                PiErrorKind::InvalidInput,
                "sampling 请求缺少 messages",
            ));
        }
        let system_prompt = params
            .get("systemPrompt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| config.system_prompt.clone());
        let max_output_tokens = params
            .get("maxTokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        let temperature = params
            .get("temperature")
            .and_then(|v| v.as_f64())
            .map(|v| v as f32);
        let request = ProviderRequest {
            model: config.model.clone(),
            messages,
            tools: Vec::new(),
            system_prompt,
            max_output_tokens,
            temperature,
            stream: false,
        };
        let provider = provider_for(&config.model)?;
        let response = provider.complete(request)?;
        Ok(json!({
            "role": "assistant",
            "content": {
                "type": "text",
                "text": response.message.content,
            },
            "model": config.model.model,
            "stopReason": "endTurn"
        }))
    }
}

/// Captures `notifications/progress` from any MCP server and queues them as
/// `Event::ToolProgress`. The agent loop drains the queue at end-of-tool so
/// the events land in the same `Vec<Event>` the rest of the turn produces.
pub struct EventQueueProgressHandler {
    queue: Arc<Mutex<Vec<Event>>>,
}

impl EventQueueProgressHandler {
    pub fn new() -> (Arc<Mutex<Vec<Event>>>, Arc<Self>) {
        let queue = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(Self {
            queue: queue.clone(),
        });
        (queue, handler)
    }
}

impl ProgressHandler for EventQueueProgressHandler {
    fn on_progress(&self, server_id: &str, params: Value) {
        // MCP progress payload shape:
        // { "progressToken": "...", "progress": <num>, "total": <num>?, "message": "..."? }
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                let pct = params.get("progress").and_then(|v| v.as_f64());
                let total = params.get("total").and_then(|v| v.as_f64());
                match (pct, total) {
                    (Some(p), Some(t)) if t > 0.0 => format!("进度 {:.0}/{:.0}", p, t),
                    (Some(p), _) => format!("进度 {p:.0}"),
                    _ => "正在执行…".to_string(),
                }
            });
        if let Ok(mut guard) = self.queue.lock() {
            guard.push(Event::ToolProgress {
                name: format!("mcp:{server_id}"),
                line: message,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::ModelSelection;

    #[test]
    fn sampling_round_trips_through_echo_provider() {
        let config = AppConfig {
            model: ModelSelection {
                provider: "echo".into(),
                model: "echo-local".into(),
            },
            ..AppConfig::default()
        };
        let handler = AgentSamplingHandler::new(config);
        let params = json!({
            "messages": [{
                "role": "user",
                "content": {"type": "text", "text": "hello"}
            }]
        });
        let result = handler.create_message(params).expect("sampling");
        assert_eq!(result["role"], "assistant");
        assert!(result["content"]["text"].is_string());
    }

    #[test]
    fn progress_handler_appends_to_shared_queue() {
        let (queue, handler) = EventQueueProgressHandler::new();
        handler.on_progress(
            "myserver",
            json!({"progress": 5, "total": 10, "message": "halfway"}),
        );
        let drained = queue.lock().unwrap().clone();
        assert_eq!(drained.len(), 1);
        match &drained[0] {
            Event::ToolProgress { name, line } => {
                assert_eq!(name, "mcp:myserver");
                assert_eq!(line, "halfway");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn progress_handler_falls_back_to_progress_pct_string() {
        let (queue, handler) = EventQueueProgressHandler::new();
        handler.on_progress("s", json!({"progress": 3, "total": 7}));
        let drained = queue.lock().unwrap().clone();
        match &drained[0] {
            Event::ToolProgress { line, .. } => assert!(line.contains("3")),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
