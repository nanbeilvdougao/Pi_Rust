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

pub type NotifySink = Arc<Mutex<Vec<String>>>;
pub type EventSink = Arc<Mutex<Vec<(String, String)>>>;

/// One entry in the resource catalogue exposed to extensions. Mirrors the
/// MCP resource shape so an extension can also iterate `.pi/resources/*`
/// files served by the host directly.
#[derive(Debug, Clone)]
pub struct ResourceEntry {
    pub uri: String,
    pub mime_type: Option<String>,
    pub description: Option<String>,
    pub body: Vec<u8>,
}

/// One entry in the prompt catalogue, materialized from `.pi/prompts/<name>.md`
/// with `{{argument}}` substitution.
#[derive(Debug, Clone)]
pub struct PromptEntry {
    pub name: String,
    pub description: Option<String>,
    pub template: String,
}

/// Bridges hostcalls into a `ToolRuntime`. `Hostcall::Tool` calls are
/// translated to `ToolCall { name, input }` and dispatched.
pub struct ToolBridge<'a> {
    pub tools: &'a ToolRuntime,
    pub permissions: &'a mut PermissionEngine,
    /// Optional sink for `Hostcall::UiNotify`; the agent uses this to surface
    /// extension events to the TUI.
    pub notify_sink: Option<NotifySink>,
    /// Optional sink for `Hostcall::NotifyEvent`; the agent forwards it into
    /// its `Event::ToolProgress` channel so the TUI streaming widget renders
    /// extension progress alongside built-in tools.
    pub event_sink: Option<EventSink>,
    /// Optional resource catalogue (local + MCP-aggregated). Empty by
    /// default; the agent wires this on first `ResourceList` call.
    pub resources: Vec<ResourceEntry>,
    /// Optional prompt catalogue. Same provenance as `resources`.
    pub prompts: Vec<PromptEntry>,
}

impl<'a> ToolBridge<'a> {
    pub fn new(tools: &'a ToolRuntime, permissions: &'a mut PermissionEngine) -> Self {
        Self {
            tools,
            permissions,
            notify_sink: None,
            event_sink: None,
            resources: Vec::new(),
            prompts: Vec::new(),
        }
    }

    pub fn with_notify_sink(mut self, sink: Arc<Mutex<Vec<String>>>) -> Self {
        self.notify_sink = Some(sink);
        self
    }

    pub fn with_event_sink(mut self, sink: Arc<Mutex<Vec<(String, String)>>>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    pub fn with_resources(mut self, resources: Vec<ResourceEntry>) -> Self {
        self.resources = resources;
        self
    }

    pub fn with_prompts(mut self, prompts: Vec<PromptEntry>) -> Self {
        self.prompts = prompts;
        self
    }

    pub fn available_tools(&self) -> Vec<ToolSchema> {
        self.tools.schemas()
    }

    /// Walk `<workspace>/.pi/resources/*` and load each file as a resource
    /// entry. URIs use `file://` so the schema stays uniform with MCP.
    pub fn load_workspace_resources(root: &std::path::Path) -> Vec<ResourceEntry> {
        let dir = root.join(".pi").join("resources");
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let body = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let mime_type = mime_for(&path);
            out.push(ResourceEntry {
                uri: format!("file://{}", path.display()),
                mime_type,
                description: None,
                body,
            });
        }
        out
    }

    /// Walk `<workspace>/.pi/prompts/*.md` and load each file as a prompt
    /// template. Frontmatter parsing is left out by design — the template
    /// body is the markdown verbatim; `{{name}}` placeholders interpolate
    /// from the `arguments` JSON object passed to `prompts/get`.
    pub fn load_workspace_prompts(root: &std::path::Path) -> Vec<PromptEntry> {
        let dir = root.join(".pi").join("prompts");
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let template = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            out.push(PromptEntry {
                name,
                description: None,
                template,
            });
        }
        out
    }
}

fn mime_for(path: &std::path::Path) -> Option<String> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    Some(
        match ext.as_str() {
            "md" | "markdown" => "text/markdown",
            "txt" => "text/plain",
            "json" => "application/json",
            "toml" => "application/toml",
            "yaml" | "yml" => "application/yaml",
            "html" | "htm" => "text/html",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "csv" => "text/csv",
            _ => return None,
        }
        .to_string(),
    )
}

fn interpolate(template: &str, arguments: &serde_json::Value) -> String {
    let map = arguments.as_object();
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 4 <= bytes.len() && &bytes[i..i + 2] == b"{{" {
            // Find closing `}}`.
            let rest = &template[i + 2..];
            if let Some(end) = rest.find("}}") {
                let key = rest[..end].trim();
                let value = map
                    .and_then(|m| m.get(key))
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default();
                out.push_str(&value);
                i = i + 2 + end + 2;
                continue;
            }
        }
        let ch = template[i..].chars().next().unwrap_or('?');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
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
            Hostcall::NotifyEvent { method, params } => {
                if let Some(sink) = &self.event_sink {
                    if let Ok(mut buf) = sink.lock() {
                        buf.push((method.clone(), params.clone()));
                    }
                }
                Ok(serde_json::json!({"ack": true}))
            }
            Hostcall::ResourceList => Ok(serde_json::json!({
                "resources": self.resources.iter().map(|r| serde_json::json!({
                    "uri": r.uri,
                    "mimeType": r.mime_type,
                    "description": r.description,
                })).collect::<Vec<_>>()
            })),
            Hostcall::ResourceRead { uri } => {
                let entry = self
                    .resources
                    .iter()
                    .find(|r| r.uri == *uri)
                    .ok_or_else(|| format!("未知 resource：{uri}"))?;
                let text = match std::str::from_utf8(&entry.body) {
                    Ok(s) => s.to_string(),
                    Err(_) => pi_core::base64_encode(&entry.body),
                };
                Ok(serde_json::json!({
                    "contents": [{
                        "uri": entry.uri,
                        "mimeType": entry.mime_type,
                        "text": text,
                    }]
                }))
            }
            Hostcall::PromptList => Ok(serde_json::json!({
                "prompts": self.prompts.iter().map(|p| serde_json::json!({
                    "name": p.name,
                    "description": p.description,
                })).collect::<Vec<_>>()
            })),
            Hostcall::PromptGet { name, arguments } => {
                let entry = self
                    .prompts
                    .iter()
                    .find(|p| p.name == *name)
                    .ok_or_else(|| format!("未知 prompt：{name}"))?;
                let args_value: serde_json::Value =
                    serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);
                let rendered = interpolate(&entry.template, &args_value);
                Ok(serde_json::json!({
                    "description": entry.description,
                    "messages": [{
                        "role": "user",
                        "content": {"type": "text", "text": rendered}
                    }]
                }))
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
    fn resource_list_returns_loaded_entries() {
        let tools = ToolRuntime::builtin();
        let mut permissions = PermissionEngine::new(PermissionMode::TrustedWorkspace);
        let mut bridge = ToolBridge::new(&tools, &mut permissions).with_resources(vec![
            ResourceEntry {
                uri: "file:///a.md".into(),
                mime_type: Some("text/markdown".into()),
                description: None,
                body: b"alpha".to_vec(),
            },
            ResourceEntry {
                uri: "file:///b.txt".into(),
                mime_type: Some("text/plain".into()),
                description: None,
                body: b"bravo".to_vec(),
            },
        ]);
        let listed = bridge.resolve(&Hostcall::ResourceList).expect("list");
        let names: Vec<&str> = listed["resources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v["uri"].as_str())
            .collect();
        assert_eq!(names, vec!["file:///a.md", "file:///b.txt"]);
        let read = bridge
            .resolve(&Hostcall::ResourceRead {
                uri: "file:///b.txt".into(),
            })
            .expect("read");
        assert_eq!(read["contents"][0]["text"], "bravo");
    }

    #[test]
    fn prompt_get_interpolates_arguments() {
        let tools = ToolRuntime::builtin();
        let mut permissions = PermissionEngine::new(PermissionMode::TrustedWorkspace);
        let mut bridge =
            ToolBridge::new(&tools, &mut permissions).with_prompts(vec![PromptEntry {
                name: "greet".into(),
                description: Some("greet someone".into()),
                template: "Hello {{name}}, today is {{day}}.".into(),
            }]);
        let result = bridge
            .resolve(&Hostcall::PromptGet {
                name: "greet".into(),
                arguments: r#"{"name":"Pi","day":"周二"}"#.into(),
            })
            .expect("prompt");
        let text = result["messages"][0]["content"]["text"].as_str().unwrap();
        assert_eq!(text, "Hello Pi, today is 周二.");
    }

    #[test]
    fn notify_event_lands_in_event_sink() {
        let tools = ToolRuntime::builtin();
        let mut permissions = PermissionEngine::new(PermissionMode::TrustedWorkspace);
        let sink: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut bridge = ToolBridge::new(&tools, &mut permissions).with_event_sink(sink.clone());
        bridge
            .resolve(&Hostcall::NotifyEvent {
                method: "progress".into(),
                params: r#"{"step":3}"#.into(),
            })
            .expect("notify");
        let log = sink.lock().unwrap().clone();
        assert_eq!(log, vec![("progress".into(), "{\"step\":3}".into())]);
    }

    #[test]
    fn workspace_resource_and_prompt_loaders_walk_dot_pi_directories() {
        let dir = tempdir().unwrap();
        let resources_dir = dir.path().join(".pi").join("resources");
        let prompts_dir = dir.path().join(".pi").join("prompts");
        std::fs::create_dir_all(&resources_dir).unwrap();
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(resources_dir.join("notes.md"), "# notes").unwrap();
        std::fs::write(prompts_dir.join("hi.md"), "Hello {{name}}").unwrap();

        let resources = ToolBridge::load_workspace_resources(dir.path());
        assert_eq!(resources.len(), 1);
        assert!(resources[0].uri.ends_with("notes.md"));
        assert_eq!(resources[0].mime_type.as_deref(), Some("text/markdown"));

        let prompts = ToolBridge::load_workspace_prompts(dir.path());
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "hi");
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
