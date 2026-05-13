# Differential Suite

The differential suite compares Pi Rust against two references:

- TypeScript Pi for user-visible behavior.
- Existing Rust ports for risk discovery, not source compatibility.

## MVP Checks

| Check | Reference | Current Rust Evidence |
| --- | --- | --- |
| CLI flags | TypeScript `coding-agent` help surface | `crates/pi-cli/tests/cli_contract.rs` |
| Tool names | TypeScript built-in tools | `crates/pi-tools/tests/tool_contract.rs` |
| Session append shape | TypeScript JSONL sessions | `crates/pi-session/tests/session_contract.rs` |
| Provider registry | Local-first redesign | `crates/pi-providers/tests/provider_contract.rs` |
| Extension hostcall capability | Rust redesign ABI | `crates/pi-ext/tests/extension_contract.rs` |

## Classification

- `must-match`: behavior should stay compatible.
- `intentional-difference`: behavior differs because the redesign improves local, Chinese, security, or maintenance goals.
- `future`: explicitly out of MVP scope.

Every new compatibility issue must add or update a row in `parity-ledger.md`.
