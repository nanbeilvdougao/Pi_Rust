//! Measures the token-estimate function used for context window pressure.
//! This is on the hot path of every provider call and the compaction trigger.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use pi_bench::synthetic_messages;
use pi_core::estimate_messages_tokens;

fn bench_token_estimate(c: &mut Criterion) {
    let messages = synthetic_messages(128, 512);

    c.bench_function("estimate_messages_tokens_128_msgs_512_chars", |b| {
        b.iter(|| {
            black_box(estimate_messages_tokens(&messages));
        });
    });
}

criterion_group!(benches, bench_token_estimate);
criterion_main!(benches);
