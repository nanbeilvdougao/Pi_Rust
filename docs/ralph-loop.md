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
