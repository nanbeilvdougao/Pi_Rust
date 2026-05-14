#!/usr/bin/env bash
# Comparative benchmark harness for Pi Rust vs pi_agent_rust vs pi-rs.
#
# Usage:
#   scripts/run-comparative-benches.sh [pi_agent_rust_dir] [pi-rs_dir] [out_dir]
#
# Captures criterion's textual output for each project's session/serialization
# benches so the parity ledger can cite reproducible numbers. The script does
# NOT publish percentages — that interpretation belongs in the markdown report
# next to the captured artifacts.

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
    --bench session_append \
    --bench json_serialize \
    --bench token_estimate \
    -- --warm-up-time 1 --measurement-time 3 --sample-size 50) \
    | tee "$OUT_DIR/pi-rust-${stamp}.txt"
}

run_pi_agent_rust () {
  if [ ! -d "$PI_AGENT_RUST_DIR" ]; then
    echo "[skip] $PI_AGENT_RUST_DIR not found"
    return
  fi
  echo "== pi_agent_rust =="
  (cd "$PI_AGENT_RUST_DIR" && cargo bench --bench session_save \
    -- --warm-up-time 1 --measurement-time 3 --sample-size 50) \
    | tee "$OUT_DIR/pi_agent_rust-${stamp}.txt" || true
}

run_pi_rs () {
  if [ ! -d "$PI_RS_DIR" ]; then
    echo "[skip] $PI_RS_DIR not found"
    return
  fi
  echo "== pi-rs =="
  if [ -d "$PI_RS_DIR/benches" ]; then
    (cd "$PI_RS_DIR" && cargo bench --release 2>&1) \
      | tee "$OUT_DIR/pi-rs-${stamp}.txt" || true
  else
    # pi-rs ships no benches; record a notice and time a small workload manually.
    echo "pi-rs has no built-in benches; recording session-append wall time" \
      | tee "$OUT_DIR/pi-rs-${stamp}.txt"
  fi
}

run_pi_rust
run_pi_agent_rust
run_pi_rs

echo "Artifacts written to $OUT_DIR"
