//! `pi auth …` subcommands. Thin wrapper over `pi-auth`'s resolver.

use pi_auth::{env_for_provider, layered_resolver, OAuthConfig, Resolver};
use pi_core::{PiError, PiErrorKind, PiResult};
use std::time::Duration;

pub fn run(args: &[String]) -> PiResult<()> {
    let mut resolver = layered_resolver()?;
    match args.first().map(String::as_str) {
        Some("login") => return login(&mut resolver, &args[1..]),
        Some("set") => {
            let provider = args
                .get(1)
                .ok_or_else(|| missing_arg("用法：pi auth set <provider> [--from-stdin]"))?;
            let env_name = env_for_provider(provider).ok_or_else(|| {
                PiError::new(
                    PiErrorKind::InvalidInput,
                    format!("未知 provider {provider}，无法推断 env 名"),
                )
            })?;
            let value = if args.iter().any(|a| a == "--from-stdin") {
                use std::io::Read;
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf)?;
                buf.trim_end().to_string()
            } else {
                rpassword::prompt_password(format!("{env_name}: "))
                    .map_err(|err| PiError::new(PiErrorKind::Io, format!("读取密码失败：{err}")))?
            };
            if value.is_empty() {
                return Err(PiError::new(PiErrorKind::InvalidInput, "凭证为空，已取消"));
            }
            resolver.store(provider, env_name, &value)?;
            println!("已保存 {env_name} 到加密 auth 存储");
            Ok(())
        }
        Some("remove") | Some("rm") => {
            let provider = args
                .get(1)
                .ok_or_else(|| missing_arg("用法：pi auth remove <provider>"))?;
            let env_name = env_for_provider(provider).ok_or_else(|| {
                PiError::new(
                    PiErrorKind::InvalidInput,
                    format!("未知 provider {provider}"),
                )
            })?;
            let removed = resolver.delete(provider, env_name)?;
            println!(
                "{}",
                if removed {
                    format!("已删除 {env_name}")
                } else {
                    format!("{env_name} 不在 auth 存储中")
                }
            );
            Ok(())
        }
        Some("list") | Some("ls") => {
            let names = resolver.list()?;
            if names.is_empty() {
                println!("auth 存储为空");
            } else {
                for name in names {
                    println!("{name}");
                }
            }
            Ok(())
        }
        Some(other) => Err(PiError::new(
            PiErrorKind::InvalidInput,
            format!("未知 auth 子命令：{other}。可用：set / remove / list"),
        )),
        None => Err(missing_arg("用法：pi auth <set|remove|list> …")),
    }
}

fn missing_arg(message: &str) -> PiError {
    PiError::new(PiErrorKind::InvalidInput, message.to_string())
}

fn login(resolver: &mut impl Resolver, args: &[String]) -> PiResult<()> {
    let provider = args
        .first()
        .ok_or_else(|| missing_arg("用法：pi auth login <provider> [--no-browser]"))?;
    let no_browser = args.iter().any(|a| a == "--no-browser");
    let config = oauth_config_for(provider)?;
    let tokens = pi_auth::oauth::run(&config, Duration::from_secs(180), !no_browser)?;
    // Store access token under the provider's standard env-name slot so the
    // provider modules read it transparently. Refresh token goes to a
    // sibling key so a future `pi auth refresh` can pick it up.
    let env_name = env_for_provider(provider).ok_or_else(|| {
        PiError::new(
            PiErrorKind::InvalidInput,
            format!("未知 provider {provider}，无法保存 token"),
        )
    })?;
    resolver.store(provider, env_name, &tokens.access_token)?;
    if let Some(refresh) = tokens.refresh_token.as_deref() {
        let refresh_env = format!("{env_name}_REFRESH");
        resolver.store(provider, &refresh_env, refresh)?;
    }
    println!("已通过 OAuth 登录 {provider} 并保存 token");
    if let Some(secs) = tokens.expires_in {
        println!("token 有效期约 {} 秒（之后需要 refresh）。", secs);
    }
    Ok(())
}

fn oauth_config_for(provider: &str) -> PiResult<OAuthConfig> {
    let client_id = std::env::var("PI_OAUTH_CLIENT_ID")
        .or_else(|_| {
            std::env::var(format!(
                "PI_OAUTH_CLIENT_ID_{}",
                provider.to_ascii_uppercase()
            ))
        })
        .map_err(|_| {
            PiError::new(
                PiErrorKind::Config,
                format!(
                    "OAuth 需要 client_id：export PI_OAUTH_CLIENT_ID_{}=… 后重试",
                    provider.to_ascii_uppercase()
                ),
            )
        })?;
    let (authorize, token, scope) = match provider {
        "anthropic" => (
            "https://console.anthropic.com/oauth/authorize",
            "https://console.anthropic.com/oauth/token",
            Some("api".to_string()),
        ),
        "openai" => (
            "https://auth.openai.com/oauth/authorize",
            "https://auth.openai.com/oauth/token",
            Some("api.read api.write".to_string()),
        ),
        other => {
            return Err(PiError::new(
                PiErrorKind::InvalidInput,
                format!("尚未为 {other} 内置 OAuth endpoint"),
            ));
        }
    };
    Ok(OAuthConfig {
        provider: provider.to_string(),
        authorize_endpoint: authorize.to_string(),
        token_endpoint: token.to_string(),
        client_id,
        scope,
        redirect_path: "/callback".to_string(),
    })
}
