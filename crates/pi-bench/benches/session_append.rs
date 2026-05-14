//! Measures throughput of the JSONL session store's append+load cycle. The
//! parity ledger references this benchmark as the "session_append" entry.
//!
//! Run: `cargo bench -p pi-bench --bench session_append`.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use pi_bench::synthetic_messages;
use pi_session::{JsonlSessionStore, SessionStore};
use tempfile::tempdir;

fn bench_append(c: &mut Criterion) {
    let messages = synthetic_messages(64, 96);

    c.bench_function("session_append_64_messages", |b| {
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

criterion_group!(benches, bench_append);
criterion_main!(benches);
