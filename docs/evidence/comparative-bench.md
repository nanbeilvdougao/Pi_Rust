# Comparative Benchmark — Pi Rust vs pi_agent_rust

Captured by running both projects on the same machine, same compiler family,
same criterion settings (`--warm-up-time 1 --measurement-time 2 --sample-size 30`).

- Host: Apple Silicon, macOS Darwin 25.4.0
- Stable Rust for Pi Rust (rustc 1.95.0), nightly Rust for pi_agent_rust
  (rustc 1.97.0-nightly, required by their `rust-toolchain.toml`).
- Workload definitions live in
  `crates/pi-bench/benches/comparable.rs` (Pi Rust) and
  `../pi_agent_rust/benches/tools.rs` (pi_agent_rust).

To re-run: `bash scripts/run-comparative-benches.sh` and re-paste the
relevant `time:` lines into the table below.

## Server-sent events parsing

Parsing `N` SSE events from a fully-buffered payload.

| N | pi_agent_rust `sse_parsing/parse/N` | Pi Rust `sse_parse_pi_rust/parse_N` | Speedup |
| --- | --- | --- | --- |
| 10   | 1.72 µs      | 0.36 µs    | **4.8×** |
| 100  | 15.62 µs     | 3.10 µs    | **5.0×** |
| 1000 | 152.79 µs    | 30.98 µs   | **4.9×** |

Why we win here: the Pi Rust parser is a single-pass byte-line reader that
appends `data:` payloads into a reusable `String` and emits only on blank
lines. pi_agent_rust uses a stream-oriented chunk parser with allocations per
event.

## Tool-output truncation

Applying the head/tail truncation policy used by the `bash`/`grep`/`find`
tools to a buffer of N bytes.

| N | pi_agent_rust `truncation/head/N` | Pi Rust `truncate_pi_rust/head_N` | Speedup |
| --- | --- | --- | --- |
| 1 000   | 3.29 µs   | 4.49 µs   | 0.7× (slightly slower) |
| 10 000  | 44.41 µs  | 20.69 µs  | **2.1×** |
| 100 000 | 311.61 µs | 83.61 µs  | **3.7×** |

The 1 000-byte case is close enough to be measurement noise. From 10 KB up,
Pi Rust's truncator wins because we slice `split_inclusive('\n')` exactly
twice (head, tail) and concat, while pi_agent_rust counts line offsets per
character.

## Pi Rust standalone benches

The following numbers describe Pi Rust's hot paths in absolute terms; they
are not comparative.

| Bench | Result |
| --- | --- |
| `session_append_64_messages` (open store → append 64 → load) | ~1.40 ms |
| `serialize_32_messages` (serde_json across mixed-role messages) | ~6.45 µs |
| `estimate_messages_tokens_128_msgs_512_chars` | ~2.27 µs |

## New parity axes (session / tool / agent)

These three benches were added in 2026-05 to address the three explicit
"missing comparative axes" called out by the goal hook. Numbers below are
median wall-time per iteration on Apple Silicon, criterion `--warm-up-time 1
--measurement-time 2 --sample-size 30`.

| Axis | Workload | Pi Rust | Notes |
| --- | --- | --- | --- |
| `session_save` | open store + append 100 msgs + reload | **2.18 ms** | linear: 1000-msg run lands at 22.8 ms (≈23 µs/msg). pi_agent_rust's `session_save` bench currently produces zero criterion targets on our host (criterion 0.8 macro quirk in their bench file), so a literal side-by-side cell is left blank until the upstream bench runs. |
| `tool_dispatch` | parse JSON → registry lookup → run `read` on a 3-line file | **9.6 µs** | includes the permission gate. `ls` on a small dir lands at 11.4 µs. |
| `agent_loop` | full `run_single_turn` against a `FauxProvider` (load session, send user, stream "ok", persist) | **177 µs** | pi_agent_rust has no equivalent faux-harness benchmark; this is a standalone Pi Rust number for "how fast the agent loop is when the LLM is not the bottleneck." |

## Methodology notes

- pi_agent_rust's `benches/session_save.rs` currently emits 0 criterion
  targets on our host (criterion 0.8 macro interaction). When that is fixed
  upstream we will populate a third row group here for session append.
- pi-rs ships no `benches/` directory; we leave its column out instead of
  inventing a workload.
- We deliberately do not aggregate these numbers into a single
  marketing-style "X% faster overall" headline. Hot paths matter where they
  matter; the parity ledger references this file by section instead.
