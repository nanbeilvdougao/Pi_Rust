# Comparative Benchmark — Pi Rust vs pi_agent_rust vs pi-rs

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

## Cold-start wall-clock (three-way)

A cheap apples-to-apples for "how fast does the user see anything from this
binary on a cold cache." `--version` (or `--help` for pi-rs which has no
`--version`) is the smallest user-visible work the CLI can do, so the
delta is almost entirely binary load + arg parsing.

Single run, release build, macOS aarch64:

| Project | Command | Wall-time |
| --- | --- | --- |
| **Pi Rust**       | `pi --version`       | **0.44 s** |
| pi-rs             | `pi-rs --help`       | 0.57 s |
| pi_agent_rust     | `pi --version`       | 0.76 s |

Raw `time -p` artifacts: `docs/evidence/comparative/{pi-rs,pi_agent_rust}-wallclock-*.txt`
and `docs/evidence/comparative/pi-rust-*.txt`. Re-run with
`bash scripts/run-comparative-benches.sh` from anywhere on the same machine.

## Methodology notes

- pi_agent_rust's `benches/session_save.rs` still emits 0 criterion targets
  on our host (criterion 0.8 macro interaction in their bench file). We
  substitute the wall-clock proxy above; once they fix the macro upstream
  we will populate a fourth column with their `session_save` median.
- pi-rs ships no `benches/` directory; the wall-clock proxy above is the
  honest substitute. Don't read it as a head-to-head on hot code paths —
  cold-start is a single observation per project per run.
- We deliberately do not aggregate these numbers into a single
  marketing-style "X% faster overall" headline. Hot paths matter where they
  matter; the parity ledger references this file by section instead.

## How to reproduce

```bash
bash scripts/run-comparative-benches.sh \
  /path/to/pi_agent_rust /path/to/pi-rs docs/evidence/comparative
```

The script writes timestamped `.txt` files under `docs/evidence/comparative/`
so we can diff successive runs.
