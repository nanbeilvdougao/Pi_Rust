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
//! Beyond `tools/*`, the client also implements:
//! - `resources/list` + `resources/read` (advertised under `capabilities.resources`).
//! - `prompts/list` + `prompts/get` (advertised under `capabilities.prompts`).
//! - Inbound `sampling/createMessage` via `SamplingHandler` so an MCP server
//!   can borrow the host's model for its own reasoning.
//! - `notifications/progress` dispatch via `ProgressHandler` so long-running
//!   remote tools surface progress into the agent's event stream.
//! - `$/cancelRequest` outbound + an `AtomicBool` cancel flag honoured by
//!   the rpc read loop.
//!
//! Still out of scope: SSE transport (stdio only), HTTP transport, and the
//! optional `logging` capability beyond capturing it in `ServerCapabilities`.

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
#[allow(dead_code)]
struct Response {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
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
    resources: Vec<RemoteResource>,
    prompts: Vec<RemotePrompt>,
    capabilities: ServerCapabilities,
    sampling: Arc<Mutex<Option<Arc<dyn SamplingHandler>>>>,
    progress: Arc<Mutex<Option<Arc<dyn ProgressHandler>>>>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

/// Host-side handler for inbound `sampling/createMessage` requests. MCP
/// servers use this RPC to ask Pi (the client) to run an LLM completion on
/// their behalf — for example, an MCP "summarizer" server that wants to
/// reuse the user's model instead of bundling its own.
pub trait SamplingHandler: Send + Sync {
    /// Called once per inbound `sampling/createMessage` request. Receives
    /// the raw JSON params (per MCP spec: `messages`, `modelPreferences`,
    /// `systemPrompt`, `includeContext`, `maxTokens`, …). Returns the
    /// result body that becomes `{"role":"assistant","content":{type,text}}`
    /// on the wire.
    fn create_message(&self, params: Value) -> PiResult<Value>;
}

/// Host-side dispatcher for inbound `notifications/progress` notifications.
/// Lets MCP tools surface long-running progress into the agent event stream
/// without us needing to plumb it through every individual tool call site.
pub trait ProgressHandler: Send + Sync {
    fn on_progress(&self, server_id: &str, params: Value);
}

/// Capability flags advertised by the remote server's `initialize` response.
/// `tools` is always assumed (we already register every advertised tool);
/// `resources` / `prompts` gate the corresponding RPC calls so we don't
/// poke servers that don't speak those protocols.
#[derive(Debug, Clone, Default)]
pub struct ServerCapabilities {
    pub tools: bool,
    pub resources: bool,
    pub prompts: bool,
    pub logging: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteResource {
    pub uri: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, rename = "mimeType")]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemotePrompt {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<RemotePromptArgument>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemotePromptArgument {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required: Option<bool>,
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
        let init = Self::rpc_request(
            &inner,
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}, "resources": {}, "prompts": {}, "logging": {}},
                "clientInfo": {"name": "pi-rust", "version": pi_core::VERSION},
            }),
        )?;
        let capabilities = parse_capabilities(&init);
        Self::notify(&inner, "initialized", Value::Null)?;

        // Enumerate remote tools (every server we care about implements this).
        let tool_list = Self::rpc_request(&inner, "tools/list", json!({}))?;
        let tools: Vec<RemoteToolSchema> = tool_list
            .get("tools")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        // Resources / prompts are optional; skip the call if the server did
        // not advertise the capability.
        let resources = if capabilities.resources {
            let list = Self::rpc_request(&inner, "resources/list", json!({}))?;
            list.get("resources")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let prompts = if capabilities.prompts {
            let list = Self::rpc_request(&inner, "prompts/list", json!({}))?;
            list.get("prompts")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        Ok(Self {
            id: spec.id.clone(),
            inner,
            tools,
            resources,
            prompts,
            capabilities,
            sampling: Arc::new(Mutex::new(None)),
            progress: Arc::new(Mutex::new(None)),
            cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Plug a sampling handler. The host typically wires this to a closure
    /// that runs `AgentRuntime::run_single_turn` against the server's
    /// requested model and returns the final assistant content.
    pub fn set_sampling_handler(&self, handler: Arc<dyn SamplingHandler>) {
        if let Ok(mut guard) = self.sampling.lock() {
            *guard = Some(handler);
        }
    }

    /// Plug a progress handler. Inbound `notifications/progress` are
    /// forwarded here so the agent can emit `Event::ToolProgress` for the
    /// streaming widget.
    pub fn set_progress_handler(&self, handler: Arc<dyn ProgressHandler>) {
        if let Ok(mut guard) = self.progress.lock() {
            *guard = Some(handler);
        }
    }

    /// Send a `$/cancelRequest` notification for the in-flight outbound
    /// request id. The server is expected to abort cooperatively; we also
    /// flip the local cancel flag so any blocking read returns promptly.
    pub fn cancel_in_flight(&self, request_id: u64) -> PiResult<()> {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Self::notify(
            &self.inner,
            "$/cancelRequest",
            json!({"id": request_id}),
        )
    }

    /// Returns and clears the cancel flag. Used by call_tool to short-
    /// circuit before sending a new request.
    pub fn take_cancel(&self) -> bool {
        self.cancelled
            .swap(false, std::sync::atomic::Ordering::SeqCst)
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
        Self::rpc_request_full(inner, method, params, None, None, None, "")
    }

    /// Like `rpc_request` but with handlers for server-initiated requests
    /// (`sampling/createMessage`) and notifications (`notifications/progress`,
    /// `$/cancelRequest`). Pass `None` for handlers you don't want to wire.
    #[allow(clippy::too_many_arguments)]
    fn rpc_request_full(
        inner: &Arc<Mutex<ServerInner>>,
        method: &str,
        params: Value,
        sampling: Option<Arc<dyn SamplingHandler>>,
        progress: Option<Arc<dyn ProgressHandler>>,
        cancelled: Option<Arc<std::sync::atomic::AtomicBool>>,
        server_id: &str,
    ) -> PiResult<Value> {
        let mut guard = inner
            .lock()
            .map_err(|err| PiError::new(PiErrorKind::Tool, format!("MCP lock 失败：{err}")))?;
        // Detect a dead child eagerly so the user sees `MCP server 已退出`
        // instead of a generic write/flush error after the OS finally
        // surfaces a BrokenPipe.
        if let Ok(Some(status)) = guard.child.try_wait() {
            return Err(PiError::new(
                PiErrorKind::Tool,
                format!(
                    "MCP server 已退出（exit {}），无法执行 {method}",
                    status.code().unwrap_or(-1)
                ),
            ));
        }
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
            PiError::new(
                PiErrorKind::Tool,
                format!("写入 MCP stdin 失败（server 可能已退出）：{err}"),
            )
        })?;
        guard.stdin.flush().map_err(|err| {
            PiError::new(
                PiErrorKind::Tool,
                format!("MCP stdin flush 失败（server 可能已退出）：{err}"),
            )
        })?;
        loop {
            if let Some(flag) = &cancelled {
                if flag.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(PiError::new(
                        PiErrorKind::Cancelled,
                        format!("MCP {method} 已取消"),
                    ));
                }
            }
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
            let frame: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // 1. Server-initiated request (method + id present).
            if frame.get("method").is_some() && frame.get("id").is_some() {
                let req_id = frame.get("id").cloned().unwrap_or(Value::Null);
                let req_method = frame
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let req_params = frame.get("params").cloned().unwrap_or(Value::Null);
                let response_value = match req_method.as_str() {
                    "sampling/createMessage" => match &sampling {
                        Some(handler) => match handler.create_message(req_params) {
                            Ok(value) => json!({"jsonrpc": "2.0", "id": req_id, "result": value}),
                            Err(err) => json!({
                                "jsonrpc": "2.0",
                                "id": req_id,
                                "error": {"code": -32000, "message": err.message}
                            }),
                        },
                        None => json!({
                            "jsonrpc": "2.0",
                            "id": req_id,
                            "error": {"code": -32601, "message": "sampling 未启用"}
                        }),
                    },
                    other => json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "error": {"code": -32601, "message": format!("未实现的 server→client 方法：{other}")}
                    }),
                };
                let mut line = serde_json::to_string(&response_value)?;
                line.push('\n');
                guard.stdin.write_all(line.as_bytes()).map_err(|err| {
                    PiError::new(PiErrorKind::Tool, format!("写入 server-request 响应失败：{err}"))
                })?;
                guard.stdin.flush().map_err(|err| {
                    PiError::new(PiErrorKind::Tool, format!("flush 失败：{err}"))
                })?;
                continue;
            }
            // 2. Server-initiated notification (method, no id).
            if frame.get("method").is_some() && frame.get("id").is_none() {
                let notif_method = frame
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if notif_method == "notifications/progress" {
                    if let Some(handler) = &progress {
                        handler.on_progress(
                            server_id,
                            frame.get("params").cloned().unwrap_or(Value::Null),
                        );
                    }
                }
                // `$/cancelRequest` from server toward client is a request to
                // cancel an outbound id; since the outbound id != our id, we
                // just ignore (no second outbound is in flight here).
                continue;
            }
            // 3. Response (id present, no method) — match against ours.
            if let Some(resp_id) = frame.get("id").and_then(|v| v.as_u64()) {
                if resp_id == id {
                    if let Some(err) = frame.get("error") {
                        let message = err
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        return Err(PiError::new(
                            PiErrorKind::Tool,
                            format!("MCP {method} 失败：{message}"),
                        ));
                    }
                    return Ok(frame.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        }
    }

    fn notify(inner: &Arc<Mutex<ServerInner>>, method: &str, params: Value) -> PiResult<()> {
        let mut guard = inner
            .lock()
            .map_err(|err| PiError::new(PiErrorKind::Tool, format!("MCP lock 失败：{err}")))?;
        let mut frame = json!({"jsonrpc": "2.0", "method": format!("notifications/{method}")});
        if !params.is_null() {
            frame["params"] = params;
        }
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

    /// Drain any pending notifications without blocking. Returns the list of
    /// `{"method": ..., "params": ...}` notifications the server pushed
    /// asynchronously (e.g. `notifications/tools/list_changed`,
    /// `notifications/resources/updated`, `notifications/message`).
    pub fn drain_notifications(&self, timeout_ms: u64) -> PiResult<Vec<Notification>> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|err| PiError::new(PiErrorKind::Tool, format!("MCP lock 失败：{err}")))?;
        let mut out = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        // We rely on a small poll: ChildStdout doesn't expose set_nonblocking
        // portably, so we use BufReader::fill_buf with a timeout via the
        // child's stdout — best effort. For Linux/macOS this returns quickly
        // when there is no buffered data because we just peek the buffer.
        loop {
            if std::time::Instant::now() > deadline {
                break;
            }
            let available = match guard.reader.fill_buf() {
                Ok(buf) => buf.len(),
                Err(_) => 0,
            };
            if available == 0 {
                break;
            }
            let mut line = String::new();
            let n = guard
                .reader
                .read_line(&mut line)
                .map_err(|err| PiError::new(PiErrorKind::Tool, format!("读取 MCP 通知失败：{err}")))?;
            if n == 0 {
                break;
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Notifications never carry `id`; responses do.
            if value.get("id").is_some() {
                continue;
            }
            if let Some(method) = value.get("method").and_then(|v| v.as_str()) {
                out.push(Notification {
                    method: method.to_string(),
                    params: value.get("params").cloned().unwrap_or(Value::Null),
                });
            }
        }
        Ok(out)
    }

    /// Fetch a resource's contents via `resources/read`.
    pub fn read_resource(&self, uri: &str) -> PiResult<Value> {
        if !self.capabilities.resources {
            return Err(PiError::new(
                PiErrorKind::Tool,
                format!("MCP server {} 未声明 resources 能力", self.id),
            ));
        }
        self.rpc_with_handlers("resources/read", json!({"uri": uri}))
    }

    /// Materialize a prompt via `prompts/get`.
    pub fn get_prompt(&self, name: &str, arguments: Value) -> PiResult<Value> {
        if !self.capabilities.prompts {
            return Err(PiError::new(
                PiErrorKind::Tool,
                format!("MCP server {} 未声明 prompts 能力", self.id),
            ));
        }
        self.rpc_with_handlers(
            "prompts/get",
            json!({"name": name, "arguments": arguments}),
        )
    }

    /// Run an RPC with the host's current sampling / progress / cancel
    /// handlers wired in. Used by every post-spawn call so server-initiated
    /// requests routed back to the host actually reach a handler.
    fn rpc_with_handlers(&self, method: &str, params: Value) -> PiResult<Value> {
        let sampling = self.sampling.lock().ok().and_then(|g| g.clone());
        let progress = self.progress.lock().ok().and_then(|g| g.clone());
        Self::rpc_request_full(
            &self.inner,
            method,
            params,
            sampling,
            progress,
            Some(self.cancelled.clone()),
            &self.id,
        )
    }

    pub fn resources(&self) -> &[RemoteResource] {
        &self.resources
    }

    pub fn prompts(&self) -> &[RemotePrompt] {
        &self.prompts
    }

    pub fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }

    pub fn call_tool(&self, tool_name: &str, arguments: Value) -> PiResult<String> {
        let result = self.rpc_with_handlers(
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

#[derive(Debug, Clone)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

fn parse_capabilities(init: &Value) -> ServerCapabilities {
    let caps = init.pointer("/capabilities").cloned().unwrap_or(Value::Null);
    ServerCapabilities {
        tools: caps.get("tools").is_some(),
        resources: caps.get("resources").is_some(),
        prompts: caps.get("prompts").is_some(),
        logging: caps.get("logging").is_some(),
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

    pub fn server(&self, id: &str) -> Option<&Arc<McpServer>> {
        self.servers.get(id)
    }

    /// Collect every resource advertised by every server, prefixed with the
    /// server id so the caller can disambiguate when multiple servers expose
    /// resources with the same URI.
    pub fn all_resources(&self) -> Vec<(String, RemoteResource)> {
        let mut out = Vec::new();
        for (id, server) in &self.servers {
            for resource in server.resources() {
                out.push((id.clone(), resource.clone()));
            }
        }
        out
    }

    /// Same shape as `all_resources` but for prompt templates.
    pub fn all_prompts(&self) -> Vec<(String, RemotePrompt)> {
        let mut out = Vec::new();
        for (id, server) in &self.servers {
            for prompt in server.prompts() {
                out.push((id.clone(), prompt.clone()));
            }
        }
        out
    }

    /// Drain notifications from every server. Returns `(server_id, notification)`
    /// tuples so consumers know where each event originated.
    pub fn drain_notifications(&self, timeout_ms: u64) -> PiResult<Vec<(String, Notification)>> {
        let mut out = Vec::new();
        for (id, server) in &self.servers {
            for notification in server.drain_notifications(timeout_ms)? {
                out.push((id.clone(), notification));
            }
        }
        Ok(out)
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

    fn write_python_full_server(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("full_server.py");
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
        send({"jsonrpc":"2.0","id":req["id"],"result":{"capabilities":{"tools":{},"resources":{},"prompts":{}},"serverInfo":{"name":"full","version":"0.0.1"}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":req["id"],"result":{"tools":[{"name":"noop","description":"","inputSchema":{}}]}})
    elif method == "resources/list":
        send({"jsonrpc":"2.0","id":req["id"],"result":{"resources":[{"uri":"file:///hello.txt","name":"hello","mimeType":"text/plain"}]}})
    elif method == "resources/read":
        send({"jsonrpc":"2.0","id":req["id"],"result":{"contents":[{"uri":req["params"]["uri"],"text":"hi"}]}})
    elif method == "prompts/list":
        send({"jsonrpc":"2.0","id":req["id"],"result":{"prompts":[{"name":"summary","description":"summarize","arguments":[{"name":"topic","required":True}]}]}})
    elif method == "prompts/get":
        send({"jsonrpc":"2.0","id":req["id"],"result":{"description":"summarize","messages":[{"role":"user","content":{"type":"text","text":"hi " + req["params"]["arguments"].get("topic","")}}]}})
    elif method and method.startswith("notifications/"):
        pass
    else:
        send({"jsonrpc":"2.0","id":req.get("id"),"error":{"code":-32601,"message":"unknown"}})
"#,
        )
        .unwrap();
        path
    }

    #[test]
    fn manager_loads_resources_and_prompts_from_capable_server() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = tempdir().unwrap();
        let script = write_python_full_server(dir.path());
        let config = McpConfig {
            servers: vec![ServerSpec {
                id: "full".to_string(),
                command: "python3".to_string(),
                args: vec![script.display().to_string()],
                env: BTreeMap::new(),
            }],
        };
        let manager = McpManager::from_config(&config).expect("spawn");
        let resources = manager.all_resources();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].1.uri, "file:///hello.txt");
        let prompts = manager.all_prompts();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].1.name, "summary");
        let server = manager.server("full").expect("server");
        let read = server.read_resource("file:///hello.txt").expect("read");
        assert!(
            read.pointer("/contents/0/text").and_then(|v| v.as_str()) == Some("hi"),
            "expected resource text hi, got {read}"
        );
        let prompt = server
            .get_prompt("summary", json!({"topic": "rust"}))
            .expect("prompt");
        let text = prompt
            .pointer("/messages/0/content/text")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(text.contains("rust"));
    }

    fn write_python_sampling_server(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("sampling_server.py");
        fs::write(
            &path,
            r#"
import sys
import json

def send(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

state = {"sampled": None}
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":req["id"],"result":{"capabilities":{"tools":{},"sampling":{}},"serverInfo":{"name":"s","version":"0.0.1"}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":req["id"],"result":{"tools":[{"name":"ask","description":"ask host","inputSchema":{}}]}})
    elif method == "tools/call":
        # Server-initiated sampling request first, then return the result.
        send({"jsonrpc":"2.0","id":1000,"method":"sampling/createMessage","params":{"messages":[{"role":"user","content":{"type":"text","text":"summarize"}}]}})
        # Wait for the response from the host on next line.
        # The host posts back via its stdin (which is our stdin too).
        reply_line = sys.stdin.readline().strip()
        reply = json.loads(reply_line)
        state["sampled"] = reply.get("result")
        send({"jsonrpc":"2.0","id":req["id"],"result":{"content":[{"type":"text","text":"got:" + json.dumps(state["sampled"])}]}})
    elif method == "$/cancelRequest":
        pass
    elif method and method.startswith("notifications/"):
        pass
    else:
        send({"jsonrpc":"2.0","id":req.get("id"),"error":{"code":-32601,"message":"unknown"}})
"#,
        )
        .unwrap();
        path
    }

    struct EchoSampler;
    impl SamplingHandler for EchoSampler {
        fn create_message(&self, _params: Value) -> PiResult<Value> {
            Ok(json!({
                "role": "assistant",
                "content": {"type": "text", "text": "from-host"}
            }))
        }
    }

    #[test]
    fn sampling_request_round_trips_to_host_handler() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = tempdir().unwrap();
        let script = write_python_sampling_server(dir.path());
        let config = McpConfig {
            servers: vec![ServerSpec {
                id: "s".to_string(),
                command: "python3".to_string(),
                args: vec![script.display().to_string()],
                env: BTreeMap::new(),
            }],
        };
        let manager = McpManager::from_config(&config).expect("spawn");
        let server = manager.server("s").expect("server").clone();
        server.set_sampling_handler(Arc::new(EchoSampler));
        let output = server
            .call_tool("ask", json!({}))
            .expect("call");
        assert!(output.contains("from-host"), "got: {output}");
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
