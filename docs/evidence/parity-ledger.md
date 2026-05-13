# Parity Ledger

The parity ledger records behavior against the TypeScript Pi baseline and existing Rust implementations.

| Area | Status | Decision | Evidence |
| --- | --- | --- | --- |
| CLI help/version | must-match | MVP supports stable flags with simpler parser | `crates/pi-cli` |
| CLI doctor diagnostics | intentional-difference | `pi doctor` reports local toolchain, curl, session path, and provider credential status | `crates/pi-cli` |
| CLI model listing | must-match | `--list-models` lists provider model presets | `crates/pi-cli`, `crates/pi-providers` |
| CLI tool listing and selection | must-match | `--list-tools` and `--tools` expose and restrict built-in tool schemas | `crates/pi-cli`, `crates/pi-tools` |
| Provider registry | intentional-difference | Chinese/local providers are listed from the start | `crates/pi-providers` |
| Ollama provider execution | must-match-shape | Local `/api/chat` execution path exists; streaming is future work | `crates/pi-providers` |
| OpenAI-compatible domestic providers | intentional-difference | Moonshot, DeepSeek, and Qwen have real chat-completions execution paths via their OpenAI-compatible APIs | `crates/pi-providers` |
| Provider-driven tool calls | must-match-shape | OpenAI-compatible providers receive tool schemas, parse function tool calls, and the agent loops tool results back to the provider | `crates/pi-providers`, `crates/pi-agent` |
| Provider stream event model | must-match-shape | Providers emit structured message/tool stream events and the agent maps text deltas into assistant deltas; true network SSE remains future work | `crates/pi-core`, `crates/pi-providers`, `crates/pi-agent` |
| Structured tool result messages | must-match-shape | Tool results preserve `tool_call_id` and serialize as OpenAI-compatible `role=tool` messages when available | `crates/pi-core`, `crates/pi-agent`, `crates/pi-providers`, `crates/pi-session` |
| Session format and discovery | must-match-shape | Append-only JSONL-like lines with `--continue`, `--resume <ID>`, `--session <ID>`, `--list-sessions`, `--delete-session`, `--rename-session`, and `--export-session`; schema hardening remains future work | `crates/pi-session`, `crates/pi-cli` |
| Tool permissions | intentional-difference | Mutating tools require capability checks by default | `crates/pi-permissions`, `crates/pi-tools` |
| Extension runtime | future | ABI first, JS/WASM/native host later | `crates/pi-ext` |
| TUI | future | Event boundary first, rich terminal UI later | `crates/pi-tui` |
| Benchmark claims | future | No numeric claims until reproducible harness exists | `docs/contracts/product-contract.md` |
