//! Model Context Protocol (MCP) client for Pi Rust.
//!
//! MCP servers expose `tools`, `resources`, and `prompts` over JSON-RPC 2.0
//! framed as line-delimited JSON over stdio (or, for the network transport,
//! Server-Sent Events — out of scope here). Pi Rust speaks the stdio variant
//! because it composes cleanly with the existing process-host pattern:
//! every MCP `tool` becomes a first-class `pi-tools::Tool`, and every tool
//! call is gated through `pi-permissions` exactly like a built-in tool.
//!
//! Server configuration lives in `.pi/mcp.toml`:
//!
//! ```toml
//! [[servers]]
//! id = "files"
//! command = "mcp-files"
//! args = ["--root", "."]
//!
//! [servers.env]
//! LOG_LEVEL = "info"
//! ```
//!
//! Once `McpManager::load_workspace(...)` returns, call `register_into(...)`
//! to splice every advertised tool into a [`pi_tools::ToolRuntime`]. The
//! manager keeps a long-running child process per server so the agent can
//! re-use a single MCP handshake across many tool invocations.
//!
//! What we deliberately do *not* implement here (yet): MCP `resources`,
//! `prompts`, sampling notifications, SSE transport, capability negotiation
//! flags beyond `tools/list`. Adding them is additive — they map onto the
//! same JSON-RPC frame loop.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};

use pi_core::{PiError, PiErrorKind, PiResult, ToolSchema};
use pi_permissions::{Capability, PermissionEngine, PermissionRequest};
use pi_tools::{Tool, ToolInput, ToolOutput, ToolRuntime};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub mod config;
pub use config::{McpConfig, ServerSpec};

/// Wire envelope for JSON-RPC 2.0 messages we send.
#[derive(Debug, Serialize)]
struct Request<'a> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Value::is_null")]
    params: Value,
}

#[derive(Debug, Deserialize)]
struct Response {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    #[allow(dead_code)]
    code: i32,
    message: String,
}

/// Live connection to a single MCP server.
pub struct McpServer {
    id: String,
    inner: Arc<Mutex<ServerInner>>,
    tools: Vec<RemoteToolSchema>,
}

struct ServerInner {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct RemoteToolSchema {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default, rename = "inputSchema")]
    input_schema: Option<Value>,
}

impl McpServer {
    pub fn spawn(spec: &ServerSpec) -> PiResult<Self> {
        let mut command = Command::new(&spec.command);
        command.args(&spec.args);
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|err| {
            PiError::new(
                PiErrorKind::Tool,
                format!("启动 MCP server {}: {err}", spec.id),
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| PiError::new(PiErrorKind::Tool, "MCP server 缺少 stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PiError::new(PiErrorKind::Tool, "MCP server 缺少 stdout"))?;
        let reader = BufReader::new(stdout);
        let inner = Arc::new(Mutex::new(ServerInner {
            child,
            stdin,
            reader,
            next_id: 1,
        }));

        // Initialize handshake.
        Self::rpc_request(
            &inner,
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "pi-rust", "version": pi_core::VERSION},
            }),
        )?;
        Self::notify(&inner, "initialized")?;

        // Enumerate remote tools.
        let tool_list = Self::rpc_request(&inner, "tools/list", json!({}))?;
        let tools: Vec<RemoteToolSchema> = tool_list
            .get("tools")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        Ok(Self {
            id: spec.id.clone(),
            inner,
            tools,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .iter()
            .map(|tool| ToolSchema {
                name: format!("mcp:{}/{}", self.id, tool.name),
                description: tool.description.clone(),
                input_shape: "json".to_string(),
                parameters: tool.input_schema.clone(),
                mutates: true, // conservative: assume MCP tools mutate
            })
            .collect()
    }

    fn rpc_request(
        inner: &Arc<Mutex<ServerInner>>,
        method: &str,
        params: Value,
    ) -> PiResult<Value> {
        let mut guard = inner
            .lock()
            .map_err(|err| PiError::new(PiErrorKind::Tool, format!("MCP lock 失败：{err}")))?;
        guard.next_id += 1;
        let id = guard.next_id;
        let request = Request {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let mut line = serde_json::to_string(&request)?;
        line.push('\n');
        guard.stdin.write_all(line.as_bytes()).map_err(|err| {
            PiError::new(PiErrorKind::Tool, format!("写入 MCP stdin 失败：{err}"))
        })?;
        guard.stdin.flush().map_err(|err| {
            PiError::new(PiErrorKind::Tool, format!("MCP stdin flush 失败：{err}"))
        })?;
        loop {
            let mut buf = String::new();
            let n = guard.reader.read_line(&mut buf).map_err(|err| {
                PiError::new(PiErrorKind::Tool, format!("读取 MCP stdout 失败：{err}"))
            })?;
            if n == 0 {
                return Err(PiError::new(
                    PiErrorKind::Tool,
                    "MCP server 关闭了 stdout".to_string(),
                ));
            }
            let trimmed = buf.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                continue;
            }
            let response: Response = serde_json::from_str(trimmed)?;
            if response.id == Some(id) {
                if let Some(err) = response.error {
                    return Err(PiError::new(
                        PiErrorKind::Tool,
                        format!("MCP {method} 失败：{}", err.message),
                    ));
                }
                return Ok(response.result.unwrap_or(Value::Null));
            }
            // Notifications and unrelated responses get dropped — fine for
            // our single-flight call pattern.
        }
    }

    fn notify(inner: &Arc<Mutex<ServerInner>>, method: &str) -> PiResult<()> {
        let mut guard = inner
            .lock()
            .map_err(|err| PiError::new(PiErrorKind::Tool, format!("MCP lock 失败：{err}")))?;
        let frame = json!({"jsonrpc": "2.0", "method": format!("notifications/{method}")});
        let mut line = serde_json::to_string(&frame)?;
        line.push('\n');
        guard
            .stdin
            .write_all(line.as_bytes())
            .map_err(|err| PiError::new(PiErrorKind::Tool, format!("写入 MCP 通知失败：{err}")))?;
        guard.stdin.flush().map_err(|err| {
            PiError::new(PiErrorKind::Tool, format!("MCP stdin flush 失败：{err}"))
        })?;
        Ok(())
    }

    pub fn call_tool(&self, tool_name: &str, arguments: Value) -> PiResult<String> {
        let result = Self::rpc_request(
            &self.inner,
            "tools/call",
            json!({"name": tool_name, "arguments": arguments}),
        )?;
        // MCP returns `content: [{type:"text", text:"..."}]`; concatenate.
        let mut out = String::new();
        if let Some(items) = result.get("content").and_then(|v| v.as_array()) {
            for item in items {
                if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                    out.push_str(text);
                }
            }
        }
        if out.is_empty() {
            out = serde_json::to_string(&result).unwrap_or_default();
        }
        Ok(out)
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.inner.lock() {
            let _ = guard.child.kill();
            let _ = guard.child.wait();
        }
    }
}

pub struct McpManager {
    servers: BTreeMap<String, Arc<McpServer>>,
}

impl McpManager {
    pub fn empty() -> Self {
        Self {
            servers: BTreeMap::new(),
        }
    }

    pub fn load_workspace(root: &Path) -> PiResult<Self> {
        let config = McpConfig::load_workspace(root)?;
        Self::from_config(&config)
    }

    pub fn from_config(config: &McpConfig) -> PiResult<Self> {
        let mut servers = BTreeMap::new();
        for spec in &config.servers {
            let server = McpServer::spawn(spec)?;
            servers.insert(spec.id.clone(), Arc::new(server));
        }
        Ok(Self { servers })
    }

    pub fn server_ids(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    /// Wire every advertised MCP tool into `runtime` as a `Tool` that runs
    /// `tools/call` on the remote server.
    pub fn register_into(&self, runtime: &mut ToolRuntime) {
        for (id, server) in &self.servers {
            for schema in server.schemas() {
                runtime.register(Box::new(McpTool {
                    schema,
                    server_id: id.clone(),
                    server: server.clone(),
                }));
            }
        }
    }
}

struct McpTool {
    schema: ToolSchema,
    #[allow(dead_code)]
    server_id: String,
    server: Arc<McpServer>,
}

impl Tool for McpTool {
    fn schema(&self) -> ToolSchema {
        self.schema.clone()
    }

    fn run(&self, input: &ToolInput, permissions: &mut PermissionEngine) -> PiResult<ToolOutput> {
        permissions.require(PermissionRequest {
            capability: Capability::ExtensionHostcall,
            target: self.schema.name.clone(),
            reason: format!("MCP 工具 {} 调用", self.schema.name),
        })?;
        let remote_name = self
            .schema
            .name
            .split('/')
            .next_back()
            .unwrap_or(&self.schema.name)
            .to_string();
        let arguments = if input.value.is_object() {
            input.value.clone()
        } else {
            json!({"input": input.raw})
        };
        let output = self.server.call_tool(&remote_name, arguments)?;
        Ok(ToolOutput {
            name: self.schema.name.clone(),
            output,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_python_echo_server(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("echo_server.py");
        fs::write(
            &path,
            r#"
import sys
import json

def send(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"capabilities": {"tools": {}}, "serverInfo": {"name": "echo", "version": "0.0.1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"tools": [{"name": "echo", "description": "echo the input", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}})
    elif method == "tools/call":
        text = req.get("params", {}).get("arguments", {}).get("text", "")
        send({"jsonrpc": "2.0", "id": req["id"], "result": {"content": [{"type": "text", "text": "echo:" + text}]}})
    elif method and method.startswith("notifications/"):
        pass
    else:
        send({"jsonrpc": "2.0", "id": req.get("id"), "error": {"code": -32601, "message": "unknown method"}})
"#,
        )
        .unwrap();
        path
    }

    #[test]
    fn manager_lists_and_calls_remote_tool() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            // No python3 on this host; skip.
            return;
        }
        let dir = tempdir().unwrap();
        let script = write_python_echo_server(dir.path());
        let config = McpConfig {
            servers: vec![ServerSpec {
                id: "echo".to_string(),
                command: "python3".to_string(),
                args: vec![script.display().to_string()],
                env: BTreeMap::new(),
            }],
        };
        let manager = McpManager::from_config(&config).expect("spawn");
        let mut runtime = ToolRuntime::default();
        manager.register_into(&mut runtime);
        let schemas: Vec<_> = runtime.schemas().into_iter().map(|s| s.name).collect();
        assert!(schemas.iter().any(|s| s == "mcp:echo/echo"));
        let mut perms = PermissionEngine::new(pi_permissions::PermissionMode::TrustedWorkspace);
        let output = runtime
            .run(
                pi_tools::ToolCall {
                    name: "mcp:echo/echo".to_string(),
                    input: r#"{"text":"hello"}"#.to_string(),
                },
                &mut perms,
            )
            .expect("run");
        assert_eq!(output.output, "echo:hello");
    }
}
