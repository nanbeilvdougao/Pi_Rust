//! Capability-based permission engine.
//!
//! Every tool / extension hostcall declares the capability it needs. The
//! `PermissionEngine` decides whether to allow the call based on:
//!
//! 1. **Mode** — one of `ReadOnly`, `ConfirmMutations`, `TrustedWorkspace`,
//!    `Plan`. Modes are independent of the sandbox so a workspace can be
//!    "trusted but read-only" if both are configured that way.
//! 2. **Sandbox profile** — narrow allowlist of filesystem roots and a
//!    network toggle. The default profile denies network and accepts any
//!    file path; production callers will typically set `workspace_root` to
//!    keep file ops inside the cwd.
//! 3. **Dangerous target list** — pattern blocklist (e.g. `rm -rf /`).
//! 4. **Audit log** — every decision is appended; UIs can render this.

use pi_core::{PiError, PiErrorKind, PiResult};
use serde::{Deserialize, Serialize};

pub mod landlock_apply;
pub mod sandbox;
pub use landlock_apply::{landlock_supported, restrict_self, LandlockOutcome, LandlockPlan};
pub use sandbox::{apply_sandbox, detect_backend as detect_sandbox_backend, SandboxBackend};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    ReadFile,
    WriteFile,
    DeleteFile,
    ChangeMode,
    ExecuteCommand,
    Network,
    BindSocket,
    Session,
    ExtensionHostcall,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::WriteFile => "write_file",
            Self::DeleteFile => "delete_file",
            Self::ChangeMode => "change_mode",
            Self::ExecuteCommand => "execute_command",
            Self::Network => "network",
            Self::BindSocket => "bind_socket",
            Self::Session => "session",
            Self::ExtensionHostcall => "extension_hostcall",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        Some(match value {
            "read_file" => Self::ReadFile,
            "write_file" => Self::WriteFile,
            "delete_file" => Self::DeleteFile,
            "change_mode" => Self::ChangeMode,
            "execute_command" => Self::ExecuteCommand,
            "network" => Self::Network,
            "bind_socket" => Self::BindSocket,
            "session" => Self::Session,
            "extension_hostcall" => Self::ExtensionHostcall,
            _ => return None,
        })
    }

    /// Whether the capability mutates state. Used by the engine's mode rules
    /// to decide whether ReadOnly/Plan should reject by default.
    pub fn is_mutating(self) -> bool {
        matches!(
            self,
            Self::WriteFile
                | Self::DeleteFile
                | Self::ChangeMode
                | Self::ExecuteCommand
                | Self::Network
                | Self::BindSocket
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub capability: Capability,
    pub target: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    ReadOnly,
    ConfirmMutations,
    TrustedWorkspace,
    Plan,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDecision {
    pub allowed: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub capability: Capability,
    pub target: String,
    pub allowed: bool,
    pub reason: String,
    pub timestamp_ms: u128,
}

#[derive(Debug, Clone)]
pub struct PermissionEngine {
    mode: PermissionMode,
    sandbox: SandboxProfile,
    audit: Vec<AuditEvent>,
    blocklist: Vec<String>,
}

impl PermissionEngine {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            sandbox: SandboxProfile::default(),
            audit: Vec::new(),
            blocklist: default_blocklist(),
        }
    }

    pub fn with_sandbox(mut self, sandbox: SandboxProfile) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn with_blocklist(mut self, blocklist: Vec<String>) -> Self {
        self.blocklist = blocklist;
        self
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    pub fn decide(&mut self, request: PermissionRequest) -> PermissionDecision {
        let sandbox_ok = self.sandbox.allows(&request);
        let mode_ok = match self.mode {
            PermissionMode::ReadOnly => matches!(
                request.capability,
                Capability::ReadFile | Capability::Session
            ),
            PermissionMode::ConfirmMutations => {
                !is_dangerous_target(&self.blocklist, &request.target)
            }
            PermissionMode::TrustedWorkspace => true,
            PermissionMode::Plan => matches!(
                request.capability,
                Capability::ReadFile | Capability::Session | Capability::ExtensionHostcall
            ),
        };
        let allowed = sandbox_ok && mode_ok;

        let reason = if allowed {
            "已允许：符合当前权限策略".to_string()
        } else if !sandbox_ok {
            "已拒绝：目标超出 sandbox 允许范围".to_string()
        } else {
            match self.mode {
                PermissionMode::ReadOnly => "已拒绝：read-only 模式禁止此能力".to_string(),
                PermissionMode::Plan => "已拒绝：plan 模式禁止此能力".to_string(),
                _ => "已拒绝：目标在危险命令黑名单中".to_string(),
            }
        };

        self.audit.push(AuditEvent {
            capability: request.capability,
            target: request.target,
            allowed,
            reason: reason.clone(),
            timestamp_ms: pi_core::now_ms(),
        });

        PermissionDecision { allowed, reason }
    }

    pub fn require(&mut self, request: PermissionRequest) -> PiResult<()> {
        let capability = request.capability;
        let target = request.target.clone();
        let decision = self.decide(request);
        if decision.allowed {
            Ok(())
        } else {
            Err(PiError::new(
                PiErrorKind::PermissionDenied,
                format!(
                    "权限拒绝：{} 不允许访问 {}。{}",
                    capability.as_str(),
                    target,
                    decision.reason
                ),
            ))
        }
    }

    pub fn audit_log(&self) -> &[AuditEvent] {
        &self.audit
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxProfile {
    #[serde(default)]
    pub workspace_root: Option<String>,
    #[serde(default)]
    pub extra_read_roots: Vec<String>,
    #[serde(default)]
    pub allow_network: bool,
}

impl Default for SandboxProfile {
    fn default() -> Self {
        Self {
            workspace_root: None,
            extra_read_roots: Vec::new(),
            allow_network: false,
        }
    }
}

impl SandboxProfile {
    pub fn allows(&self, request: &PermissionRequest) -> bool {
        if matches!(
            request.capability,
            Capability::Network | Capability::BindSocket
        ) && !self.allow_network
        {
            return false;
        }

        if let Some(root) = &self.workspace_root {
            if matches!(
                request.capability,
                Capability::ReadFile
                    | Capability::WriteFile
                    | Capability::DeleteFile
                    | Capability::ChangeMode
            ) {
                return request.target.starts_with(root)
                    || self
                        .extra_read_roots
                        .iter()
                        .any(|allowed| request.target.starts_with(allowed));
            }
        }

        true
    }
}

fn default_blocklist() -> Vec<String> {
    vec![
        "rm -rf /".to_string(),
        "rm -rf /*".to_string(),
        "mkfs".to_string(),
        "shutdown".to_string(),
        "reboot".to_string(),
        ":(){:|:&};:".to_string(),
        "/dev/sda".to_string(),
        "/dev/zero".to_string(),
        "dd if=".to_string(),
    ]
}

fn is_dangerous_target(blocklist: &[String], target: &str) -> bool {
    let lowered = target.to_ascii_lowercase();
    blocklist
        .iter()
        .any(|pattern| lowered.contains(&pattern.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_mode_blocks_dangerous_targets() {
        let mut engine = PermissionEngine::new(PermissionMode::ConfirmMutations);
        let request = PermissionRequest {
            capability: Capability::ExecuteCommand,
            target: "rm -rf /".to_string(),
            reason: "test".to_string(),
        };
        let decision = engine.decide(request);
        assert!(!decision.allowed);
    }

    #[test]
    fn read_only_mode_blocks_writes() {
        let mut engine = PermissionEngine::new(PermissionMode::ReadOnly);
        let request = PermissionRequest {
            capability: Capability::WriteFile,
            target: "/tmp/x".to_string(),
            reason: "test".to_string(),
        };
        let decision = engine.decide(request);
        assert!(!decision.allowed);
    }

    #[test]
    fn plan_mode_blocks_execute() {
        let mut engine = PermissionEngine::new(PermissionMode::Plan);
        let request = PermissionRequest {
            capability: Capability::ExecuteCommand,
            target: "ls".to_string(),
            reason: "test".to_string(),
        };
        let decision = engine.decide(request);
        assert!(!decision.allowed);
    }

    #[test]
    fn trusted_mode_allows_everything_inside_sandbox() {
        let mut engine =
            PermissionEngine::new(PermissionMode::TrustedWorkspace).with_sandbox(SandboxProfile {
                workspace_root: Some("/workspace".to_string()),
                extra_read_roots: Vec::new(),
                allow_network: false,
            });
        let outside = engine.decide(PermissionRequest {
            capability: Capability::WriteFile,
            target: "/etc/passwd".to_string(),
            reason: "x".to_string(),
        });
        assert!(!outside.allowed);
        let inside = engine.decide(PermissionRequest {
            capability: Capability::WriteFile,
            target: "/workspace/file".to_string(),
            reason: "x".to_string(),
        });
        assert!(inside.allowed);
    }
}
