//! Platform keychain backend (feature `keyring`).
//!
//! Names are stored under a single service `pi-rust` keyed by env-var name
//! (`OPENAI_API_KEY`, `MOONSHOT_API_KEY`, …) so the surface matches the
//! encrypted-file backend exactly. `list()` returns the env names this
//! process has seen — keyring crate does not enumerate items, so we maintain
//! a small companion index file at `~/.pi-rust/keyring-index.json`.

use std::fs;
use std::path::PathBuf;

use keyring::Entry;
use pi_core::{PiError, PiErrorKind, PiResult};
use serde::{Deserialize, Serialize};

use crate::Resolver;

const SERVICE: &str = "pi-rust";

#[derive(Default, Serialize, Deserialize)]
struct Index {
    env_names: Vec<String>,
}

pub struct KeyringStore {
    index_path: PathBuf,
    index: Index,
}

impl KeyringStore {
    pub fn new() -> PiResult<Self> {
        let home = std::env::var("HOME")
            .map_err(|_| PiError::new(PiErrorKind::Config, "无法读取 HOME 环境变量"))?;
        let index_path = PathBuf::from(home)
            .join(".pi-rust")
            .join("keyring-index.json");
        let index = if let Ok(text) = fs::read_to_string(&index_path) {
            serde_json::from_str(&text).unwrap_or_default()
        } else {
            Index::default()
        };
        Ok(Self { index_path, index })
    }

    fn save_index(&self) -> PiResult<()> {
        if let Some(parent) = self.index_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string(&self.index)
            .map_err(|err| PiError::new(PiErrorKind::Config, format!("序列化索引失败：{err}")))?;
        fs::write(&self.index_path, text)?;
        Ok(())
    }
}

impl Resolver for KeyringStore {
    fn lookup(&self, _provider: &str, env_name: &str) -> PiResult<Option<String>> {
        let entry = match Entry::new(SERVICE, env_name) {
            Ok(entry) => entry,
            Err(_) => return Ok(None),
        };
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Ok(None),
        }
    }

    fn store(&mut self, _provider: &str, env_name: &str, value: &str) -> PiResult<()> {
        let entry = Entry::new(SERVICE, env_name).map_err(|err| {
            PiError::new(PiErrorKind::Config, format!("打开 keyring 项失败：{err}"))
        })?;
        entry.set_password(value).map_err(|err| {
            PiError::new(PiErrorKind::Config, format!("写入 keyring 失败：{err}"))
        })?;
        if !self.index.env_names.iter().any(|n| n == env_name) {
            self.index.env_names.push(env_name.to_string());
            self.save_index()?;
        }
        Ok(())
    }

    fn delete(&mut self, _provider: &str, env_name: &str) -> PiResult<bool> {
        let entry = match Entry::new(SERVICE, env_name) {
            Ok(entry) => entry,
            Err(_) => return Ok(false),
        };
        let existed = matches!(
            entry.delete_credential(),
            Ok(()) | Err(keyring::Error::NoEntry)
        );
        self.index.env_names.retain(|n| n != env_name);
        let _ = self.save_index();
        Ok(existed)
    }

    fn list(&self) -> PiResult<Vec<String>> {
        Ok(self.index.env_names.clone())
    }
}
