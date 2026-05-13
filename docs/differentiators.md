# Differentiators

## Chinese-First Experience

- Default diagnostics are Simplified Chinese.
- Built-in provider registry includes Moonshot, DeepSeek, Qwen, Ollama, and local echo.
- CLI help is written for local users before translation.

## Local And Offline Workflows

- `echo` provider keeps the CLI testable without network or API keys.
- Ollama appears as a first-class provider target.
- Installation guidance keeps single-binary, cargo, and future offline package flows separate.

## openEuler And epkg Direction

The MVP does not shell out to epkg yet. The tool boundary reserves this as an optional capability so package management can later be enabled behind explicit permissions instead of becoming a default shell escape hatch.

## Evidence Over Claims

No numeric performance claim is release-facing until it has:

- a command that regenerates the result,
- checked-in raw artifacts,
- environment metadata,
- and a parity note explaining what was compared.
