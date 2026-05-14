//! End-to-end agent-loop latency driven by a `FauxProvider`. Measures the
//! cost of:
//!
//! - load session
//! - append user message
//! - call provider (faux, ~zero latency)
//! - dispatch stream events into agent sink
//! - persist assistant message
//!
//! This is the single most representative number for "how fast does pi
//! respond from a user keypress, assuming network/LLM are not the
//! bottleneck". Pi_agent_rust does not expose an equivalent harness, so
//! `docs/evidence/comparative-bench.md` records this as a standalone metric.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use pi_agent::AgentRuntime;
use pi_core::{AppConfig, ModelSelection};
use pi_providers::{register_test_provider, FauxProvider, FauxTurn};
use pi_session::JsonlSessionStore;
use tempfile::tempdir;

fn bench_agent_turn(c: &mut Criterion) {
    let mut group = c.benchmark_group("agent_loop_pi_rust");
    group.bench_function("faux_round_trip", |b| {
        b.iter_with_setup(
            || {
                let dir = tempdir().expect("tempdir");
                let faux = FauxProvider::with_script([FauxTurn::Text("ok".into())]);
                let _guard = register_test_provider("bench-faux", faux);
                let config = AppConfig {
                    model: ModelSelection {
                        provider: "bench-faux".to_string(),
                        model: "faux".to_string(),
                    },
                    tools_enabled: false,
                    ..AppConfig::default()
                };
                let store = JsonlSessionStore::new(dir.path().to_path_buf());
                let agent = AgentRuntime::try_new(config, store).expect("agent");
                (dir, agent, _guard)
            },
            |(_, mut agent, _guard)| {
                let turn = agent.run_single_turn("bench", "hi").expect("run");
                black_box(turn);
            },
        );
    });
    group.finish();
}

criterion_group!(benches, bench_agent_turn);
criterion_main!(benches);
