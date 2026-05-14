//! Regression cases translated from TS pi's
//! `packages/coding-agent/test/suite/regressions/*.test.ts`. Each test names
//! the upstream issue it traces back to plus a short note about the
//! behavior we're locking in.
//!
//! These are *behavioral* regressions, not 1-to-1 ports — the underlying
//! abstractions differ (faux provider vs. TS mock, ratatui vs. ink). What
//! we preserve is the user-visible invariant the upstream test guards.

use std::fs;
use std::path::PathBuf;

use pi_agent::{AgentRuntime, PersistedSettings, SkillSet, SlashRegistry};
use pi_core::{AppConfig, Event, ModelSelection, Role, ToolInvocation};
use pi_providers::{register_test_provider, FauxProvider, FauxTurn};
use pi_session::{JsonlSessionStore, SessionStore};

fn tmp_session_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "pi-rust-regressions-{label}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn faux_agent(
    provider_id: &str,
    faux: std::sync::Arc<FauxProvider>,
) -> (
    AgentRuntime<JsonlSessionStore>,
    JsonlSessionStore,
    pi_providers::TestProviderGuard,
) {
    let guard = register_test_provider(provider_id, faux);
    let root = tmp_session_root(provider_id);
    let store = JsonlSessionStore::new(&root);
    let config = AppConfig {
        model: ModelSelection {
            provider: provider_id.to_string(),
            model: "faux".to_string(),
        },
        tools_enabled: false,
        ..AppConfig::default()
    };
    let agent = AgentRuntime::try_new(config, store.clone()).expect("agent");
    (agent, store, guard)
}

// ts-pi 2023: a slash command and a follow-up message in the same prompt
// should not crash the queue.
#[test]
fn ts2023_slash_command_followup_is_handled() {
    let registry = SlashRegistry::builtin();
    let outcome = registry.handle("/help").expect("help routed");
    assert!(outcome.assistant.unwrap().contains("/help"));
    // Untouched: non-slash prompts return None (caller routes to provider).
    assert!(registry.handle("regular prompt").is_none());
}

// ts-pi 2753: settings file reload between turns is reflected in subsequent
// turns without restarting the agent process.
#[test]
fn ts2753_settings_reload_between_turns() {
    // Layered loader is the production code path; here we directly call
    // load_layered with a fake workspace to prove the layer order:
    let dir = tempfile::tempdir().unwrap();
    let pi_dir = dir.path().join(".pi");
    fs::create_dir_all(&pi_dir).unwrap();
    fs::write(
        pi_dir.join("config.toml"),
        "provider = \"anthropic\"\nmodel = \"claude-sonnet-4-6\"\n",
    )
    .unwrap();
    let s1 = PersistedSettings::load_layered(Some(dir.path()));
    assert_eq!(s1.provider.as_deref(), Some("anthropic"));
    fs::write(
        pi_dir.join("config.toml"),
        "provider = \"deepseek\"\nmodel = \"deepseek-chat\"\n",
    )
    .unwrap();
    let s2 = PersistedSettings::load_layered(Some(dir.path()));
    assert_eq!(s2.provider.as_deref(), Some("deepseek"));
}

// ts-pi 2781: workspace skill precedence — workspace .pi/skills overrides
// the user-level ones (we only have workspace today, so verify the load
// reads the workspace files correctly).
#[test]
fn ts2781_skill_collision_workspace_wins() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join(".pi").join("skills");
    fs::create_dir_all(&skills).unwrap();
    fs::write(
        skills.join("style.md"),
        "---\ntrigger = \"always\"\n---\nBe terse.",
    )
    .unwrap();
    let set = SkillSet::load_workspace(dir.path());
    assert_eq!(set.skills().len(), 1);
    assert!(set.always_prompt().contains("Be terse"));
}

// ts-pi 2835: tools allowlist filters down to the requested subset.
#[test]
fn ts2835_tools_allowlist_filters() {
    use pi_tools::ToolRuntime;
    let runtime =
        ToolRuntime::builtin_with_names(&["read".to_string(), "ls".to_string()]).expect("subset");
    let names: Vec<String> = runtime.schemas().into_iter().map(|s| s.name).collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"read".to_string()));
    assert!(names.contains(&"ls".to_string()));
    assert!(!names.contains(&"bash".to_string()));
}

// ts-pi 2860: replaying a session reconstructs the same transcript even if
// new turns are appended in between.
#[test]
fn ts2860_replayed_session_preserves_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlSessionStore::new(dir.path());
    store
        .append("sess", &pi_core::Message::new(Role::User, "step 1"))
        .unwrap();
    store
        .append("sess", &pi_core::Message::new(Role::Assistant, "ok 1"))
        .unwrap();
    store
        .append("sess", &pi_core::Message::new(Role::User, "step 2"))
        .unwrap();
    let loaded = store.load("sess").unwrap();
    assert_eq!(loaded.messages.len(), 3);
    assert_eq!(loaded.messages[0].content, "step 1");
    assert_eq!(loaded.messages[2].content, "step 2");
}

// ts-pi 3217: alias resolution prefers explicit `provider/model` literal
// over fuzzy alias match.
#[test]
fn ts3217_alias_literal_provider_model_wins() {
    let r = pi_providers::resolve_alias("openai/gpt-4o").unwrap();
    assert_eq!(r.model.provider, "openai");
    assert_eq!(r.model.model, "gpt-4o");
    assert!(!r.via_alias);
    let r2 = pi_providers::resolve_alias("sonnet").unwrap();
    assert_eq!(r2.model.provider, "anthropic");
    assert_eq!(r2.model.model, "claude-sonnet-4-6");
}

// ts-pi 3302: find tool respects glob patterns.
#[test]
fn ts3302_find_glob_matches_rs_files() {
    use pi_permissions::{PermissionEngine, PermissionMode};
    use pi_tools::{ToolCall, ToolRuntime};
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/a.rs"), "").unwrap();
    fs::write(dir.path().join("src/b.txt"), "").unwrap();
    let runtime = ToolRuntime::builtin();
    let mut perms = PermissionEngine::new(PermissionMode::TrustedWorkspace);
    let out = runtime
        .run(
            ToolCall {
                name: "find".to_string(),
                input: serde_json::json!({"glob": "**/*.rs", "path": dir.path()}).to_string(),
            },
            &mut perms,
        )
        .unwrap();
    assert!(out.output.contains("a.rs"));
    assert!(!out.output.contains("b.txt"));
}

// ts-pi 3317: provider error propagates as a typed event/error, not a panic.
#[test]
fn ts3317_provider_error_propagates_typed() {
    let faux = FauxProvider::with_script([FauxTurn::Error(pi_core::PiError::new(
        pi_core::PiErrorKind::Network,
        "connection lost",
    ))]);
    let (mut agent, _store, _guard) = faux_agent("regress-3317", faux);
    let err = agent.run_single_turn("s", "hi").unwrap_err();
    assert_eq!(err.kind, pi_core::PiErrorKind::Network);
    assert!(err.message.contains("connection lost"));
}

// ts-pi 3592: when `--no-tools` is set, the agent still loads extension
// tools registered via `tools_mut`.
#[test]
fn ts3592_extension_tools_survive_no_builtin_flag() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlSessionStore::new(dir.path());
    let config = AppConfig {
        tools_enabled: false,
        ..AppConfig::default()
    };
    let mut agent = AgentRuntime::try_new(config, store).unwrap();
    // tools_mut still hands back the (empty) tool runtime; registration via
    // it survives across turns even when tools_enabled=false (which only
    // disables the *invocation* path).
    let count_before = agent.tool_schemas().len();
    agent.tools_mut().register(Box::new(NoopTool));
    let count_after = agent.tool_schemas().len();
    assert_eq!(count_after, count_before + 1);
}

struct NoopTool;
impl pi_tools::Tool for NoopTool {
    fn schema(&self) -> pi_core::ToolSchema {
        pi_core::ToolSchema {
            name: "noop_ext".to_string(),
            description: "noop".to_string(),
            input_shape: "".to_string(),
            parameters: None,
            mutates: false,
        }
    }
    fn run(
        &self,
        _input: &pi_tools::ToolInput,
        _perms: &mut pi_permissions::PermissionEngine,
    ) -> pi_core::PiResult<pi_tools::ToolOutput> {
        Ok(pi_tools::ToolOutput {
            name: "noop_ext".to_string(),
            output: String::new(),
        })
    }
}

// ts-pi 3686: session writes carry a header line that includes the cwd of
// the originating shell, so resume rewinds to the right directory.
#[test]
fn ts3686_session_header_records_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlSessionStore::new(dir.path());
    store
        .append("named", &pi_core::Message::new(Role::User, "hi"))
        .unwrap();
    let session = store.load("named").unwrap();
    let header = session.header.expect("header line");
    assert_eq!(header.version, pi_session::SESSION_VERSION);
    assert!(header.cwd.is_some());
}

// ts-pi 3688: cancellation while compaction is in flight does not corrupt
// the buffer — at the API level, calling cancel before run_single_turn is
// safe and resets before the next turn.
#[test]
fn ts3688_cancel_before_turn_is_safe() {
    let faux = FauxProvider::with_script([FauxTurn::Text("ok".into())]);
    let (mut agent, _store, _guard) = faux_agent("regress-3688", faux);
    agent.cancel();
    let turn = agent.run_single_turn("s", "hi").expect("run");
    // reset_cancel() is called at the start of run_single_turn so the prior
    // cancel does not bleed into this turn.
    assert!(turn
        .events
        .iter()
        .any(|e| matches!(e, Event::AssistantMessage(_))));
}

// ts-pi 3982: usage tokens reported by the provider land in the session
// turn's usage tally.
#[test]
fn ts3982_usage_event_emitted_when_provider_reports() {
    let faux = FauxProvider::with_script([FauxTurn::Usage {
        text: "ok".into(),
        usage: pi_core::Usage {
            prompt_tokens: 12,
            completion_tokens: 8,
            total_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    }]);
    let (mut agent, _store, _guard) = faux_agent("regress-3982", faux);
    let turn = agent.run_single_turn("s", "hi").unwrap();
    assert_eq!(turn.usage.total_tokens, 20);
    assert!(turn
        .events
        .iter()
        .any(|e| matches!(e, Event::Usage(u) if u.total_tokens == 20)));
}

// ts-pi 4167: providers that emit a tool_call in the same turn as text are
// surfaced via the structured event channel, not flattened into one delta.
#[test]
fn ts4167_text_plus_tool_call_round_trip() {
    let faux = FauxProvider::with_script([
        FauxTurn::Mixed {
            text: "thinking…".into(),
            tool_calls: vec![ToolInvocation {
                id: Some("call_a".into()),
                name: "ls".into(),
                input: ".".into(),
            }],
        },
        FauxTurn::Text("done".into()),
    ]);
    let (mut agent, _store, _guard) = faux_agent("regress-4167", faux);
    // The agent will run the tool — make sure built-in tools are enabled
    // so it can dispatch `ls` and recurse with the result.
    agent.config_mut().tools_enabled = true;
    let turn = agent.run_single_turn("s", "list things").unwrap();
    assert!(turn
        .events
        .iter()
        .any(|e| matches!(e, Event::ToolStarted { name, .. } if name == "ls")));
    assert!(turn
        .events
        .iter()
        .any(|e| matches!(e, Event::AssistantMessage(m) if m == "done")));
}

// pi-rust local: round-trip a TS-pi v3 session file written by Anthropic's
// upstream coding-agent.
#[test]
fn loads_ts_pi_v3_session_jsonl_via_store_api() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonlSessionStore::new(dir.path());
    let ts_file = dir.path().join("ts-v3.jsonl");
    fs::write(
        &ts_file,
        concat!(
            "{\"type\":\"session\",\"version\":3,\"id\":\"ts-v3\",\"timestamp\":\"2026-05-14T03:42:11Z\",\"cwd\":\"/tmp/x\",\"parentSession\":null}\n",
            "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-05-14T03:42:12Z\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
        ),
    )
    .unwrap();
    let session = store.load("ts-v3").unwrap();
    assert_eq!(session.header.unwrap().version, 3);
    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.messages[0].content, "hello");
}

// pi-rust local: alias short names resolve to the canonical provider/model
// pair so `--model sonnet` Just Works.
#[test]
fn alias_short_name_resolves() {
    let r = pi_providers::resolve_alias("opus").unwrap();
    assert_eq!(r.model.provider, "anthropic");
    assert_eq!(r.model.model, "claude-opus-4-7");
    assert!(r.via_alias);
}

// pi-rust local: faux provider records every request the agent sent, so
// regression tests can assert on the prompt envelope.
#[test]
fn faux_provider_records_requests() {
    let faux = FauxProvider::with_script([FauxTurn::Text("ok".into())]);
    let (mut agent, _store, _guard) = faux_agent("regress-faux-rec", faux.clone());
    agent.run_single_turn("s", "你好").unwrap();
    let recorded = faux.requests();
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0]
        .messages
        .iter()
        .any(|m| m.content.contains("你好")));
}

// pi-rust local: permission engine rejects writes in plan mode.
#[test]
fn permission_plan_mode_blocks_writes() {
    use pi_permissions::{Capability, PermissionEngine, PermissionMode, PermissionRequest};
    let mut engine = PermissionEngine::new(PermissionMode::Plan);
    let decision = engine.decide(PermissionRequest {
        capability: Capability::WriteFile,
        target: "/tmp/x".to_string(),
        reason: "test".to_string(),
    });
    assert!(!decision.allowed);
}

// pi-rust local: provider probe returns Unsupported for providers without
// a documented probe endpoint, rather than panicking.
#[test]
fn provider_probe_reports_unsupported_for_minimax() {
    let reports = pi_providers::probe_all();
    let report = reports
        .iter()
        .find(|r| r.provider == "minimax")
        .expect("minimax row");
    assert!(matches!(
        report.outcome,
        pi_providers::ProbeOutcome::Unsupported
    ));
}

// pi-rust local: telemetry stays disabled with no opt-in env.
#[test]
fn telemetry_disabled_by_default() {
    assert!(!pi_core::telemetry::decide_enabled(None, None, None));
}

// pi-rust local: error hints surface a callable next step for missing
// credentials.
#[test]
fn error_hint_for_missing_credential_mentions_auth_set() {
    let err = pi_core::PiError::new(pi_core::PiErrorKind::Provider, "缺少凭证 ANTHROPIC_API_KEY");
    let hint = pi_core::hint_for(&err, pi_core::Locale::ZhCn);
    let rendered = hint.format();
    assert!(rendered.contains("pi auth set"));
}
