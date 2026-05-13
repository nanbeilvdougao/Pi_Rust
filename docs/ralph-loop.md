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
