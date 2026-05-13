# Parity Ledger

The parity ledger records behavior against the TypeScript Pi baseline and existing Rust implementations.

| Area | Status | Decision | Evidence |
| --- | --- | --- | --- |
| CLI help/version | must-match | MVP supports stable flags with simpler parser | `crates/pi-cli` |
| Provider registry | intentional-difference | Chinese/local providers are listed from the start | `crates/pi-providers` |
| Session format | must-match-shape | Append-only JSONL-like lines, schema to harden later | `crates/pi-session` |
| Tool permissions | intentional-difference | Mutating tools require capability checks by default | `crates/pi-permissions`, `crates/pi-tools` |
| Extension runtime | future | ABI first, JS/WASM/native host later | `crates/pi-ext` |
| TUI | future | Event boundary first, rich terminal UI later | `crates/pi-tui` |
| Benchmark claims | future | No numeric claims until reproducible harness exists | `docs/contracts/product-contract.md` |
