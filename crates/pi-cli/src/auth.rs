//! `pi auth …` subcommands. Thin wrapper over `pi-auth`'s resolver.

use pi_auth::{env_for_provider, layered_resolver, Resolver};
use pi_core::{PiError, PiErrorKind, PiResult};

pub fn run(args: &[String]) -> PiResult<()> {
    let mut resolver = layered_resolver()?;
    match args.first().map(String::as_str) {
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
