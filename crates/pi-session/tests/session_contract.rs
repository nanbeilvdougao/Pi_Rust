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

    let _ = fs::remove_dir_all(root);
}
