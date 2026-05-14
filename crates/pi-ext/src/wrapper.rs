//! Wrap pi-tools / pi-mcp / sibling extension surfaces as
//! `HostcallResolver` implementations so a running extension can call them
//! through the same JSON-RPC channel it already uses for `pi-core`-defined
//! hostcalls. Mirrors TS pi's `extensions/wrapper.ts`.
//!
//! Conceptually:
//!
//! ```text
//!     Extension                Wrapper                pi-tools / MCP
//!     ─────────                ───────                ──────────────
//!     Hostcall::Tool  ─────►   resolve()  ─────►     ToolRuntime.run()
//!                              permissions.require()
//!     Hostcall::UiNotify ───►  on_event sink (Event::ToolFinished{...})
//! ```
//!
//! The wrapper does *not* short-circuit the permission engine — it asks the
//! engine before forwarding. That keeps the security guarantee that
//! "extensions never see capabilities they did not declare" intact.

use std::sync::{Arc, Mutex};

use pi_core::ToolSchema;
use pi_permissions::PermissionEngine;
use pi_tools::{ToolCall, ToolRuntime};

use crate::process::HostcallResolver;
use crate::Hostcall;

/// Bridges hostcalls into a `ToolRuntime`. `Hostcall::Tool` calls are
/// translated to `ToolCall { name, input }` and dispatched.
pub struct ToolBridge<'a> {
    pub tools: &'a ToolRuntime,
    pub permissions: &'a mut PermissionEngine,
    /// Optional sink for `Hostcall::UiNotify`; the agent uses this to surface
    /// extension events to the TUI.
    pub notify_sink: Option<Arc<Mutex<Vec<String>>>>,
}

impl<'a> ToolBridge<'a> {
    pub fn new(tools: &'a ToolRuntime, permissions: &'a mut PermissionEngine) -> Self {
        Self {
            tools,
            permissions,
            notify_sink: None,
        }
    }

    pub fn with_notify_sink(mut self, sink: Arc<Mutex<Vec<String>>>) -> Self {
        self.notify_sink = Some(sink);
        self
    }

    pub fn available_tools(&self) -> Vec<ToolSchema> {
        self.tools.schemas()
    }
}

impl<'a> HostcallResolver for ToolBridge<'a> {
    fn resolve(&mut self, call: &Hostcall) -> Result<serde_json::Value, String> {
        match call {
            Hostcall::Tool { name, input } => {
                let output = self
                    .tools
                    .run(
                        ToolCall {
                            name: name.clone(),
                            input: input.clone(),
                        },
                        self.permissions,
                    )
                    .map_err(|err| err.message)?;
                Ok(serde_json::json!({
                    "name": output.name,
                    "output": output.output,
                }))
            }
            Hostcall::UiNotify { message } => {
                if let Some(sink) = &self.notify_sink {
                    if let Ok(mut buf) = sink.lock() {
                        buf.push(message.clone());
                    }
                }
                Ok(serde_json::json!({"ack": true}))
            }
            // Session / Http live outside the tool runtime; callers should
            // chain a different resolver for those. We return an explicit
            // "not supported here" so the extension can fail fast.
            Hostcall::SessionRead { .. }
            | Hostcall::SessionWrite { .. }
            | Hostcall::Http { .. } => Err(format!(
                "ToolBridge 不直接处理 {} 类 hostcall；请挂接对应的 resolver",
                call.required_capability().as_str()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_permissions::PermissionMode;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn tool_hostcall_round_trips_through_runtime() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        fs::write(&file, "hi\n").unwrap();
        let tools = ToolRuntime::builtin();
        let mut permissions = PermissionEngine::new(PermissionMode::TrustedWorkspace);
        let mut bridge = ToolBridge::new(&tools, &mut permissions);
        let payload = serde_json::json!({"path": file});
        let result = bridge
            .resolve(&Hostcall::Tool {
                name: "read".to_string(),
                input: payload.to_string(),
            })
            .expect("resolve");
        assert_eq!(result["name"], "read");
        assert!(result["output"].as_str().unwrap().contains("hi"));
    }

    #[test]
    fn ui_notify_lands_in_sink() {
        let tools = ToolRuntime::builtin();
        let mut permissions = PermissionEngine::new(PermissionMode::TrustedWorkspace);
        let sink: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut bridge = ToolBridge::new(&tools, &mut permissions).with_notify_sink(sink.clone());
        bridge
            .resolve(&Hostcall::UiNotify {
                message: "hello".to_string(),
            })
            .expect("resolve");
        assert_eq!(sink.lock().unwrap().clone(), vec!["hello".to_string()]);
    }

    #[test]
    fn unsupported_hostcall_returns_actionable_error() {
        let tools = ToolRuntime::builtin();
        let mut permissions = PermissionEngine::new(PermissionMode::TrustedWorkspace);
        let mut bridge = ToolBridge::new(&tools, &mut permissions);
        let err = bridge
            .resolve(&Hostcall::Http {
                method: "GET".into(),
                url: "https://example.com".into(),
            })
            .unwrap_err();
        assert!(err.contains("network"));
    }
}
