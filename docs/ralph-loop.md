# Ralph Loop

Completion promise: `IMPROVEMENTS_DONE`

Goal: keep improving Pi Rust until it reaches full functional parity with the TypeScript Pi baseline and surpasses existing Rust implementations in selected dimensions.

## Iteration 1

Focus: move beyond scaffold-only provider registration.

Changes:

- Added a real local Ollama provider path using the `/api/chat` endpoint.
- Added `--list-models` to improve CLI parity with Pi-style provider discovery.
- Extended provider metadata with supported model presets.
- Kept `echo` as a deterministic no-network test provider.

Why this matters:

- Local Ollama gives us a real model execution path without cloud credentials.
- The provider registry now separates provider availability from implemented execution.
- This is the first step toward matching TypeScript Pi behavior without depending on paid APIs.

Remaining gaps:

- Cloud providers still need real HTTPS clients and streaming parsers.
- Tool calling is still shortcut-driven instead of model-directed.
- TUI, RPC, SDK, extension runtime, project indexing, and compaction remain incomplete.

## Iteration 2

Focus: expose tool discovery and selection through CLI instead of keeping tools as internal-only machinery.

Changes:

- Added `--list-tools` to print built-in tool schemas.
- Added `--tools <LIST>` to select a comma-separated tool subset.
- Added validation for unknown tool names.
- Updated CLI parity evidence for tool discovery.

Why this matters:

- Tool schema visibility is required before provider-side tool calling can be made compatible with TypeScript Pi.
- Restricting tools is a safety and reproducibility primitive for future differential tests.

## Iteration 3

Focus: make domestic OpenAI-compatible providers executable instead of metadata-only.

Changes:

- Added Moonshot, DeepSeek, and Qwen provider execution paths.
- Added API key diagnostics for `MOONSHOT_API_KEY`, `DEEPSEEK_API_KEY`, and `DASHSCOPE_API_KEY`.
- Added OpenAI-compatible chat request shaping and response content extraction.
- Kept Ollama as the preferred no-cloud local path.

Why this matters:

- Chinese/local provider support is now a real product differentiator rather than a README claim.
- The provider abstraction can now cover local HTTP and OpenAI-compatible HTTPS providers.

Known limitation:

- Cloud providers currently use `curl` as an interim TLS transport because the current environment lacks Cargo/Rust tooling to add and verify a native HTTP dependency.

## Iteration 4

Focus: make the CLI easier to operate and debug.

Changes:

- Added `pi doctor`.
- Reports whether `cargo`, `rustc`, and `curl` are available.
- Reports the session root and whether it exists.
- Reports provider credential environment status without printing secrets.

Why this matters:

- Installation and provider failures become diagnosable without reading source code.
- This supports the design goal of better operational clarity than ad hoc Rust ports.

## Iteration 7

Focus: preserve structured tool result identity across the agent/session/provider loop.

Changes:

- Added optional `tool_call_id` metadata to `Message`.
- Persisted `tool_call_id` in JSONL session records.
- Agent now writes provider-driven tool outputs with the originating call ID.
- OpenAI-compatible requests serialize tool results as `role=tool` with `tool_call_id` when available.

Why this matters:

- Tool result identity is required for real OpenAI-compatible tool calling parity.
- This removes a major simplification from the previous provider-driven tool loop.

## Iteration 8

Focus: expand session management beyond list/resume.

Changes:

- Added session deletion.
- Added session rename.
- Added Markdown session export.
- Extended CLI help and session contracts for lifecycle operations.

Why this matters:

- TS Pi-style parity requires session management as a user-facing workflow, not only append/load primitives.
- Markdown export makes sessions inspectable and portable without introducing a database dependency yet.

## Iteration 9

Focus: introduce a structured provider stream event contract.

Changes:

- Added `StreamEvent` to core events.
- Provider responses now carry structured stream events in addition to legacy text deltas.
- Text responses emit message start, text delta, and message done events.
- Tool calls emit tool-call stream deltas with call identity and input deltas.
- Agent maps provider stream events into the public event list while preserving existing assistant deltas for compatibility.

Why this matters:

- TUI and RPC parity need structured stream events instead of raw strings.
- This creates a stable internal contract before implementing true network SSE.
