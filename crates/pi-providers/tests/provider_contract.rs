use pi_providers::ProviderRegistry;

#[test]
fn registry_prioritizes_local_and_chinese_providers() {
    let registry = ProviderRegistry::builtin();
    let ids = registry
        .list()
        .map(|provider| provider.id.as_str())
        .collect::<Vec<_>>();

    for id in ["echo", "ollama", "moonshot", "deepseek", "qwen"] {
        assert!(ids.contains(&id), "missing provider {id}");
    }

    assert!(registry.get("ollama").expect("ollama").local_first);
    assert!(registry
        .get("ollama")
        .expect("ollama")
        .supported_models
        .contains(&"qwen2.5:7b".to_string()));
    assert!(registry
        .get("moonshot")
        .expect("moonshot")
        .requires_api_key_env
        .is_some());
}
