//! Verifies that the agent uses a registered `FauxProvider` end-to-end, so
//! tests can drive the agent loop without hitting a real LLM. Mirrors the TS
//! `coding-agent` harness's faux-provider tests.

use std::fs;

use pi_agent::AgentRuntime;
use pi_core::{AppConfig, ModelSelection, Role};
use pi_providers::{register_test_provider, FauxProvider, FauxTurn};
use pi_session::{JsonlSessionStore, SessionStore};

#[test]
fn agent_picks_up_registered_faux_provider() {
    let root = std::env::temp_dir().join(format!("pi-rust-faux-harness-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    let faux = FauxProvider::with_script([FauxTurn::Text("已收到".into())]);
    let _guard = register_test_provider("faux-test", faux.clone());

    let config = AppConfig {
        model: ModelSelection {
            provider: "faux-test".to_string(),
            model: "faux".to_string(),
        },
        tools_enabled: false,
        ..AppConfig::default()
    };
    let store = JsonlSessionStore::new(&root);
    let mut agent = AgentRuntime::try_new(config, store.clone()).expect("agent");

    let turn = agent
        .run_single_turn("harness", "你好")
        .expect("run faux turn");

    let assistant_msg = turn
        .session
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("assistant message");
    assert_eq!(assistant_msg.content, "已收到");

    let recorded = faux.requests();
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0]
        .messages
        .iter()
        .any(|m| m.content.contains("你好")));

    let loaded = store.load("harness").expect("load");
    assert!(loaded
        .messages
        .iter()
        .any(|m| m.role == Role::Assistant && m.content == "已收到"));

    let _ = fs::remove_dir_all(root);
}
