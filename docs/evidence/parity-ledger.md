# Parity Ledger

The parity ledger records behavior against the TypeScript Pi baseline and existing Rust implementations (`pi_agent_rust`, `pi-rs`).

Status legend:

- `must-match` — behavior should stay compatible with TS pi.
- `intentional-difference` — behavior differs because the redesign improves
  local, Chinese, security, or maintenance goals.
- `must-match-shape` — wire/format shape matches but the surface is smaller or
  larger than TS pi on purpose.
- `future` — explicitly out of MVP scope.

| Area | Status | Decision | Evidence |
| --- | --- | --- | --- |
| CLI help/version | must-match | clap-based parser with both `pi --help` and legacy `pi doctor` / `pi sessions` subcommands | `crates/pi-cli` |
| CLI doctor diagnostics | intentional-difference | `pi doctor` reports cargo, rustc, curl, session path, all provider credential envs, rust version, pi version | `crates/pi-cli` |
| CLI model listing | must-match | `--list-models` lists provider model presets including defaults marker | `crates/pi-cli`, `crates/pi-providers` |
| CLI tool listing and selection | must-match | `--list-tools` and `--tools` expose JSON-Schema parameters | `crates/pi-cli`, `crates/pi-tools` |
| CLI permission mode | intentional-difference | `--permission read-only|confirm|trusted|plan` selects the engine mode at startup | `crates/pi-cli`, `crates/pi-permissions` |
| CLI locale | intentional-difference | `--locale zh-cn|en` switches the default system prompt language | `crates/pi-cli`, `crates/pi-agent` |
| CLI interactive TUI | must-match-shape | `pi` (no prompt) or `-i` launches a ratatui+crossterm TUI with streaming, history, Ctrl+C interrupt, Ctrl+L clear, Ctrl+J multi-line | `crates/pi-tui`, `crates/pi-cli` |
| CLI RPC mode | must-match-shape | `pi --rpc` runs a JSON-RPC stdio loop: `health`, `list_providers`, `list_tools`, `complete`, `shutdown` | `crates/pi-cli/src/rpc.rs` |
| Persistent settings | must-match-shape | TOML layered settings: `~/.pi-rust/config.toml` (user) then `<workspace>/.pi/config.toml` (workspace); CLI flags win | `crates/pi-agent/src/settings.rs` |
| Skills system | must-match-shape | `<workspace>/.pi/skills/*.md` with optional TOML frontmatter; `always` skills inject into the system prompt; `keyword`/`command` triggers exposed | `crates/pi-agent/src/skills.rs` |
| Custom slash commands | must-match-shape | `<workspace>/.pi/commands/<name>.md` registers `/<name>` with `{{argument}}` substitution | `crates/pi-agent/src/slash.rs` |
| Provider registry | intentional-difference | Chinese providers (Moonshot, DeepSeek, Qwen/DashScope, Zhipu GLM, MiniMax) are first-class alongside OpenAI, Anthropic, Gemini, Ollama, and echo | `crates/pi-providers` |
| Ollama provider execution | must-match | Real `/api/chat` execution path with NDJSON streaming, structured tool calls, usage tokens | `crates/pi-providers/src/ollama.rs` |
| OpenAI-compatible providers | must-match-shape | Real `chat/completions` (non-streaming) and SSE streaming paths, with `tool_calls`, `reasoning_content` (DeepSeek), `tool_choice=auto`, `stream_options.include_usage` | `crates/pi-providers/src/openai.rs` |
| Anthropic provider | must-match-shape | Real `v1/messages` API, SSE streaming, tool_use blocks, thinking_delta, prompt caching usage fields | `crates/pi-providers/src/anthropic.rs` |
| Gemini provider | must-match-shape | Real `v1beta/models/:generateContent` and `:streamGenerateContent?alt=sse`, function calling, usage metadata | `crates/pi-providers/src/gemini.rs` |
| Provider stream event model | must-match-shape | Unified `StreamEvent::{MessageStart,TextDelta,ThinkingDelta,ToolCallDelta,UsageDelta,MessageDone}` for every provider | `crates/pi-core`, `crates/pi-providers`, `crates/pi-agent` |
| Provider-driven tool calls | must-match | Multi-round tool loop: assistant tool-call turn is persisted before the tool result, matching OpenAI/Anthropic replay requirements | `crates/pi-agent` |
| System prompt | must-match-shape | Default prompt assembled from locale, cwd, os, arch, date, provider/model, the live tool inventory, plus always-on skills | `crates/pi-agent/src/system_prompt.rs`, `crates/pi-agent/src/skills.rs` |
| Context compaction | must-match-shape | Triggers when estimated tokens exceed `context_window_tokens * compaction_threshold`; keeps head (system + first user) and last N user/assistant pairs intact, summarizes the middle through the same provider | `crates/pi-agent/src/compaction.rs` |
| Slash commands (built-in) | must-match | `/help`, `/clear`, `/sessions`, `/model`, `/tools`, `/compact`, `/permission`, `/quit` handled in-agent before provider routing | `crates/pi-agent/src/slash.rs` |
| Session format and discovery | must-match-shape | Append-only JSONL with serde-derived schema, sanitized session ids, `--continue`, `--resume <ID>`, `--session <ID>`, `--list-sessions` (showing last user excerpt), `--delete-session`, `--rename-session`, `--export-session`, `--export-json` | `crates/pi-session`, `crates/pi-cli` |
| Tool permissions | intentional-difference | Mutating tools, network, and extension hostcalls all gate through `pi-permissions::PermissionEngine`, with dangerous-target blocklist and audit log | `crates/pi-permissions`, `crates/pi-tools` |
| Built-in tools surface | must-match-shape | `read` (with offset/limit/line_numbers), `write` (with create_dirs), `edit` (unique-match + diff preview + replace_all), `bash` (cwd, timeout_ms, output truncation), `search`, `grep` (regex, case_sensitive), `find` (glob), `ls`, `epkg` | `crates/pi-tools` |
| Tool input dual mode | intentional-difference | Each tool accepts both structured JSON `parameters` (from provider tool calls) and a legacy plain-string form (for `/tool` shortcuts) | `crates/pi-tools` |
| Output truncation | must-match-shape | Bash and other unbounded tools cap output by both bytes and lines and emit a `... <n> lines / <m> bytes elided ...` separator | `crates/pi-tools/src/truncate.rs` |
| Multimodal attachments | must-match-shape | `Message::attachments` carries base64 or URL image attachments. Mapped to OpenAI `image_url`, Anthropic `image`, Gemini `inlineData`/`fileData`. Ctrl+V in the TUI pastes images straight from the clipboard into the next turn | `crates/pi-core`, `crates/pi-providers`, `crates/pi-tui/src/clipboard.rs` |
| Model alias resolution | must-match | `pi --model sonnet` (or any provider/model literal) resolves through `pi_providers::resolve_alias`; `pi --list-aliases` prints the table | `crates/pi-providers/src/aliases.rs`, `crates/pi-cli/src/main.rs` |
| TUI autocomplete + `@` file references | must-match-shape | Tab opens a completion popup for slash commands or fuzzy file paths; `@path` expands inline into the prompt with file contents attached | `crates/pi-tui/src/completion.rs` |
| Clipboard image paste | must-match-shape | Ctrl+V reads the system clipboard via `arboard`, encodes images as inline PNG (zero-deps hand-rolled writer), attaches them to the next message | `crates/pi-tui/src/clipboard.rs` |
| Configurable keybindings | must-match-shape | All TUI actions go through `KeyBindings`; `~/.pi-rust/keybindings.toml` and `<workspace>/.pi/keybindings.toml` layer overrides; chord parser supports Ctrl/Alt/Shift/Super | `crates/pi-tui/src/keybindings.rs` |
| Encrypted credential storage | must-match-shape | `pi auth set/list/remove` writes ChaCha20-Poly1305 encrypted TOML under `~/.pi-rust/auth.enc`; optional `keyring` feature plugs in macOS Keychain / Linux Secret Service / Windows Credential Manager. Process env still wins over disk | `crates/pi-auth` |
| Session-level cwd | must-match-shape | First line of a `.jsonl` session is a `SessionHeader { type: "session", version: 2, cwd, created_ms, … }`; `--resume` chdir's into the recorded cwd if it still exists | `crates/pi-session` |
| TUI theme system | must-match-shape | `dark` / `light` / `solarized` built-ins; `~/.pi-rust/theme.toml` and workspace overrides; `--theme` flag chooses base; named ANSI and `#rrggbb` colors | `crates/pi-tui/src/theme.rs` |
| HTML session export | must-match | `pi --export-html <id>` produces a single self-contained HTML page (inline CSS, folded tool outputs, no external CDN) | `crates/pi-session/src/export_html.rs` |
| Localized error hints | must-match-shape | `pi_core::hint_for` maps every `PiErrorKind` (plus message keywords) to zh-CN / en summaries with bullet-pointed next steps; CLI appends them to stderr | `crates/pi-core/src/error_hints.rs` |
| Provider health probe | must-match-shape | `pi --probe` (alone or with `pi doctor`) issues a cheap credentialed read against every configured provider and reports `ok` / `auth_fail` / `unreachable` / `missing_credential` / `unsupported` | `crates/pi-providers/src/probe.rs` |
| Agent harness / faux provider | must-match-shape | `pi_providers::FauxProvider` plus `register_test_provider` give the agent loop a deterministic stand-in with scripted turns and request recording; used by the agent test suite | `crates/pi-providers/src/faux.rs`, `crates/pi-agent/tests/faux_harness.rs` |
| MCP server integration | must-match-shape | `.pi/mcp.toml` declares servers; `pi_mcp::McpManager` spawns them, drives JSON-RPC 2.0 over stdio (`initialize` + `tools/list` + `tools/call`), and registers every remote tool into `pi-tools::ToolRuntime` as a first-class capability-gated tool | `crates/pi-mcp` |
| WASM extension full-duplex RPC | must-match-shape | Custom WASI `BlockingStdin` lets the guest read host replies between writes; guest runs on a worker thread while the host streams stdout, dispatches hostcalls, and feeds replies back without quitting the module | `crates/pi-ext/src/wasm.rs` |
| Comparative benches (session / tool / agent) | intentional-difference | New `pi-bench` benches for session_save (2.18 ms / 100 msgs), tool_dispatch (9.6 µs read), and agent_loop (177 µs faux round-trip), captured under `docs/evidence/comparative-bench.md` | `crates/pi-bench/benches/{session_save,tool_dispatch,agent_loop}.rs` |
| Extension runtime | must-match-shape | Process-backed JSON-RPC host (`pi-ext::process::ExtensionHost`) with capability-gated hostcalls. WASM host is future work but the ABI is stable | `crates/pi-ext` |
| Streaming cancellation | intentional-difference | Cooperative `Arc<AtomicBool>` cancel handle; TUI Ctrl+C flips it; provider stream sinks check `cancelled()` between events | `crates/pi-core`, `crates/pi-agent`, `crates/pi-tui` |
| Benchmark harness | intentional-difference | `pi-bench` crate exposes criterion benches for session append, JSON serialize, token estimate, and the comparable workloads (SSE parse, output truncation) | `crates/pi-bench` |
| Comparative benchmarks | intentional-difference | Side-by-side numbers vs pi_agent_rust captured under `docs/evidence/comparative-bench.md`. Pi Rust currently 4.8–5.0× faster on SSE parsing and 2.1–3.7× faster on truncation at 10 KB+ outputs | `docs/evidence/comparative-bench.md`, `crates/pi-bench/benches/comparable.rs` |
| WASM extension host | intentional-difference | `pi-ext` ships a wasmtime + wasi-preview-1 host behind feature `wasm`. Same JSON-RPC protocol as the process host; capability gates apply | `crates/pi-ext/src/wasm.rs` |
| SQLite session index | intentional-difference | Optional `sqlite-index` feature mirrors JSONL into a SQLite database with FTS5 full-text search; JSONL stays canonical | `crates/pi-session/src/sqlite_index.rs` |
| Self-update | must-match-shape | `pi --version-check` queries GitHub Releases; `pi --self-update --yes` atomically swaps the running binary with the matching release asset | `crates/pi-cli/src/update.rs` |
