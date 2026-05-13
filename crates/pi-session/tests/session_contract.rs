use std::fs;

use pi_core::{Message, Role};
use pi_session::{JsonlSessionStore, SessionStore};

#[test]
fn jsonl_store_appends_and_loads_messages() {
    let root = std::env::temp_dir().join(format!("pi-rust-session-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let store = JsonlSessionStore::new(&root);

    store
        .append("contract", &Message::new(Role::User, "你好"))
        .expect("append user");
    store
        .append("contract", &Message::new(Role::Assistant, "收到"))
        .expect("append assistant");

    let loaded = store.load("contract").expect("load session");
    assert_eq!(loaded.messages.len(), 2);
    assert_eq!(loaded.messages[0].role, Role::User);
    assert_eq!(loaded.messages[1].role, Role::Assistant);

    let sessions = store.list().expect("list sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "contract");
    assert_eq!(sessions[0].message_count, 2);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn jsonl_store_manages_session_lifecycle() {
    let root = std::env::temp_dir().join(format!(
        "pi-rust-session-lifecycle-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    let store = JsonlSessionStore::new(&root);

    let message = Message::tool_result(Some("call_1".to_string()), "工具输出");
    store.append("old", &message).expect("append tool result");

    let exported = store.export_markdown("old").expect("export markdown");
    assert!(exported.contains("# Pi Session: old"));
    assert!(exported.contains("call_1"));
    assert!(exported.contains("工具输出"));

    store.rename("old", "new").expect("rename session");
    assert!(store.load("old").expect("load old").messages.is_empty());
    assert_eq!(store.load("new").expect("load new").messages.len(), 1);

    assert!(store.delete("new").expect("delete session"));
    assert!(!store.delete("new").expect("delete missing session"));

    let _ = fs::remove_dir_all(root);
}
