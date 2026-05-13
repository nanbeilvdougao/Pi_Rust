use pi_permissions::{
    Capability, PermissionEngine, PermissionMode, PermissionRequest, SandboxProfile,
};

#[test]
fn sandbox_limits_file_access_to_workspace() {
    let sandbox = SandboxProfile {
        workspace_root: Some("/workspace/project".to_string()),
        extra_read_roots: vec!["/workspace/shared".to_string()],
        allow_network: false,
    };
    let mut engine = PermissionEngine::new(PermissionMode::ConfirmMutations).with_sandbox(sandbox);

    let allowed = engine.decide(PermissionRequest {
        capability: Capability::ReadFile,
        target: "/workspace/project/src/main.rs".to_string(),
        reason: "read project file".to_string(),
    });
    assert!(allowed.allowed);

    let denied = engine.decide(PermissionRequest {
        capability: Capability::ReadFile,
        target: "/etc/passwd".to_string(),
        reason: "read outside workspace".to_string(),
    });
    assert!(!denied.allowed);
}
