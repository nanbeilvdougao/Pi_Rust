use std::fs;

use pi_agent::AgentRuntime;
use pi_core::{AppConfig, Event, ModelSelection, Role};
use pi_session::{JsonlSessionStore, SessionStore};

#[test]
fn provider_tool_call_runs_tool_and_returns_to_provider() {
    let root = std::env::temp_dir().join(format!(
        "pi-rust-agent-tool-loop-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);

    let config = AppConfig {
        model: ModelSelection {
            provider: "echo".to_string(),
            model: "echo-local".to_string(),
        },
        tools_enabled: true,
        enabled_tool_names: Some(vec!["ls".to_string()]),
        ..AppConfig::default()
    };
    let store = JsonlSessionStore::new(&root);
    let mut agent = AgentRuntime::try_new(config, store.clone()).expect("create runtime");

    let turn = agent
        .run_single_turn("contract", "CALL_TOOL ls .")
        .expect("run provider-driven tool turn");

    assert!(turn
        .events
        .iter()
        .any(|event| matches!(event, Event::ToolStarted { name } if name == "ls")));
    assert!(turn
        .events
        .iter()
        .any(|event| matches!(event, Event::ToolFinished { name, .. } if name == "ls")));
    assert!(turn
        .events
        .iter()
        .any(|event| matches!(event, Event::ProviderStream(_))));
    let saw_final = turn.events.iter().any(|event| {
        matches!(event, Event::AssistantMessage(message) if message.contains("工具结果已返回"))
    });
    assert!(saw_final);

    let loaded = store.load("contract").expect("load persisted session");
    assert_eq!(loaded.messages.len(), 3);
    assert_eq!(loaded.messages[0].role, Role::User);
    assert_eq!(loaded.messages[0].content, "CALL_TOOL ls .");
    assert_eq!(loaded.messages[1].role, Role::Tool);
    assert_eq!(loaded.messages[1].tool_call_id.as_deref(), Some("echo-ls"));
    assert_eq!(loaded.messages[2].role, Role::Assistant);

    let _ = fs::remove_dir_all(root);
}
