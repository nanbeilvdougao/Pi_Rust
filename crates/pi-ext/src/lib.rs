//! Extension ABI v1.
//!
//! Two parallel surfaces:
//!
//! 1. **Data model** (this module): `ExtensionManifest`, `Hostcall`,
//!    `Capability` mapping. Stable across host implementations.
//! 2. **Process host** ([`process`]): a concrete extension runner that spawns
//!    a child process and exchanges JSON-RPC messages over stdio. Each
//!    hostcall round-trips through `pi_permissions::PermissionEngine`, so even
//!    a fully native extension cannot bypass the capability map.
//!
//! Why a process host instead of WASM right now: it is portable to all targets
//! including older Linux, supports `epkg` / `apt` / shell-style extensions out
//! of the box, and keeps the `unsafe_code = forbid` lint happy. A wasmtime
//! host can be added later behind a feature flag without changing the manifest
//! format or the hostcall surface.

use std::path::{Path, PathBuf};

use pi_permissions::Capability;
use serde::{Deserialize, Serialize};

pub mod process;
#[cfg(feature = "wasm")]
pub mod wasm;

pub use process::{ExtensionHost, ExtensionInstance, HostcallReply};
#[cfg(feature = "wasm")]
pub use wasm::WasmExtensionHost;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbiVersion {
    pub major: u16,
    pub minor: u16,
}

impl AbiVersion {
    pub const V1: Self = Self { major: 1, minor: 0 };
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default = "default_abi")]
    pub abi: AbiVersion,
    pub entry: ExtensionEntry,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub description: Option<String>,
}

fn default_abi() -> AbiVersion {
    AbiVersion::V1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExtensionEntry {
    /// Spawn an executable file. `args` and env are passed verbatim.
    Process {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
    /// Shell command (uses `sh -c`). Easier for scripted extensions.
    Shell { command: String },
    /// WASM module (path relative to manifest dir). The extension speaks the
    /// same line-delimited JSON-RPC protocol as the process host, just over
    /// piped wasi stdio.
    Wasm {
        path: String,
        #[serde(default)]
        env: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Hostcall {
    Tool { name: String, input: String },
    SessionRead { key: String },
    SessionWrite { key: String, value: String },
    Http { method: String, url: String },
    UiNotify { message: String },
}

impl Hostcall {
    pub fn required_capability(&self) -> Capability {
        match self {
            Self::Tool { .. } => Capability::ExtensionHostcall,
            Self::SessionRead { .. } | Self::SessionWrite { .. } => Capability::Session,
            Self::Http { .. } => Capability::Network,
            Self::UiNotify { .. } => Capability::ExtensionHostcall,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionConformanceCase {
    pub id: String,
    pub hostcall: Hostcall,
    pub expected_capability: Capability,
}

pub fn mvp_conformance_cases() -> Vec<ExtensionConformanceCase> {
    vec![
        ExtensionConformanceCase {
            id: "tool-hostcall-requires-extension-capability".to_string(),
            hostcall: Hostcall::Tool {
                name: "read".to_string(),
                input: "README.md".to_string(),
            },
            expected_capability: Capability::ExtensionHostcall,
        },
        ExtensionConformanceCase {
            id: "http-hostcall-requires-network".to_string(),
            hostcall: Hostcall::Http {
                method: "GET".to_string(),
                url: "https://example.com".to_string(),
            },
            expected_capability: Capability::Network,
        },
        ExtensionConformanceCase {
            id: "session-write-requires-session".to_string(),
            hostcall: Hostcall::SessionWrite {
                key: "k".to_string(),
                value: "v".to_string(),
            },
            expected_capability: Capability::Session,
        },
    ]
}

/// Loads a manifest from `<dir>/extension.toml` (or `<file>` if the path is a
/// regular file). Returns the manifest plus the directory it was found in,
/// which is treated as the extension's root.
pub fn load_manifest(path: impl AsRef<Path>) -> Result<(ExtensionManifest, PathBuf), String> {
    let path = path.as_ref();
    let (text, root) = if path.is_dir() {
        let manifest_path = path.join("extension.toml");
        let text = std::fs::read_to_string(&manifest_path)
            .map_err(|err| format!("读取 {} 失败：{err}", manifest_path.display()))?;
        (text, path.to_path_buf())
    } else {
        let text = std::fs::read_to_string(path)
            .map_err(|err| format!("读取 {} 失败：{err}", path.display()))?;
        let root = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        (text, root)
    };
    let manifest: ExtensionManifest =
        toml::from_str(&text).map_err(|err| format!("解析 manifest 失败：{err}"))?;
    Ok((manifest, root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_through_toml() {
        let manifest = ExtensionManifest {
            id: "demo".to_string(),
            name: "Demo".to_string(),
            version: "0.1.0".to_string(),
            abi: AbiVersion::V1,
            entry: ExtensionEntry::Shell {
                command: "echo hi".to_string(),
            },
            capabilities: vec![Capability::ExtensionHostcall],
            description: Some("demo extension".to_string()),
        };
        let text = toml::to_string(&manifest).expect("serialize");
        let decoded: ExtensionManifest = toml::from_str(&text).expect("deserialize");
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn hostcalls_map_to_capabilities_consistently() {
        for case in mvp_conformance_cases() {
            assert_eq!(
                case.hostcall.required_capability(),
                case.expected_capability
            );
        }
    }
}
