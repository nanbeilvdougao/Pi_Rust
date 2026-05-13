# Extension ABI Plan

The extension system starts with an ABI contract instead of a runtime choice.

## Versioning

- MVP ABI: `1.0`
- Hostcalls must declare the capability they require.
- Breaking hostcall shape changes require a new major ABI.

## Hostcalls

- `Tool`: calls a named built-in tool and requires `extension_hostcall`.
- `SessionRead`: reads extension-visible session state and requires `session`.
- `SessionWrite`: writes extension-visible session state and requires `session`.
- `Http`: performs network access and requires `network`.
- `UiNotify`: sends a UI notification and requires `extension_hostcall`.

## Runtime Choice

JS, WASM, and native plugins remain implementation options. The first runtime must satisfy the same conformance cases in `pi-ext`, so the ABI survives runtime replacement.

## Security Baseline

- No extension can receive ambient filesystem, shell, network, or session access.
- Every hostcall is auditable.
- Denials should be explainable in Chinese and machine-readable events.
