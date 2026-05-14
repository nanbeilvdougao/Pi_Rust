//! First-run config wizard.
//!
//! Triggered when `~/.pi-rust/config.toml` does not exist. Walks the user
//! through:
//!
//! 1. Pick a provider from the built-in registry.
//! 2. Pick a model from that provider's supported list.
//! 3. Optionally prompt for the API key — value is stored via `pi-auth`'s
//!    encrypted file backend so it survives across runs without sitting in
//!    `~/.bash_history`.
//! 4. Write `~/.pi-rust/config.toml` with `provider = …`, `model = …`.
//!
//! On non-TTY stdout (CI, pipes) we skip the wizard entirely and let the
//! caller fall back to env-var configuration.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use crossterm::tty::IsTty;

use pi_core::{PiError, PiErrorKind, PiResult};

pub struct WizardResult {
    pub provider: String,
    pub model: String,
    pub config_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ProviderChoice {
    pub id: String,
    pub display_name: String,
    pub default_model: String,
    pub supported_models: Vec<String>,
    pub requires_api_key_env: Option<String>,
}

pub fn config_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".pi-rust").join("config.toml"))
}

pub fn needs_wizard() -> bool {
    match config_path() {
        Some(path) => !path.exists(),
        None => false,
    }
}

/// Run the wizard. Returns `Ok(None)` when stdout is not a TTY (the caller
/// will run with defaults).
pub fn run(providers: &[ProviderChoice]) -> PiResult<Option<WizardResult>> {
    if !io::stdout().is_tty() {
        return Ok(None);
    }
    if providers.is_empty() {
        return Ok(None);
    }

    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "—— Pi Rust 首次配置 ——");
    let _ = writeln!(
        stdout,
        "未发现 ~/.pi-rust/config.toml，按提示选择默认 provider / model；可随时用 `pi --provider` 覆盖。"
    );
    let _ = writeln!(stdout);

    let provider = prompt_choice(
        &mut stdout,
        "选择 provider",
        providers
            .iter()
            .map(|p| format!("{:<10} {}", p.id, p.display_name))
            .collect(),
    )?;
    let provider = &providers[provider];
    let model_idx = if provider.supported_models.is_empty() {
        0
    } else {
        prompt_choice(
            &mut stdout,
            &format!("选择 {} 的模型", provider.id),
            provider.supported_models.clone(),
        )?
    };
    let model = provider
        .supported_models
        .get(model_idx)
        .cloned()
        .unwrap_or_else(|| provider.default_model.clone());

    // Offer to store the API key (encrypted) when the provider needs one.
    if let Some(env_name) = &provider.requires_api_key_env {
        let _ = writeln!(stdout);
        let _ = writeln!(
            stdout,
            "{} 需要凭证。现在输入会以 ChaCha20-Poly1305 加密保存到 ~/.pi-rust/auth.enc；",
            provider.id
        );
        let _ = writeln!(
            stdout,
            "也可以直接回车跳过，之后用 `export {}=…` 或 `pi auth set {}` 配置。",
            env_name, provider.id
        );
        let _ = write!(stdout, "{}: ", env_name);
        let _ = stdout.flush();
        let mut value = String::new();
        io::stdin().read_line(&mut value)?;
        let value = value.trim().to_string();
        if !value.is_empty() {
            store_credential(&provider.id, env_name, &value)?;
            let _ = writeln!(stdout, "已加密保存。");
        }
    }

    let cfg_path =
        config_path().ok_or_else(|| PiError::new(PiErrorKind::Config, "无法解析配置目录"))?;
    if let Some(parent) = cfg_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = format!(
        "# 由 pi 首次启动向导写入\nprovider = \"{}\"\nmodel = \"{}\"\n",
        provider.id, model
    );
    fs::write(&cfg_path, text)?;
    let _ = writeln!(stdout, "已写入 {}.", cfg_path.display());

    Ok(Some(WizardResult {
        provider: provider.id.clone(),
        model,
        config_path: cfg_path,
    }))
}

fn store_credential(provider: &str, env_name: &str, value: &str) -> PiResult<()> {
    let path = pi_auth::default_auth_path()?;
    let mut store = pi_auth::encrypted_file::EncryptedFileStore::open(path)?;
    use pi_auth::Resolver as _;
    store.store(provider, env_name, value)
}

fn prompt_choice<W: Write>(out: &mut W, header: &str, options: Vec<String>) -> PiResult<usize> {
    loop {
        let _ = writeln!(out);
        let _ = writeln!(out, "{header}：");
        for (idx, label) in options.iter().enumerate() {
            let _ = writeln!(out, "  {:>2}) {}", idx + 1, label);
        }
        let _ = write!(out, "请输入 1-{}: ", options.len());
        let _ = out.flush();
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        match buf.trim().parse::<usize>() {
            Ok(n) if n >= 1 && n <= options.len() => return Ok(n - 1),
            _ => {
                let _ = writeln!(out, "无效输入，请重新选择。");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_wizard_is_idempotent() {
        // Just exercise the path; result depends on the test runner's $HOME.
        let _ = needs_wizard();
    }

    #[test]
    fn run_returns_none_under_non_tty() {
        // cargo test captures stdout → not a TTY, wizard short-circuits.
        let providers = vec![ProviderChoice {
            id: "echo".into(),
            display_name: "Echo".into(),
            default_model: "echo-local".into(),
            supported_models: vec!["echo-local".into()],
            requires_api_key_env: None,
        }];
        let outcome = run(&providers).expect("wizard");
        assert!(outcome.is_none());
    }
}
