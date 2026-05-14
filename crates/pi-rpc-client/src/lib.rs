//! Embedding SDK for the line-delimited JSON-RPC protocol that
//! `pi --rpc` (and `pi --rpc-stdio`) speak.
//!
//! Usage:
//!
//! ```no_run
//! use pi_rpc_client::{PiRpcClient, RpcConfig};
//! let mut client = PiRpcClient::spawn(RpcConfig::default()).expect("spawn");
//! let response = client.complete("你好", None).expect("complete");
//! println!("{} events", response.events.len());
//! client.shutdown().ok();
//! ```
//!
//! Why a separate crate (rather than just calling `pi-cli::rpc::run` in
//! process)?
//! - **Boundary**: SDK users do not need to depend on the whole CLI / agent /
//!   provider tree. `pi-rpc-client` only needs `pi-core` for the wire types.
//! - **Process model**: each rpc consumer spawns its own `pi` subprocess.
//!   Crashes / OOMs are contained; ergonomics match the TS SDK exactly.
//! - **Reuse**: tooling that wants to script Pi from another binary can pull
//!   this crate and skip writing their own JSON-RPC plumbing.
//!
//! Parity target: `packages/agent/src/sdk/rpc-client.ts`.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use pi_core::{Event, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Configuration for spawning the `pi --rpc-stdio` subprocess.
#[derive(Debug, Clone)]
pub struct RpcConfig {
    /// Path or command name for the pi binary. Falls back to `pi` on PATH.
    pub binary: PathBuf,
    /// Extra arguments to prepend before `--rpc-stdio` (e.g. `--config foo.toml`).
    pub extra_args: Vec<String>,
    /// Working directory for the subprocess. None inherits the parent.
    pub working_dir: Option<PathBuf>,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            binary: PathBuf::from("pi"),
            extra_args: Vec::new(),
            working_dir: None,
        }
    }
}

/// The high-level client. Holds the child process handle and request id
/// counter; drops the child on `Drop` for cleanup.
pub struct PiRpcClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: AtomicU64,
}

#[derive(Debug, Serialize)]
struct RpcRequest<'a> {
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Value::is_null")]
    params: Value,
}

#[derive(Debug, Deserialize)]
struct RpcResponse {
    #[allow(dead_code)]
    id: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<String>,
}

/// Successful `complete` payload.
#[derive(Debug, Clone, Deserialize)]
pub struct CompleteResponse {
    #[serde(default)]
    pub events: Vec<Event>,
    #[serde(default)]
    pub session: String,
    #[serde(default)]
    pub usage: Usage,
}

/// Provider metadata returned by `list_providers`.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub display_name: String,
    pub default_model: String,
    #[serde(default)]
    pub supported_models: Vec<String>,
    #[serde(default)]
    pub local_first: bool,
    #[serde(default)]
    pub requires_api_key_env: Option<String>,
}

/// Tool metadata returned by `list_tools`. Mirrors `pi_core::ToolSchema`
/// without depending on `pi-core` consumers needing the full crate.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub input_shape: String,
    #[serde(default)]
    pub parameters: Option<Value>,
    #[serde(default)]
    pub mutates: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AliasInfo {
    pub alias: String,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    #[serde(default)]
    pub message_count: usize,
    #[serde(default)]
    pub updated_ms: u128,
    #[serde(default)]
    pub last_user_excerpt: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("spawn pi subprocess failed: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("rpc returned error: {0}")]
    Server(String),
    #[error("rpc transport closed unexpectedly")]
    Closed,
    #[error("rpc response was not valid json: {0}")]
    InvalidJson(#[from] serde_json::Error),
    #[error("rpc response missing result")]
    MissingResult,
}

impl PiRpcClient {
    /// Spawn `pi --rpc-stdio` and connect stdin/stdout pipes.
    pub fn spawn(config: RpcConfig) -> Result<Self, RpcError> {
        let mut cmd = Command::new(&config.binary);
        for arg in &config.extra_args {
            cmd.arg(arg);
        }
        cmd.arg("--rpc-stdio");
        if let Some(dir) = &config.working_dir {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().ok_or(RpcError::Closed)?;
        let stdout = child.stdout.take().ok_or(RpcError::Closed)?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: AtomicU64::new(1),
        })
    }

    /// Build a client from already-open stdio handles. Useful for in-process
    /// tests against the server side without spawning a real `pi`.
    pub fn from_raw(child: Child, stdin: ChildStdin, stdout: ChildStdout) -> Self {
        Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: AtomicU64::new(1),
        }
    }

    /// `health` — returns server version string.
    pub fn health(&mut self) -> Result<String, RpcError> {
        let value = self.call("health", Value::Null)?;
        Ok(value
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    /// `list_providers` — built-in provider registry snapshot.
    pub fn list_providers(&mut self) -> Result<Vec<ProviderInfo>, RpcError> {
        let value = self.call("list_providers", Value::Null)?;
        Ok(serde_json::from_value(value)?)
    }

    /// `list_tools` — schemas for every built-in tool.
    pub fn list_tools(&mut self) -> Result<Vec<ToolInfo>, RpcError> {
        let value = self.call("list_tools", Value::Null)?;
        Ok(serde_json::from_value(value)?)
    }

    /// `complete` — single turn. Returns the full event vector, plus the
    /// session id assigned by the server (e.g. for follow-up calls).
    pub fn complete(
        &mut self,
        prompt: &str,
        session_id: Option<&str>,
    ) -> Result<CompleteResponse, RpcError> {
        let mut params = serde_json::json!({"prompt": prompt});
        if let Some(id) = session_id {
            params["session_id"] = Value::String(id.to_string());
        }
        let value = self.call("complete", params)?;
        Ok(serde_json::from_value(value)?)
    }

    /// `list_models` — provider/model pairs, optionally filtered by provider.
    pub fn list_models(&mut self, provider: Option<&str>) -> Result<Vec<ModelInfo>, RpcError> {
        let params = match provider {
            Some(p) => serde_json::json!({"provider": p}),
            None => Value::Null,
        };
        let value = self.call("list_models", params)?;
        Ok(serde_json::from_value(value)?)
    }

    /// `list_aliases` — global alias table (alias → provider/model).
    pub fn list_aliases(&mut self) -> Result<Vec<AliasInfo>, RpcError> {
        let value = self.call("list_aliases", Value::Null)?;
        Ok(serde_json::from_value(value)?)
    }

    /// `list_sessions` — every session in the active session store.
    pub fn list_sessions(&mut self) -> Result<Vec<SessionSummary>, RpcError> {
        let value = self.call("list_sessions", Value::Null)?;
        Ok(serde_json::from_value(value)?)
    }

    /// `get_config` — server's current `AppConfig` snapshot.
    pub fn get_config(&mut self) -> Result<Value, RpcError> {
        self.call("get_config", Value::Null)
    }

    /// `cancel` — flip the agent's cancel flag. Returns `{"cancelled": true}`.
    pub fn cancel(&mut self) -> Result<Value, RpcError> {
        self.call("cancel", Value::Null)
    }

    /// Politely ask the server to exit. The server replies, then closes the
    /// pipe; subsequent calls will fail with `RpcError::Closed`.
    pub fn shutdown(mut self) -> Result<(), RpcError> {
        let _ = self.call("shutdown", Value::Null);
        // The server breaks the loop right after acking shutdown; reap it.
        let _ = self.child.wait();
        Ok(())
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = RpcRequest {
            id,
            method,
            params,
        };
        let line = serde_json::to_string(&request)?;
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        let mut buf = String::new();
        let read = self.stdout.read_line(&mut buf)?;
        if read == 0 {
            return Err(RpcError::Closed);
        }
        let response: RpcResponse = serde_json::from_str(buf.trim())?;
        if let Some(err) = response.error {
            return Err(RpcError::Server(err));
        }
        response.result.ok_or(RpcError::MissingResult)
    }
}

impl Drop for PiRpcClient {
    fn drop(&mut self) {
        // Best-effort cleanup. If the user already called shutdown we'll get
        // an error from kill() which we ignore.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    fn ok<T, E: std::fmt::Debug>(value: Result<T, E>) -> T {
        match value {
            Ok(v) => v,
            Err(err) => panic!("expected Ok, got Err: {err:?}"),
        }
    }

    fn some<T>(value: Option<T>) -> T {
        match value {
            Some(v) => v,
            None => panic!("expected Some, got None"),
        }
    }

    /// Minimal in-process RPC emulator: we feed a request line to a buffer
    /// the way PiRpcClient would, then parse the response from a buffer
    /// containing what a `pi-cli::rpc::run` server would have produced.
    /// This validates the framing and parsing logic without spawning.
    #[test]
    fn parses_complete_response_shape() {
        let response = serde_json::json!({
            "id": 1,
            "result": {
                "events": [{"UserMessage": "hi"}, {"AssistantMessage": "ok"}],
                "session": "default",
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }
        });
        let line = format!("{}\n", ok(serde_json::to_string(&response)));
        let rpc_resp: RpcResponse = ok(serde_json::from_str(line.trim()));
        assert!(rpc_resp.error.is_none());
        let payload: CompleteResponse = ok(serde_json::from_value(some(rpc_resp.result)));
        assert_eq!(payload.events.len(), 2);
        assert_eq!(payload.session, "default");
        assert_eq!(payload.usage.total_tokens, 2);
    }

    #[test]
    fn parses_error_payload() {
        let response = serde_json::json!({"id": 1, "error": "boom"});
        let rpc_resp: RpcResponse = ok(serde_json::from_str(&response.to_string()));
        assert_eq!(rpc_resp.error.as_deref(), Some("boom"));
    }

    #[test]
    fn list_providers_round_trips_through_value() {
        let raw = serde_json::json!([
            {"id":"echo","display_name":"Echo","default_model":"echo-local",
             "supported_models":["echo-local"],"local_first":true,"requires_api_key_env":null}
        ]);
        let providers: Vec<ProviderInfo> = ok(serde_json::from_value(raw));
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "echo");
    }

    /// Smoke test for the request-side framing: write a request to a cursor
    /// and check it ends in `\n`.
    #[test]
    fn request_framing_appends_newline() {
        let req = RpcRequest {
            id: 1,
            method: "health",
            params: Value::Null,
        };
        let mut buf = Vec::new();
        let line = ok(serde_json::to_string(&req));
        ok(buf.write_all(line.as_bytes()));
        ok(buf.write_all(b"\n"));
        assert!(buf.ends_with(b"\n"));
        let text = ok(String::from_utf8(buf));
        assert!(text.contains("\"method\":\"health\""));
    }
}
