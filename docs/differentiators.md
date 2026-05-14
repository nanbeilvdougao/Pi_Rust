# Differentiators

What Pi Rust does that the TS pi and the existing Rust ports
(`pi_agent_rust`, `pi-rs`) do not, or do less well.

## Local-first, Chinese-first

- Default diagnostics, system prompt, slash commands, and error messages are
  Simplified Chinese.
- Built-in provider registry includes Moonshot, DeepSeek, Qwen/DashScope,
  Zhipu GLM, MiniMax, Ollama, and a local echo before OpenAI / Anthropic / Gemini.
- `pi --locale en` switches to English for users who want the same surface in
  a Latin-script terminal.
- Single static binary; no Node, no Python, no shell-out to `curl` at runtime
  (we use `ureq` + rustls).

## Real streaming for every provider

Every built-in provider implements both `complete()` and `stream()`:

| Provider | Streaming wire format |
| --- | --- |
| OpenAI-compatible (OpenAI, Moonshot, DeepSeek, Qwen, Zhipu, MiniMax) | SSE chunks with `tool_calls`, `reasoning_content`, `usage` |
| Anthropic | SSE with `content_block_delta`, `tool_use`, `thinking_delta`, prompt caching usage |
| Gemini | SSE `:streamGenerateContent?alt=sse`, function calls, usage metadata |
| Ollama | NDJSON `/api/chat`, structured tool calls, `prompt_eval_count` / `eval_count` |
| Echo | Synthetic events for offline tests |

The `StreamSink` trait is uniform across providers so the agent loop never
branches on transport.

## Cooperative cancellation across the stack

A single `Arc<AtomicBool>` is shared between the TUI, the agent, and every
streaming provider. Ctrl+C flips it; provider sinks check `cancelled()`
between events and unwind cleanly without panicking. The agent emits a
`Cancelled` event so the UI can render the partial output.

## Permission engine with audit log and modes

`pi-permissions` is a real engine, not a comment:

- Four modes: `read-only`, `confirm`, `trusted`, `plan`.
- Per-decision audit log with timestamps, exported as JSON.
- Sandbox profile narrows file ops to a workspace root with extra read
  roots.
- Dangerous-target blocklist covers `rm -rf /`, `mkfs`, `dd if=`, fork bombs,
  raw device paths, `shutdown`/`reboot`.
- Every tool call AND every extension hostcall runs through it.

## Extension ABI v1

`pi-ext` ships a stable ABI with a working process host:

- Manifest is TOML with `kind = "process"` or `kind = "shell"` entry.
- Host frames extensions via line-delimited JSON over stdio.
- Hostcalls map to capabilities the manifest must declare AND the engine
  must allow; either missing → reply with `ok=false, error=...`.
- A wasmtime host can be added later without changing the manifest format.

## Tool surface in line with TS pi

- `read` accepts `{path, offset, limit, line_numbers}`.
- `edit` accepts `{path, old, new, expect_unique, replace_all}` and returns
  a unified diff preview alongside the success message.
- `bash` accepts `{command, cwd, timeout_ms}` and reports `--- exit N ---`.
- `grep` is a real regex search with `case_sensitive` and `max_results`.
- `find` is a real glob (`globset`) search.
- Tools accept both structured JSON (provider tool-call path) and the legacy
  plain-string form (`/tool` shortcuts).

## Context compaction the agent actually uses

The agent calls `maybe_compact` before every provider turn. When the
estimated token usage crosses `context_window_tokens * compaction_threshold`
(defaults to 128k * 0.85), it keeps the head (system + first user) and the
last few user/assistant pairs intact, summarizes the middle, and replaces it
with a single `[summary]` system message. Compaction is observable through a
`Event::Compacted { before, after }` event.

## Evidence over claims

- `pi-bench` exposes criterion benches for the JSONL session store, JSON
  serialize, token estimate, and the comparable workloads pi_agent_rust also
  measures (SSE parsing, output truncation).
- Captured side-by-side numbers live at
  `docs/evidence/comparative-bench.md`. On Apple Silicon stable Rust vs.
  pi_agent_rust on nightly:
  - **SSE parsing 10 / 100 / 1000 events: 4.8× / 5.0× / 4.9× faster.**
  - **Tool-output truncation 10 KB / 100 KB: 2.1× / 3.7× faster.**
- Every numeric performance claim is regenerable via
  `scripts/run-comparative-benches.sh` and re-published into the same doc.
- The parity ledger lists what is and is not aligned with TS pi.

## Compared with `pi_agent_rust` and `pi-rs`

| Capability | pi_agent_rust | pi-rs | Pi Rust |
| --- | --- | --- | --- |
| Built-in Chinese providers | partial | none | 5 (Moonshot, DeepSeek, Qwen, Zhipu, MiniMax) |
| Anthropic native + SSE | partial | partial | yes, with thinking + caching usage |
| Gemini native + SSE | no | no | yes |
| Ollama streaming | partial | partial | yes (NDJSON `/api/chat`) |
| Capability permission engine | basic | basic | 4 modes + audit + sandbox + blocklist |
| Slash command system | partial | partial | yes (built-ins + workspace `.pi/commands/*.md`) |
| Skills system | partial | partial | yes (`.pi/skills/*.md` with `always` / `command` / `keyword` triggers) |
| Layered settings | partial | partial | yes (user → workspace → CLI) |
| Context compaction | no | no | yes, configurable threshold |
| Interactive TUI | partial | partial | ratatui+crossterm with streaming render |
| Process extension host | no | no | yes (JSON-RPC over stdio) |
| WASM extension host | no | no | yes (wasmtime + WASI preview 1, feature `wasm`) |
| SQLite session index + FTS | no | no | yes (feature `sqlite-index`) |
| Multimodal image attachments | partial | partial | yes (OpenAI / Anthropic / Gemini) |
| Tool diff preview on edit | no | no | yes |
| Output truncation policy | yes | partial | **2.1–3.7× faster** at ≥10 KB |
| SSE streaming parser | yes | partial | **~4.9× faster** |
| RPC mode for SDK/IDE | no | no | yes (`pi --rpc`, JSON-RPC over stdio) |
| Self-update | no | no | yes (`pi --self-update --yes`) |
| Criterion benchmark harness | yes | no | yes (with comparative report) |
| `unsafe_code = forbid` workspace lint | partial | no | yes |
