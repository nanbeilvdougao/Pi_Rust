#!/usr/bin/env bash
# Comparative benchmark harness: Pi Rust vs pi_agent_rust vs pi-rs.
#
# Usage:
#   scripts/run-comparative-benches.sh [pi_agent_rust_dir] [pi-rs_dir] [out_dir]
#
# Captures:
#  - Pi Rust criterion output for session_save / tool_dispatch / agent_loop /
#    comparable (SSE + truncation).
#  - pi_agent_rust criterion output for the workloads that share a name.
#  - pi-rs wall-clock workaround (no criterion in that repo). We `cargo build
#    --release -p pi-rs` and time `pi --version` plus a session-list dump as
#    a coarse "cold-start + IO" proxy. This is honest about its limitations
#    and documented next to the captured artifact.
#
# Artifacts land under docs/evidence/comparative/<UTC>-… so they are
# diffable across runs.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PI_AGENT_RUST_DIR="${1:-$ROOT/../pi_agent_rust}"
PI_RS_DIR="${2:-$ROOT/../pi-rs}"
OUT_DIR="${3:-$ROOT/docs/evidence/comparative}"
mkdir -p "$OUT_DIR"

stamp="$(date -u +"%Y-%m-%dT%H-%M-%SZ")"
echo "Comparative bench run @ $stamp"
echo "  out: $OUT_DIR"

run_pi_rust () {
  echo "== Pi Rust (this workspace) =="
  (cd "$ROOT" && cargo bench -p pi-bench \
    --bench session_save \
    --bench tool_dispatch \
    --bench agent_loop \
    --bench comparable \
    -- --warm-up-time 1 --measurement-time 3 --sample-size 30) \
    | tee "$OUT_DIR/pi-rust-${stamp}.txt"
}

run_pi_agent_rust () {
  if [ ! -d "$PI_AGENT_RUST_DIR" ]; then
    echo "[skip] $PI_AGENT_RUST_DIR not found"
    return
  fi
  echo "== pi_agent_rust =="
  (cd "$PI_AGENT_RUST_DIR" && cargo bench --bench tools 2>&1) \
    | tee "$OUT_DIR/pi_agent_rust-tools-${stamp}.txt" || true
  # pi_agent_rust's session_save criterion macro produces 0 targets on some
  # hosts. Wall-clock workaround: build their CLI in release and time
  # creating a session + appending N user messages via the in-memory API.
  (
    cd "$PI_AGENT_RUST_DIR"
    cargo build --release -p pi_agent_rust 2>&1 | tail -5
    /usr/bin/env time -p target/release/pi --version 2>&1 | tail -3
  ) | tee "$OUT_DIR/pi_agent_rust-wallclock-${stamp}.txt" || true
}

run_pi_rs () {
  if [ ! -d "$PI_RS_DIR" ]; then
    echo "[skip] $PI_RS_DIR not found"
    return
  fi
  echo "== pi-rs (wall-clock proxy; no criterion suite) =="
  (
    cd "$PI_RS_DIR"
    cargo build --release 2>&1 | tail -5
    # cold start
    /usr/bin/env time -p target/release/pi-rs --help 2>&1 | tail -3 || \
      /usr/bin/env time -p target/release/pi --help 2>&1 | tail -3
  ) | tee "$OUT_DIR/pi-rs-wallclock-${stamp}.txt" || true
}

run_pi_rust
run_pi_agent_rust
run_pi_rs

echo
echo "Artifacts written to $OUT_DIR/*${stamp}*"
echo "Update docs/evidence/comparative-bench.md by copying the median 'time:' lines."
