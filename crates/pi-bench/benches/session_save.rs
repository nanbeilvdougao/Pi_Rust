//! Session append + load throughput. Comparable to pi_agent_rust's
//! `session_save` bench (in-memory append + entries.clone).
//!
//! Our session store is JSONL-on-disk, so we measure the actual fs path —
//! that's the apples-to-apples for a real `pi --resume` use case. Numbers
//! land in `docs/evidence/comparative-bench.md`.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use pi_bench::synthetic_messages;
use pi_session::{JsonlSessionStore, SessionStore};
use tempfile::tempdir;

fn bench_session_save(c: &mut Criterion) {
    let mut group = c.benchmark_group("session_save_pi_rust");
    for count in [100usize, 1000] {
        let messages = synthetic_messages(count, 96);
        group.bench_function(format!("append_{count}_messages"), |b| {
            b.iter(|| {
                let dir = tempdir().expect("tempdir");
                let store = JsonlSessionStore::new(dir.path());
                for message in &messages {
                    store.append("bench", message).expect("append");
                }
                black_box(store.load("bench").expect("load"));
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_session_save);
criterion_main!(benches);
