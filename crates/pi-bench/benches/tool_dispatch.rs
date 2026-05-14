//! Tool-dispatch latency: parse a structured JSON input, walk the registry,
//! execute the tool, return output. Mirrors pi_agent_rust's `tools` bench
//! family (truncation/sse) for a comparable "hot tool path" metric.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use pi_permissions::{PermissionEngine, PermissionMode};
use pi_tools::{ToolCall, ToolRuntime};
use serde_json::json;
use tempfile::tempdir;

fn bench_dispatch(c: &mut Criterion) {
    let mut group = c.benchmark_group("tool_dispatch_pi_rust");
    let runtime = ToolRuntime::builtin();

    // Read tool dispatch on a tiny file: simulates the bash-result→read path
    // that dominates real-world tool latency.
    let dir = tempdir().expect("tempdir");
    let file = dir.path().join("note.txt");
    std::fs::write(&file, "line 1\nline 2\nline 3\n").unwrap();
    let read_input = serde_json::to_string(&json!({"path": file})).unwrap();

    group.bench_function("read_one_file", |b| {
        b.iter(|| {
            let mut perms = PermissionEngine::new(PermissionMode::TrustedWorkspace);
            let out = runtime
                .run(
                    ToolCall {
                        name: "read".to_string(),
                        input: read_input.clone(),
                    },
                    &mut perms,
                )
                .expect("read");
            black_box(out);
        });
    });

    // Pure registry dispatch with a no-op-ish ls on a tiny dir.
    let ls_input = serde_json::to_string(&json!({"path": dir.path()})).unwrap();
    group.bench_function("ls_small_dir", |b| {
        b.iter(|| {
            let mut perms = PermissionEngine::new(PermissionMode::TrustedWorkspace);
            let out = runtime
                .run(
                    ToolCall {
                        name: "ls".to_string(),
                        input: ls_input.clone(),
                    },
                    &mut perms,
                )
                .expect("ls");
            black_box(out);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_dispatch);
criterion_main!(benches);
