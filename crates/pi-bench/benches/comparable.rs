//! Comparable workloads against pi_agent_rust's `tools` bench so the parity
//! ledger can quote side-by-side numbers without paraphrasing competitor
//! results. We mirror their two named workloads:
//!
//! - `truncation/head/N` — apply an N-char head/tail truncation policy.
//! - `sse_parse/N` — parse N SSE events out of an `[N data: blocks]` body.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use pi_providers::read_sse_for_bench;
use pi_tools::truncate::{truncate, TruncationPolicy};

fn bench_truncate(c: &mut Criterion) {
    let mut group = c.benchmark_group("truncate_pi_rust");
    let sample: String = (0..2000).map(|i| format!("line {i}\n")).collect();
    for size in [1000usize, 10_000, 100_000] {
        let big = sample.repeat((size / sample.len()).max(1));
        let big = big[..big.len().min(size * 4)].to_string();
        group.bench_function(format!("head_{size}"), |b| {
            b.iter(|| {
                black_box(truncate(
                    &big,
                    TruncationPolicy {
                        byte_cap: size,
                        line_cap: size / 8,
                        head_lines: size / 16,
                        tail_lines: size / 16,
                    },
                ));
            });
        });
    }
    group.finish();
}

fn bench_sse(c: &mut Criterion) {
    let mut group = c.benchmark_group("sse_parse_pi_rust");
    for count in [10usize, 100, 1000] {
        let mut payload = String::new();
        for i in 0..count {
            payload.push_str(&format!("data: {{\"index\":{i}}}\n\n"));
        }
        let bytes = payload.into_bytes();
        group.bench_function(format!("parse_{count}"), |b| {
            b.iter(|| {
                let mut total = 0usize;
                read_sse_for_bench(bytes.as_slice(), |_| {
                    total += 1;
                    Ok(())
                })
                .expect("parse");
                black_box(total);
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_truncate, bench_sse);
criterion_main!(benches);
