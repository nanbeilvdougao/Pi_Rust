//! Persistent settings loaded from `.pi/config.toml` (workspace) and
//! `~/.pi-rust/config.toml` (user). Workspace settings win over user
//! settings; CLI flags win over both. Mirrors how TS pi stratifies its
//! `settings.json` files.
//!
//! Settings are intentionally a narrow subset of `AppConfig`. We do not
//! persist secrets — those live in environment variables.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use pi_core::{
    AppConfig, Locale, ModelSelection, PermissionModeKind, PiError, PiErrorKind, PiResult,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedSettings {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub locale: Option<Locale>,
    #[serde(default)]
    pub permission_mode: Option<PermissionModeKind>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub tools_enabled: Option<bool>,
    #[serde(default)]
    pub enabled_tool_names: Option<Vec<String>>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub max_tool_steps: Option<u32>,
    #[serde(default)]
    pub context_window_tokens: Option<u32>,
    #[serde(default)]
    pub compaction_threshold: Option<f32>,
}

impl PersistedSettings {
    pub fn load_layered(workspace_root: Option<&Path>) -> Self {
        let mut merged = PersistedSettings::default();
        if let Some(home) = user_settings_path() {
            if let Some(loaded) = read_settings(&home) {
                merged = merge(merged, loaded);
            }
            // Also try ~/.pi-rust/config.json for TS-pi-style users.
            let home_json = home.with_extension("json");
            if let Some(loaded) = read_json_settings(&home_json) {
                merged = merge(merged, loaded);
            }
        }
        if let Some(root) = workspace_root {
            let workspace_toml = root.join(".pi").join("config.toml");
            if let Some(loaded) = read_settings(&workspace_toml) {
                merged = merge(merged, loaded);
            }
            // TS pi writes `.pi/config.json`; accept it transparently.
            let workspace_json = root.join(".pi").join("config.json");
            if let Some(loaded) = read_json_settings(&workspace_json) {
                merged = merge(merged, loaded);
            }
        }
        merged
    }

    pub fn apply_to(&self, config: &mut AppConfig) {
        if let Some(provider) = &self.provider {
            config.model.provider = provider.clone();
        }
        if let Some(model) = &self.model {
            config.model.model = model.clone();
        }
        if let Some(locale) = self.locale {
            config.locale = locale;
        }
        if let Some(mode) = self.permission_mode {
            config.permission_mode = mode;
        }
        if let Some(stream) = self.stream {
            config.stream = stream;
        }
        if let Some(enabled) = self.tools_enabled {
            config.tools_enabled = enabled;
        }
        if let Some(names) = &self.enabled_tool_names {
            config.enabled_tool_names = Some(names.clone());
        }
        if let Some(prompt) = &self.system_prompt {
            config.system_prompt = Some(prompt.clone());
        }
        if let Some(steps) = self.max_tool_steps {
            config.max_tool_steps = steps;
        }
        if let Some(window) = self.context_window_tokens {
            config.context_window_tokens = window;
        }
        if let Some(threshold) = self.compaction_threshold {
            config.compaction_threshold = threshold;
        }
    }
}

fn merge(mut base: PersistedSettings, top: PersistedSettings) -> PersistedSettings {
    if top.provider.is_some() {
        base.provider = top.provider;
    }
    if top.model.is_some() {
        base.model = top.model;
    }
    if top.locale.is_some() {
        base.locale = top.locale;
    }
    if top.permission_mode.is_some() {
        base.permission_mode = top.permission_mode;
    }
    if top.stream.is_some() {
        base.stream = top.stream;
    }
    if top.tools_enabled.is_some() {
        base.tools_enabled = top.tools_enabled;
    }
    if top.enabled_tool_names.is_some() {
        base.enabled_tool_names = top.enabled_tool_names;
    }
    if top.system_prompt.is_some() {
        base.system_prompt = top.system_prompt;
    }
    if top.max_tool_steps.is_some() {
        base.max_tool_steps = top.max_tool_steps;
    }
    if top.context_window_tokens.is_some() {
        base.context_window_tokens = top.context_window_tokens;
    }
    if top.compaction_threshold.is_some() {
        base.compaction_threshold = top.compaction_threshold;
    }
    base
}

fn user_settings_path() -> Option<PathBuf> {
    env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".pi-rust").join("config.toml"))
}

fn read_settings(path: &Path) -> Option<PersistedSettings> {
    let text = fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

fn read_json_settings(path: &Path) -> Option<PersistedSettings> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Save user-level settings. The TOML format keeps things human-editable.
pub fn save_user_settings(settings: &PersistedSettings) -> PiResult<PathBuf> {
    let path = user_settings_path()
        .ok_or_else(|| PiError::new(PiErrorKind::Config, "无法读取 HOME 环境变量"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string(settings)
        .map_err(|err| PiError::new(PiErrorKind::Config, format!("序列化设置失败：{err}")))?;
    fs::write(&path, text)?;
    Ok(path)
}

/// Helper for migrating `ModelSelection` from settings without copying provider
/// strings around.
pub fn model_from_settings(
    settings: &PersistedSettings,
    fallback: ModelSelection,
) -> ModelSelection {
    ModelSelection {
        provider: settings.provider.clone().unwrap_or(fallback.provider),
        model: settings.model.clone().unwrap_or(fallback.model),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{AppConfig, Locale, PermissionModeKind};

    #[test]
    fn apply_overrides_appconfig() {
        let mut config = AppConfig::default();
        let settings = PersistedSettings {
            provider: Some("anthropic".into()),
            model: Some("claude-opus-4-7".into()),
            locale: Some(Locale::En),
            permission_mode: Some(PermissionModeKind::TrustedWorkspace),
            stream: Some(false),
            tools_enabled: Some(false),
            enabled_tool_names: Some(vec!["read".into()]),
            system_prompt: Some("custom".into()),
            max_tool_steps: Some(32),
            context_window_tokens: Some(200_000),
            compaction_threshold: Some(0.7),
        };
        settings.apply_to(&mut config);
        assert_eq!(config.model.provider, "anthropic");
        assert_eq!(config.model.model, "claude-opus-4-7");
        assert!(matches!(config.locale, Locale::En));
        assert!(matches!(
            config.permission_mode,
            PermissionModeKind::TrustedWorkspace
        ));
        assert!(!config.stream);
        assert!(!config.tools_enabled);
        assert_eq!(config.max_tool_steps, 32);
        assert_eq!(config.context_window_tokens, 200_000);
    }

    #[test]
    fn merge_lets_workspace_win() {
        let user = PersistedSettings {
            provider: Some("openai".into()),
            ..PersistedSettings::default()
        };
        let workspace = PersistedSettings {
            provider: Some("deepseek".into()),
            model: Some("deepseek-chat".into()),
            ..PersistedSettings::default()
        };
        let merged = merge(user, workspace);
        assert_eq!(merged.provider.as_deref(), Some("deepseek"));
        assert_eq!(merged.model.as_deref(), Some("deepseek-chat"));
    }
}
