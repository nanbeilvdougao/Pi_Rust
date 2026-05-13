use pi_permissions::{PermissionEngine, PermissionMode};
use pi_tools::{ToolCall, ToolRuntime};

#[test]
fn builtin_tool_schemas_are_stable() {
    let runtime = ToolRuntime::builtin();
    let names = runtime
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect::<Vec<_>>();

    for required in ["bash", "edit", "epkg", "ls", "read", "search", "write"] {
        assert!(names.contains(&required.to_string()), "missing {required}");
    }
}

#[test]
fn dangerous_bash_is_blocked_by_default() {
    let runtime = ToolRuntime::builtin();
    let mut permissions = PermissionEngine::new(PermissionMode::ConfirmMutations);
    let result = runtime.run(
        ToolCall {
            name: "bash".to_string(),
            input: "rm -rf /".to_string(),
        },
        &mut permissions,
    );

    assert!(result.is_err());
    assert_eq!(permissions.audit_log().len(), 1);
    assert!(!permissions.audit_log()[0].allowed);
}

#[test]
fn builtin_tool_selection_rejects_unknown_names() {
    let selected = vec!["read".to_string(), "missing".to_string()];
    let result = ToolRuntime::builtin_with_names(&selected);
    assert!(result.is_err());
}
