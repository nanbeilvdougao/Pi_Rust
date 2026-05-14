//! Credential storage for Pi Rust providers.
//!
//! Three layers, queried in this order:
//!
//! 1. Process environment (`<PROVIDER>_API_KEY` etc) — wins so power users
//!    can override anything without touching files.
//! 2. Platform keychain (`feature = "keyring"`, opt-in because it pulls
//!    OS-specific deps) — macOS Keychain, Windows Credential Manager, Linux
//!    Secret Service over D-Bus.
//! 3. Encrypted file at `~/.pi-rust/auth.enc`, ChaCha20-Poly1305 with a key
//!    derived from a machine secret (`/etc/machine-id`, `ioreg` on macOS,
//!    or a per-user random fallback persisted at `~/.pi-rust/.machine-id`).
//!
//! The encrypted-file backing is *obfuscation-grade* — better than plaintext
//! in `~/.bashrc` history but trivially recovered by anyone with read access
//! to both the file and the machine-id source. Real users should layer the
//! keychain feature on top. The split is intentional: encrypted-file works
//! offline, headless, and in CI without dbus.
//!
//! All operations are explicit: `pi auth set <provider>` prompts via stderr
//! and writes the credential; `pi auth list` shows names only; the runtime
//! `Resolver::lookup` is the read path.

use std::collections::BTreeMap;
use std::path::PathBuf;

use pi_core::{PiError, PiErrorKind, PiResult};
use serde::{Deserialize, Serialize};

#[cfg(feature = "encrypted-file")]
pub mod encrypted_file;
#[cfg(feature = "keyring")]
pub mod keyring_store;
pub mod oauth;
pub mod resolver;

pub use oauth::{OAuthConfig, OAuthTokens};

pub use resolver::{layered_resolver, EnvResolver, LayeredResolver, Resolver};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    pub provider: String,
    pub env_name: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CredentialFile {
    #[serde(default)]
    pub credentials: BTreeMap<String, String>,
}

pub fn default_auth_path() -> PiResult<PathBuf> {
    let home = std::env::var("HOME")
        .map_err(|_| PiError::new(PiErrorKind::Config, "无法读取 HOME 环境变量"))?;
    Ok(PathBuf::from(home).join(".pi-rust").join("auth.enc"))
}

/// Look up the canonical env-var name for a given provider id. Mirrors the
/// `requires_api_key_env` strings from `pi-providers` without depending on
/// that crate.
pub fn env_for_provider(provider: &str) -> Option<&'static str> {
    Some(match provider {
        "openai" => "OPENAI_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "moonshot" => "MOONSHOT_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "qwen" => "DASHSCOPE_API_KEY",
        "zhipu" => "ZHIPU_API_KEY",
        "minimax" => "MINIMAX_API_KEY",
        "gemini" => "GEMINI_API_KEY",
        _ => return None,
    })
}
