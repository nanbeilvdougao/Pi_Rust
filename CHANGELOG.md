# Changelog

All notable changes to Pi Rust are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); this project
adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Azure OpenAI provider (`pi --provider azure`) with `api-key` and AAD
  bearer auth via `AZURE_OPENAI_USE_AAD`.
- Google Vertex AI provider (`pi --provider vertex`) reusing the Gemini
  body shape but signing with `VERTEX_ACCESS_TOKEN` and routing through
  `v1/projects/{p}/locations/{r}/publishers/google/models/{m}`.
- GitHub Copilot provider (`pi --provider copilot`) with Copilot-specific
  Editor / Plugin / Integration headers and `GITHUB_COPILOT_TOKEN`.
- OpenRouter provider (`pi --provider openrouter`) — straight OpenAI-
  compat against `openrouter.ai/api/v1`.
- Mistral provider (`pi --provider mistral`) — `api.mistral.ai/v1`.
- Cloudflare Workers AI provider (`pi --provider cloudflare`) — runs
  models at `accounts/{id}/ai/run/{model}`.
- OpenAI Responses API provider (`pi --provider openai-responses`) — the
  newer `/v1/responses` endpoint with typed SSE events
  (`response.output_text.delta`, `response.completed`).
- `pi --file <path>` and `pi --message-file <path>` flags: append file
  contents to the first user message (multi-file supported) or replace
  the prompt outright.
- Provider-specific auth-guidance: `pi_core::auth_guidance_for` returns
  per-provider recovery steps (rotate URLs, env-var requirements, OAuth
  client-id hints). Surfaced automatically on `Provider` errors that
  look like auth failures (401/403/"缺少凭证"/"expired").
- This `CHANGELOG.md` (backfilled from `git log`).

## [0.1.0] - 2026-05-14 - "Full parity"

### Added
- Multi-edit tool: atomic multi-replace with pre-flight verification and
  full rollback on any miss.
- File-mutation queue: per-path mutex + snapshot/commit guard that
  detects external modifications between read and write.
- Anthropic OAuth login flow with PKCE, loopback callback server, and
  token storage via `pi-auth`. `PI_OAUTH_CLIENT_ID_<PROVIDER>` selects
  the registered client.
- AWS Bedrock provider with a hand-rolled SigV4 signer (RFC 4231 test
  vector verified) and Anthropic-on-Bedrock `invokeModel` routing.
- `image_generate` tool (OpenAI `images/generations` + Imagen
  `:predict`) — saves PNG bytes under Network + WriteFile capabilities.
- Interactive session picker (`pi sessions` / `pi --pick`) — ratatui
  list, arrow keys, Enter / n / Esc.
- First-run config wizard on TTY when `~/.pi-rust/config.toml` is
  missing.
- `source_info`: git branch / commit / dirty flag / remote URL + package
  manager detection (cargo / npm / pnpm / yarn / bun / pip / poetry /
  uv / go / maven / gradle) injected into the system prompt.
- TUI footer overhaul: git branch+commit+dirty, provider/model, token
  usage progress bar, last-error class.
- `pi --trace path.json`: chrome://tracing JSON emitter for provider /
  tool / compaction spans.
- Opt-in anonymous telemetry. Default off, `PI_TELEMETRY=1` opts in,
  `DO_NOT_TRACK=1` always wins. Events carry only version, command,
  error kind, and bucketed duration.
- TS pi `.pi/` directory interop: v3 JSONL header (camelCase, RFC3339
  timestamp, `parentSession`) parses transparently; `.pi/config.json`
  reads alongside `.pi/config.toml`.
- Extension wrapper (`pi_ext::ToolBridge`): bridges tool runtime +
  MCP-registered tools to extensions as `Hostcall::Tool`.
- Cross-platform release workflow (`.github/workflows/release.yml`):
  five binaries (`pi-{x86_64,aarch64}-{linux,macos,windows}`) with
  sha256 sidecars, matching `pi --self-update`.
- 20-case TS-pi regression suite under
  `crates/pi-agent/tests/ts_pi_regressions.rs`.

### Changed
- `comparative-bench` now ships three-way numbers vs pi_agent_rust and
  pi-rs. Pi Rust leads on SSE parsing (4.9×), output truncation (3.7×),
  and cold-start (0.44 s vs pi-rs 0.57 s vs pi_agent_rust 0.76 s).

## [0.0.3] - 2026-05-13 - "Provider streaming"

### Added
- Structured provider stream events (`MessageStart`, `TextDelta`,
  `ThinkingDelta`, `ToolCallDelta`, `UsageDelta`, `MessageDone`) used
  by every provider.
- Session lifecycle commands (`--delete-session`, `--rename-session`,
  `--export-session`).
- Provider-driven tool calls with structured tool_result identity
  (`tool_call_id` preserved across turns).

### Changed
- All provider modules moved into `crates/pi-providers/src/<provider>.rs`.

## [0.0.2] - 2026-05-13 - "Initial CLI"

### Added
- CLI doctor diagnostics (`pi doctor`).
- Domestic OpenAI-compatible providers (Moonshot, DeepSeek, Qwen).
- Tool discovery via `pi --list-tools`.
- Ollama provider execution path.

## [0.0.1] - 2026-05-13 - "Skeleton"

### Added
- Initial workspace layout: `pi-core` / `pi-agent` / `pi-providers` /
  `pi-tools` / `pi-permissions` / `pi-session` / `pi-tui` / `pi-ext` /
  `pi-cli`.
- Echo provider + basic JSONL session store + permission engine
  scaffolding.

[Unreleased]: https://github.com/nanbeilvdougao/Pi_Rust/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/nanbeilvdougao/Pi_Rust/releases/tag/v0.1.0
[0.0.3]: https://github.com/nanbeilvdougao/Pi_Rust/releases/tag/v0.0.3
[0.0.2]: https://github.com/nanbeilvdougao/Pi_Rust/releases/tag/v0.0.2
[0.0.1]: https://github.com/nanbeilvdougao/Pi_Rust/releases/tag/v0.0.1
