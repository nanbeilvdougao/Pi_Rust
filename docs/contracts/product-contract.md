# Product Contract

This document is the source of truth for the first Rust redesign milestone.

## Must Match

- CLI must support `--help`, `--version`, `--provider`, `--model`, `--list-providers`, `--print`, `--session`, `--continue`, and positional prompts.
- Sessions must be append-only and recoverable from a JSONL-like text format.
- Built-in tools must expose stable names and schemas: `read`, `write`, `edit`, `bash`, `search`, and `ls`.
- Provider streaming must be represented as ordered events even when the first implementation is non-streaming.
- File mutation and command execution must require an explicit capability decision.

## Intentional Differences

- Chinese output and diagnostics are first-class defaults.
- Local providers such as Ollama, Moonshot, DeepSeek, Qwen, and openEuler/epkg workflows are prioritized earlier than long-tail extension compatibility.
- The extension system starts with a stable ABI contract before choosing a JS, WASM, or native host.
- The core crates avoid runtime-heavy dependencies until a feature needs them.

## Future

- Full TypeScript extension compatibility.
- Rich TUI parity.
- SQLite/segmented session storage.
- Differential benchmark automation.
- Homebrew, cargo-binstall, offline installers, and rollback-aware self-update.

## Non Goals For MVP

- Line-for-line compatibility with any existing Rust port.
- Marketing performance claims without reproducible artifacts.
- A general-purpose plugin runtime before the hostcall security model is stable.
