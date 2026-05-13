# Architecture

Pi Rust uses a small core with explicit capability boundaries.

```text
pi-cli -> pi-agent -> pi-providers
                  -> pi-tools -> pi-permissions
                  -> pi-session
pi-tui -> pi-core events
pi-ext -> pi-permissions capability map
```

## Rules

- `pi-core` must not depend on any other local crate.
- `pi-cli` is composition only; product logic belongs in library crates.
- Tools and extensions cannot bypass `pi-permissions`.
- Provider implementations return ordered response events so streaming can be added without changing the agent API.
- Session storage is append-first. Faster indexes or SQLite stores must preserve recoverability.

## MVP Data Flow

1. `pi-cli` parses arguments into `AppConfig`.
2. `pi-agent` loads the requested session from `pi-session`.
3. The user prompt is appended before execution.
4. `/tool <name> <input>` prompts are routed to `pi-tools`; all mutating operations go through `pi-permissions`.
5. Normal prompts are routed to `pi-providers`.
6. Assistant/tool output is appended and emitted as `pi-core::Event`.
