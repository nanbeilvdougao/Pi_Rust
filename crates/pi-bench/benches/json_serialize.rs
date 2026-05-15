//! Measures the serde_json serialization cost for the message types we send
//! to providers. The parity ledger references this benchmark as
//! "wire_serialize".

#![allow(clippy::expect_used)]

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use pi_bench::synthetic_messages;

fn bench_serialize(c: &mut Criterion) {
    let messages = synthetic_messages(32, 256);

    c.bench_function("serialize_32_messages", |b| {
        b.iter(|| {
            for message in &messages {
                let s = serde_json::to_string(message).expect("serialize");
                black_box(s);
            }
        });
    });

    c.bench_function("escape_legacy_path", |b| {
        b.iter(|| {
            for message in &messages {
                black_box(pi_core::escape_json_string(&message.content));
            }
        });
    });
}

criterion_group!(benches, bench_serialize);
criterion_main!(benches);
