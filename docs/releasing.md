# Releasing

## Release Targets

- Source build with Cargo.
- Single binary artifacts for macOS, Linux, and Windows.
- Future Homebrew and cargo-binstall metadata.
- Future offline archive with checksum and rollback instructions.

## Required Gates

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Evidence Requirements

- CLI contract tests must pass.
- Tool schema contract tests must pass.
- Provider registry contract tests must pass.
- Session format contract tests must pass.
- Extension ABI conformance tests must pass.

## Version Policy

- Patch: compatible bug fixes and provider metadata updates.
- Minor: additive CLI flags, tools, providers, or extension hostcalls.
- Major: breaking CLI, session, tool schema, or extension ABI changes.
