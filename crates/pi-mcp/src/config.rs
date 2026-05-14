//! `.pi/mcp.toml` config loader.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use pi_core::{PiError, PiErrorKind, PiResult};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<ServerSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSpec {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl McpConfig {
    pub fn load_workspace(root: &Path) -> PiResult<Self> {
        let path = root.join(".pi").join("mcp.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)?;
        toml::from_str(&text).map_err(|err| {
            PiError::new(
                PiErrorKind::Config,
                format!("解析 .pi/mcp.toml 失败：{err}"),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn missing_config_is_empty() {
        let dir = tempdir().unwrap();
        let config = McpConfig::load_workspace(dir.path()).unwrap();
        assert!(config.servers.is_empty());
    }

    #[test]
    fn parses_servers_section() {
        let dir = tempdir().unwrap();
        let pi_dir = dir.path().join(".pi");
        fs::create_dir_all(&pi_dir).unwrap();
        fs::write(
            pi_dir.join("mcp.toml"),
            concat!(
                "[[servers]]\n",
                "id = \"files\"\n",
                "command = \"mcp-files\"\n",
                "args = [\"--root\", \".\"]\n",
                "\n",
                "[servers.env]\n",
                "LOG_LEVEL = \"info\"\n",
            ),
        )
        .unwrap();
        let config = McpConfig::load_workspace(dir.path()).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].id, "files");
        assert_eq!(config.servers[0].command, "mcp-files");
        assert_eq!(config.servers[0].args, vec!["--root", "."]);
        assert_eq!(
            config.servers[0].env.get("LOG_LEVEL").map(String::as_str),
            Some("info")
        );
    }
}
