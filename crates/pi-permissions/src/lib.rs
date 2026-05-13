use pi_core::{PiError, PiErrorKind, PiResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    ReadFile,
    WriteFile,
    ExecuteCommand,
    Network,
    Session,
    ExtensionHostcall,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::WriteFile => "write_file",
            Self::ExecuteCommand => "execute_command",
            Self::Network => "network",
            Self::Session => "session",
            Self::ExtensionHostcall => "extension_hostcall",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub capability: Capability,
    pub target: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    ReadOnly,
    ConfirmMutations,
    TrustedWorkspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDecision {
    pub allowed: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    pub capability: Capability,
    pub target: String,
    pub allowed: bool,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct PermissionEngine {
    mode: PermissionMode,
    sandbox: SandboxProfile,
    audit: Vec<AuditEvent>,
}

impl PermissionEngine {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            sandbox: SandboxProfile::default(),
            audit: Vec::new(),
        }
    }

    pub fn with_sandbox(mut self, sandbox: SandboxProfile) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub fn decide(&mut self, request: PermissionRequest) -> PermissionDecision {
        let allowed = self.sandbox.allows(&request)
            && match self.mode {
            PermissionMode::ReadOnly => matches!(
                request.capability,
                Capability::ReadFile | Capability::Session
            ),
            PermissionMode::ConfirmMutations => !is_dangerous_target(&request.target),
            PermissionMode::TrustedWorkspace => true,
        };

        let reason = if allowed {
            "已允许：符合当前权限策略".to_string()
        } else {
            "已拒绝：目标被权限策略拦截".to_string()
        };

        self.audit.push(AuditEvent {
            capability: request.capability,
            target: request.target,
            allowed,
            reason: reason.clone(),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProfile {
    pub workspace_root: Option<String>,
    pub extra_read_roots: Vec<String>,
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
        if request.capability == Capability::Network && !self.allow_network {
            return false;
        }

        if let Some(root) = &self.workspace_root {
            if matches!(request.capability, Capability::ReadFile | Capability::WriteFile) {
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

fn is_dangerous_target(target: &str) -> bool {
    let lowered = target.to_ascii_lowercase();
    lowered.contains("rm -rf /")
        || lowered.contains("mkfs")
        || lowered.contains("/dev/")
        || lowered.contains("shutdown")
        || lowered.contains("reboot")
}
