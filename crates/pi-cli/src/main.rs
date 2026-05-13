use std::env;
use std::path::PathBuf;
use std::process::Command;

use pi_agent::AgentRuntime;
use pi_core::{AppConfig, Event, ModelSelection, PiError, PiErrorKind, PiResult, VERSION};
use pi_providers::ProviderRegistry;
use pi_session::JsonlSessionStore;
use pi_tools::ToolRuntime;

fn main() {
    if let Err(error) = run() {
        eprintln!("错误：{error}");
        std::process::exit(1);
    }
}

fn run() -> PiResult<()> {
    let parsed = ParsedArgs::parse(env::args().skip(1))?;

    if parsed.help {
        print_help();
        return Ok(());
    }

    if parsed.doctor {
        print_doctor()?;
        return Ok(());
    }

    if parsed.version {
        println!("{VERSION}");
        return Ok(());
    }

    if parsed.list_providers {
        print_providers();
        return Ok(());
    }

    if parsed.list_models {
        print_models();
        return Ok(());
    }

    if parsed.list_tools {
        print_tools();
        return Ok(());
    }

    let prompt = parsed.prompt.ok_or_else(|| {
        PiError::new(
            PiErrorKind::InvalidInput,
            "缺少提示词。运行 `pi --help` 查看用法。",
        )
    })?;

    let config = AppConfig {
        model: ModelSelection {
            provider: parsed.provider,
            model: parsed.model,
        },
        session_path: parsed.session_id.clone(),
        print_mode: parsed.print_mode,
        tools_enabled: parsed.tools_enabled,
        enabled_tool_names: parsed.enabled_tool_names.clone(),
        ..AppConfig::default()
    };

    ProviderRegistry::builtin().require(&config.model.provider)?;
    let session_id = parsed.session_id.unwrap_or_else(|| "default".to_string());
    let store = JsonlSessionStore::new(default_session_root()?);
    let mut agent = AgentRuntime::try_new(config, store)?;
    let turn = agent.run_single_turn(&session_id, &prompt)?;

    for event in turn.events {
        match event {
            Event::AssistantDelta(delta) if parsed.print_mode => print!("{delta}"),
            Event::AssistantMessage(message) if !parsed.print_mode => println!("{message}"),
            Event::ToolFinished { output, .. } => println!("{output}"),
            _ => {}
        }
    }

    if parsed.print_mode {
        println!();
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct ParsedArgs {
    help: bool,
    doctor: bool,
    version: bool,
    list_providers: bool,
    list_models: bool,
    list_tools: bool,
    print_mode: bool,
    tools_enabled: bool,
    enabled_tool_names: Option<Vec<String>>,
    provider: String,
    model: String,
    session_id: Option<String>,
    prompt: Option<String>,
}

impl ParsedArgs {
    fn parse(args: impl IntoIterator<Item = String>) -> PiResult<Self> {
        let mut parsed = Self {
            help: false,
            doctor: false,
            version: false,
            list_providers: false,
            list_models: false,
            list_tools: false,
            print_mode: false,
            tools_enabled: true,
            enabled_tool_names: None,
            provider: "echo".to_string(),
            model: "echo-local".to_string(),
            session_id: None,
            prompt: None,
        };

        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "-h" | "--help" => parsed.help = true,
                "doctor" if parsed.prompt.is_none() => parsed.doctor = true,
                "-V" | "--version" => parsed.version = true,
                "--list-providers" => parsed.list_providers = true,
                "--list-models" => parsed.list_models = true,
                "--list-tools" => parsed.list_tools = true,
                "-p" | "--print" => parsed.print_mode = true,
                "--no-tools" => parsed.tools_enabled = false,
                "--tools" => {
                    let value = next_value(&mut iter, "--tools")?;
                    parsed.enabled_tool_names = Some(parse_csv(&value));
                }
                "--continue" | "-c" => parsed.session_id = Some("default".to_string()),
                "--provider" => parsed.provider = next_value(&mut iter, "--provider")?,
                "--model" => parsed.model = next_value(&mut iter, "--model")?,
                "--session" => parsed.session_id = Some(next_value(&mut iter, "--session")?),
                other if other.starts_with('-') => {
                    return Err(PiError::new(
                        PiErrorKind::InvalidInput,
                        format!("未知参数：{other}"),
                    ));
                }
                value => {
                    let mut prompt = parsed.prompt.take().unwrap_or_default();
                    if !prompt.is_empty() {
                        prompt.push(' ');
                    }
                    prompt.push_str(value);
                    parsed.prompt = Some(prompt);
                }
            }
        }

        Ok(parsed)
    }
}

fn next_value(iter: &mut impl Iterator<Item = String>, flag: &str) -> PiResult<String> {
    iter.next().ok_or_else(|| {
        PiError::new(
            PiErrorKind::InvalidInput,
            format!("参数 {flag} 需要一个值"),
        )
    })
}

fn parse_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn default_session_root() -> PiResult<PathBuf> {
    let home = env::var("HOME").map_err(|_| {
        PiError::new(
            PiErrorKind::Config,
            "无法读取 HOME 环境变量，不能确定会话目录",
        )
    })?;
    Ok(PathBuf::from(home).join(".pi-rust").join("sessions"))
}

fn print_help() {
    println!(
        "Pi Rust\n\n用法:\n  pi [OPTIONS] <MESSAGE>\n  pi doctor\n\n选项:\n  -p, --print              单次输出模式\n  -c, --continue           继续默认会话\n      --session <ID>       使用指定会话 ID\n      --provider <NAME>    设置 provider，默认 echo\n      --model <MODEL>      设置模型，默认 echo-local\n      --tools <LIST>       启用指定工具，逗号分隔\n      --list-providers     列出内置 provider\n      --list-models        列出内置模型预设\n      --list-tools         列出内置工具 schema\n      --no-tools           禁用内置工具\n  -h, --help               显示帮助\n  -V, --version            显示版本\n\n工具快捷方式:\n  /tool read README.md\n  /tool ls ."
    );
}

fn print_providers() {
    for provider in ProviderRegistry::builtin().list() {
        let key = provider
            .requires_api_key_env
            .as_deref()
            .unwrap_or("无需 API Key");
        println!(
            "{}\t{}\t默认模型: {}\t{}",
            provider.id, provider.display_name, provider.default_model, key
        );
    }
}

fn print_models() {
    for provider in ProviderRegistry::builtin().list() {
        println!("{}:", provider.id);
        for model in &provider.supported_models {
            let default_marker = if model == &provider.default_model {
                " (默认)"
            } else {
                ""
            };
            println!("  {model}{default_marker}");
        }
    }
}

fn print_tools() {
    for schema in ToolRuntime::builtin().schemas() {
        let mutation = if schema.mutates { "mutates" } else { "read-only" };
        println!(
            "{}\t{}\t{}\t{}",
            schema.name, mutation, schema.input_shape, schema.description
        );
    }
}

fn print_doctor() -> PiResult<()> {
    println!("Pi Rust doctor");
    print_command_status("cargo");
    print_command_status("rustc");
    print_command_status("curl");

    let session_root = default_session_root()?;
    println!("session_root\t{}", session_root.display());
    println!("session_root_exists\t{}", session_root.exists());

    for provider in ProviderRegistry::builtin().list() {
        if let Some(env_name) = &provider.requires_api_key_env {
            let status = if env::var(env_name).is_ok() {
                "configured"
            } else {
                "missing"
            };
            println!("provider_env\t{}\t{}\t{}", provider.id, env_name, status);
        }
    }

    Ok(())
}

fn print_command_status(command: &str) {
    let lookup = format!("command -v {command}");
    let status = Command::new("sh")
        .args(["-c", lookup.as_str()])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false);
    println!("command\t{command}\t{}", if status { "found" } else { "missing" });
}
