//! `pi` command-line entry point.
//!
//! Two modes:
//! - `--print` (or any positional prompt without `--interactive`): one-shot
//!   batch turn. Streams to stdout when stream is enabled.
//! - default (no prompt) or `--interactive`: launches the ratatui TUI.
//!
//! Beyond what TS pi exposes:
//! - `--permission` selects the engine mode at startup (read-only/confirm/trusted/plan).
//! - `--system` overrides the default system prompt.
//! - `--export-session`, `--rename-session`, `--delete-session` are first-class.

use std::env;
use std::path::PathBuf;
use std::process::Command;

use clap::{Parser, ValueEnum};
use pi_agent::AgentRuntime;

mod auth;
mod rpc;
mod update;
use pi_core::{
    AppConfig, Event, Locale, ModelSelection, PermissionModeKind, PiError, PiErrorKind, PiResult,
    VERSION,
};
use pi_providers::ProviderRegistry;
use pi_session::JsonlSessionStore;
use pi_tools::ToolRuntime;

#[derive(Parser, Debug)]
#[command(
    name = "pi",
    version = VERSION,
    about = "Pi Rust — 本地优先的中文 AI 编程助手",
    long_about = None,
)]
struct Cli {
    /// Prompt text. Provide multiple words separated by spaces. Omit to enter
    /// interactive TUI mode.
    #[arg(value_name = "PROMPT")]
    prompt: Vec<String>,

    /// One-shot batch mode: stream the assistant reply to stdout and exit.
    #[arg(short = 'p', long = "print")]
    print: bool,

    /// Continue the default session (`default.jsonl`).
    #[arg(short = 'c', long = "continue")]
    continue_default: bool,

    /// Resume a specific session id.
    #[arg(long = "resume", value_name = "ID")]
    resume: Option<String>,

    /// Pick a session interactively (ratatui list). Alias used by bare `pi sessions`.
    #[arg(long = "pick", action = clap::ArgAction::SetTrue)]
    pick: bool,

    /// Use the named session id for this turn.
    #[arg(long = "session", value_name = "ID")]
    session: Option<String>,

    /// Provider id (echo, ollama, openai, moonshot, deepseek, qwen, zhipu,
    /// minimax, anthropic, gemini).
    #[arg(long = "provider", default_value = "echo")]
    provider: String,

    /// Model id within the chosen provider.
    #[arg(long = "model", default_value = "echo-local")]
    model: String,

    /// Restrict to a comma-separated subset of tools.
    #[arg(long = "tools", value_delimiter = ',')]
    tools: Option<Vec<String>>,

    /// Read additional context from files; each --file appends a fenced
    /// block before the final positional prompt. May be repeated.
    #[arg(long = "file", value_name = "PATH")]
    file: Vec<std::path::PathBuf>,

    /// Read the full first user message from a file (replaces positional
    /// PROMPT). Mutually exclusive with positional prompt content.
    #[arg(long = "message-file", value_name = "PATH")]
    message_file: Option<std::path::PathBuf>,

    /// Disable tools entirely.
    #[arg(long = "no-tools")]
    no_tools: bool,

    /// Override the default system prompt.
    #[arg(long = "system")]
    system: Option<String>,

    /// Permission mode.
    #[arg(long = "permission", value_enum, default_value_t = PermissionModeArg::Confirm)]
    permission: PermissionModeArg,

    /// Output locale.
    #[arg(long = "locale", value_enum, default_value_t = LocaleArg::ZhCn)]
    locale: LocaleArg,

    /// Stream responses as they arrive.
    #[arg(long = "stream", default_value_t = true, action = clap::ArgAction::Set)]
    stream: bool,

    /// Maximum tool-call rounds per turn.
    #[arg(long = "max-steps", default_value_t = 16)]
    max_steps: u32,

    /// Force interactive TUI even if a prompt is provided.
    #[arg(short = 'i', long = "interactive")]
    interactive: bool,

    /// TUI theme: dark (default) / light / solarized, or path-based override.
    #[arg(long = "theme")]
    theme: Option<String>,

    /// Emit a chrome://tracing JSON to this path covering this invocation.
    #[arg(long = "trace", value_name = "PATH")]
    trace: Option<std::path::PathBuf>,

    /// Run a JSON-RPC over stdio loop (for SDK/IDE integrations).
    #[arg(long = "rpc")]
    rpc: bool,

    /// Run `pi doctor` and exit.
    #[arg(long = "doctor", action = clap::ArgAction::SetTrue)]
    doctor: bool,

    /// Send a real liveness probe to each configured provider.
    #[arg(long = "probe", action = clap::ArgAction::SetTrue)]
    probe: bool,

    /// Check GitHub Releases for a newer version.
    #[arg(long = "version-check", action = clap::ArgAction::SetTrue)]
    version_check: bool,

    /// Self-update by replacing the running binary with the latest release.
    /// Requires `--yes` to actually proceed.
    #[arg(long = "self-update", action = clap::ArgAction::SetTrue)]
    self_update: bool,

    /// Confirmation flag for `--self-update`.
    #[arg(long = "yes", action = clap::ArgAction::SetTrue)]
    yes: bool,

    /// Override GitHub repo for release lookups (default: Shellmia0/Pi_Rust).
    #[arg(long = "release-repo")]
    release_repo: Option<String>,

    #[arg(long = "list-providers", action = clap::ArgAction::SetTrue)]
    list_providers: bool,
    #[arg(long = "list-models", action = clap::ArgAction::SetTrue)]
    list_models: bool,
    #[arg(long = "list-aliases", action = clap::ArgAction::SetTrue)]
    list_aliases: bool,
    #[arg(long = "list-tools", action = clap::ArgAction::SetTrue)]
    list_tools: bool,
    #[arg(long = "list-resources", action = clap::ArgAction::SetTrue)]
    list_resources: bool,
    #[arg(long = "list-prompts", action = clap::ArgAction::SetTrue)]
    list_prompts: bool,
    /// Apply a Linux Landlock ruleset to the process so even pi itself
    /// cannot read files outside the workspace. Best-effort: no-op on
    /// non-Linux and on kernels < 5.13.
    #[arg(long = "strict-sandbox", action = clap::ArgAction::SetTrue)]
    strict_sandbox: bool,
    /// Boot with a workspace agent profile. Reads `.pi/agents/<name>.md`
    /// (or `.toml`) and uses its contents as the system prompt override.
    #[arg(long = "agent", value_name = "NAME")]
    agent: Option<String>,
    #[arg(long = "list-sessions", action = clap::ArgAction::SetTrue)]
    list_sessions: bool,
    #[arg(long = "delete-session", value_name = "ID")]
    delete_session: Option<String>,
    #[arg(long = "rename-session", value_names = ["FROM", "TO"], num_args = 2)]
    rename_session: Option<Vec<String>>,
    #[arg(long = "export-session", value_name = "ID")]
    export_session: Option<String>,
    #[arg(long = "export-json", value_name = "ID")]
    export_session_json: Option<String>,
    #[arg(long = "export-html", value_name = "ID")]
    export_session_html: Option<String>,
}

#[derive(Clone, Debug, ValueEnum)]
enum PermissionModeArg {
    ReadOnly,
    Confirm,
    Trusted,
    Plan,
}

impl From<PermissionModeArg> for PermissionModeKind {
    fn from(value: PermissionModeArg) -> Self {
        match value {
            PermissionModeArg::ReadOnly => PermissionModeKind::ReadOnly,
            PermissionModeArg::Confirm => PermissionModeKind::ConfirmMutations,
            PermissionModeArg::Trusted => PermissionModeKind::TrustedWorkspace,
            PermissionModeArg::Plan => PermissionModeKind::Plan,
        }
    }
}

#[derive(Clone, Debug, ValueEnum)]
enum LocaleArg {
    ZhCn,
    En,
}

impl From<LocaleArg> for Locale {
    fn from(value: LocaleArg) -> Self {
        match value {
            LocaleArg::ZhCn => Locale::ZhCn,
            LocaleArg::En => Locale::En,
        }
    }
}

fn main() {
    maybe_run_first_run_wizard();
    // Pre-clap subcommand routing: `pi auth …`, `pi doctor`, `pi sessions`.
    if let Some(auth_args) = auth_subcommand() {
        if let Err(err) = auth::run(&auth_args) {
            eprintln!("错误：{err}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(legacy) = legacy_subcommand_args() {
        if let Err(err) = run(legacy) {
            eprintln!("错误：{err}");
            std::process::exit(1);
        }
        return;
    }

    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            use clap::error::ErrorKind;
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                err.exit();
            }
            err.exit();
        }
    };
    let start = std::time::Instant::now();
    if let Err(error) = run(args) {
        eprintln!("错误：{error}");
        let locale = match std::env::var("PI_LOCALE").as_deref() {
            Ok("en") | Ok("en-US") => pi_core::Locale::En,
            _ => pi_core::Locale::ZhCn,
        };
        let hint = pi_core::hint_for(&error, locale);
        eprintln!("提示：{}", hint.format());
        if matches!(error.kind, pi_core::PiErrorKind::Provider)
            && (error.message.contains("401")
                || error.message.contains("403")
                || error.message.contains("缺少凭证")
                || error.message.contains("expired"))
        {
            for provider in [
                "anthropic",
                "openai-responses",
                "openai",
                "azure",
                "bedrock",
                "vertex",
                "copilot",
                "moonshot",
                "deepseek",
                "qwen",
                "zhipu",
                "minimax",
                "gemini",
                "openrouter",
                "mistral",
                "cloudflare",
            ] {
                if error.message.contains(provider) {
                    if let Some(g) = pi_core::auth_guidance_for(provider, locale) {
                        eprintln!("登录指引：{}", g.format());
                    }
                    break;
                }
            }
        }
        pi_core::record_telemetry(
            "run",
            Some(format!("{:?}", error.kind)),
            Some(start.elapsed().as_millis() as u64),
        );
        pi_core::flush_telemetry();
        pi_core::timings::finalize();
        std::process::exit(1);
    }
    pi_core::record_telemetry("run", None, Some(start.elapsed().as_millis() as u64));
    pi_core::flush_telemetry();
    pi_core::timings::finalize();
}

fn legacy_subcommand_args() -> Option<Cli> {
    let argv: Vec<String> = env::args().collect();
    if argv.len() < 2 {
        return None;
    }
    match argv[1].as_str() {
        "doctor" if argv.len() == 2 => Cli::try_parse_from(["pi", "--doctor"]).ok(),
        "sessions" if argv.len() == 2 => Cli::try_parse_from(["pi", "--pick"]).ok(),
        _ => None,
    }
}

fn maybe_run_first_run_wizard() {
    if !pi_tui::needs_config_wizard() {
        return;
    }
    let providers: Vec<pi_tui::ProviderChoice> = ProviderRegistry::builtin()
        .list()
        .map(|p| pi_tui::ProviderChoice {
            id: p.id.clone(),
            display_name: p.display_name.clone(),
            default_model: p.default_model.clone(),
            supported_models: p.supported_models.clone(),
            requires_api_key_env: p.requires_api_key_env.clone(),
        })
        .collect();
    if let Err(err) = pi_tui::run_config_wizard(&providers) {
        eprintln!("配置向导失败：{err}（继续以默认 echo provider 运行）");
    }
}

fn auth_subcommand() -> Option<Vec<String>> {
    let argv: Vec<String> = env::args().collect();
    if argv.len() >= 2 && argv[1] == "auth" {
        Some(argv[2..].to_vec())
    } else {
        None
    }
}

fn parse_args() -> Result<Cli, clap::Error> {
    Cli::try_parse()
}

fn run(cli: Cli) -> PiResult<()> {
    if let Some(trace_path) = cli.trace.clone() {
        pi_core::timings::enable(trace_path);
    }
    let _trace_finalize = pi_core::timings::span("pi.run");
    if cli.doctor {
        return print_doctor(cli.probe);
    }
    if cli.probe {
        return print_probe();
    }
    if cli.version_check {
        return update::version_check(cli.release_repo.as_deref());
    }
    if cli.self_update {
        return update::self_update(cli.release_repo.as_deref(), cli.yes);
    }
    if cli.list_providers {
        print_providers();
        return Ok(());
    }
    if cli.list_models {
        print_models();
        return Ok(());
    }
    if cli.list_aliases {
        print_aliases();
        return Ok(());
    }
    if cli.list_tools {
        print_tools();
        return Ok(());
    }
    if cli.list_resources {
        print_resources()?;
        return Ok(());
    }
    if cli.list_prompts {
        print_prompts()?;
        return Ok(());
    }

    let session_root = default_session_root()?;
    let store = JsonlSessionStore::new(session_root);

    if cli.list_sessions {
        return print_sessions(&store);
    }
    let mut picked_session: Option<String> = None;
    if cli.pick {
        match pi_tui::pick_session(&store)
            .map_err(|err| PiError::new(PiErrorKind::Io, err.to_string()))?
        {
            pi_tui::PickResult::Selected(id) => picked_session = Some(id),
            pi_tui::PickResult::NewSession => picked_session = Some("default".to_string()),
            pi_tui::PickResult::Cancelled => {
                // Fall back to text listing so non-TTY invocations still
                // see something useful.
                return print_sessions(&store);
            }
        }
    }
    if let Some(id) = cli.delete_session {
        let deleted = store.delete(&id)?;
        println!(
            "{}",
            if deleted {
                format!("已删除会话：{id}")
            } else {
                format!("会话不存在：{id}")
            }
        );
        return Ok(());
    }
    if let Some(pair) = cli.rename_session {
        if pair.len() != 2 {
            return Err(PiError::new(
                PiErrorKind::InvalidInput,
                "--rename-session 需要 <FROM> <TO>",
            ));
        }
        store.rename(&pair[0], &pair[1])?;
        println!("已重命名会话：{} -> {}", pair[0], pair[1]);
        return Ok(());
    }
    if let Some(id) = cli.export_session {
        println!("{}", store.export_markdown(&id)?);
        return Ok(());
    }
    if let Some(id) = cli.export_session_json {
        println!("{}", store.export_json(&id)?);
        return Ok(());
    }
    if let Some(id) = cli.export_session_html {
        println!("{}", store.export_html(&id)?);
        return Ok(());
    }

    let session_id = picked_session
        .or_else(|| cli.session.clone())
        .or_else(|| cli.resume.clone())
        .or_else(|| {
            if cli.continue_default {
                Some("default".to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "default".to_string());

    let mut prompt = if let Some(path) = &cli.message_file {
        std::fs::read_to_string(path).map_err(|err| {
            PiError::new(
                PiErrorKind::Io,
                format!("--message-file 读取 {} 失败：{err}", path.display()),
            )
        })?
    } else {
        cli.prompt.join(" ")
    };
    if !cli.file.is_empty() {
        let mut attachments = String::new();
        for path in &cli.file {
            let body = std::fs::read_to_string(path).map_err(|err| {
                PiError::new(
                    PiErrorKind::Io,
                    format!("--file 读取 {} 失败：{err}", path.display()),
                )
            })?;
            attachments.push_str(&format!(
                "\n\n--- file: {} ---\n{}\n--- end ---",
                path.display(),
                body
            ));
        }
        if prompt.trim().is_empty() {
            prompt = attachments.trim_start().to_string();
        } else {
            prompt.push_str(&attachments);
        }
    }
    let interactive = cli.interactive || (prompt.trim().is_empty() && !cli.print);

    let mut config = AppConfig::default();
    // Layered settings: user → workspace. CLI flags override below.
    let workspace_root = env::current_dir().ok();
    let settings = pi_agent::PersistedSettings::load_layered(workspace_root.as_deref());
    settings.apply_to(&mut config);

    let cli_locale: pi_core::Locale = cli.locale.clone().into();
    let cli_permission: pi_core::PermissionModeKind = cli.permission.clone().into();
    let cli_provided_provider = cli.provider != "echo" || settings.provider.is_none();
    let cli_provided_model = cli.model != "echo-local" || settings.model.is_none();
    let provider_is_default = cli.provider == "echo" && !cli_provided_provider;
    // If the user only set --model and the value looks like an alias, resolve
    // both provider and model from the alias table. Explicit --provider always
    // wins over alias resolution.
    if cli_provided_model && (provider_is_default || cli.provider == "echo") {
        if let Ok(resolved) = pi_providers::resolve_alias(&cli.model) {
            if resolved.via_alias {
                config.model = resolved.model;
                if cli_provided_provider
                    && cli.provider != config.model.provider
                    && cli.provider != "echo"
                {
                    // user passed both --provider and an alias that contradicts
                    // it; keep --provider.
                    config.model.provider = cli.provider.clone();
                }
            } else {
                config.model.model = cli.model.clone();
                if cli_provided_provider {
                    config.model.provider = cli.provider.clone();
                }
            }
        } else {
            config.model.model = cli.model.clone();
            if cli_provided_provider {
                config.model.provider = cli.provider.clone();
            }
        }
    } else {
        if cli_provided_provider {
            config.model.provider = cli.provider.clone();
        }
        if cli_provided_model {
            config.model.model = cli.model.clone();
        }
    }
    config.session_path = Some(session_id.clone());
    config.print_mode = cli.print;
    if cli.no_tools {
        config.tools_enabled = false;
    }
    if cli.tools.is_some() {
        config.enabled_tool_names = cli.tools.clone();
    }
    if cli.system.is_some() {
        config.system_prompt = cli.system.clone();
    }
    if let Some(agent_name) = &cli.agent {
        // `pi --agent <name>` boots a workspace-defined agent profile from
        // `.pi/agents/<name>.{md,toml}`. The file body becomes the system
        // prompt; an explicit `--system` still wins.
        let body = load_agent_profile(agent_name)?;
        if cli.system.is_none() {
            config.system_prompt = Some(body);
        }
    }
    config.locale = cli_locale;
    config.permission_mode = cli_permission;
    config.stream = cli.stream;
    if cli.max_steps != 16 {
        config.max_tool_steps = cli.max_steps;
    }
    let _ = ModelSelection::default();

    let registry = ProviderRegistry::builtin();
    let info = registry.require(&config.model.provider)?;
    // Reject unknown models upfront with a friendly listing so the user
    // doesn't see a generic provider error later. We allow:
    // - exact match against the registry's supported_models list,
    // - any model when supported_models is empty (registry doesn't enumerate),
    // - the literal `--model echo-local` always (echo provider).
    if !info.supported_models.is_empty()
        && !info
            .supported_models
            .iter()
            .any(|m| m == &config.model.model)
    {
        let preview: Vec<String> = info
            .supported_models
            .iter()
            .take(6)
            .cloned()
            .collect();
        let more = if info.supported_models.len() > preview.len() {
            format!("… (+{} more)", info.supported_models.len() - preview.len())
        } else {
            String::new()
        };
        return Err(PiError::new(
            PiErrorKind::Provider,
            format!(
                "未知模型：{} / {}。\n该 provider 已知模型：{} {}\n运行 `pi --list-models --provider {}` 查看完整列表。",
                info.id,
                config.model.model,
                preview.join(", "),
                more,
                info.id,
            ),
        ));
    }

    let mut agent = AgentRuntime::try_new(config.clone(), store)?;

    // Wire MCP servers configured in `.pi/mcp.toml` into the agent's tool runtime.
    if let Some(root) = env::current_dir().ok() {
        if let Ok(mcp_manager) = pi_mcp::McpManager::load_workspace(&root) {
            if !mcp_manager.server_ids().is_empty() {
                mcp_manager.register_into(agent.tools_mut());
                // Install the host-side callback handlers so each MCP server
                // can issue `sampling/createMessage` requests against our
                // model and stream `notifications/progress` into the agent's
                // event channel.
                let sampling = std::sync::Arc::new(
                    pi_agent::mcp_bridge::AgentSamplingHandler::new(agent.config().clone()),
                );
                let (progress_queue, progress_handler) =
                    pi_agent::mcp_bridge::EventQueueProgressHandler::new();
                for id in mcp_manager.server_ids() {
                    if let Some(server) = mcp_manager.server(&id) {
                        server.set_sampling_handler(sampling.clone());
                        server.set_progress_handler(progress_handler.clone());
                    }
                }
                agent.set_mcp_progress_queue(progress_queue);
                // Manager kept alive in a static for the process lifetime so
                // child processes do not get killed mid-conversation.
                let leaked: &'static pi_mcp::McpManager = Box::leak(Box::new(mcp_manager));
                let _ = leaked;
            }
        }
    }

    // Opt-in syscall-level isolation. The user passed `--strict-sandbox` so
    // we ratchet down the process FS view via Landlock (Linux ≥5.13). On
    // other platforms or older kernels this returns Unsupported / NotApplied
    // and we log to stderr but keep running.
    if cli.strict_sandbox {
        use pi_permissions::{landlock_supported, restrict_self, LandlockOutcome, LandlockPlan};
        if !landlock_supported() {
            eprintln!(
                "提示：--strict-sandbox 仅在 Linux ≥5.13 生效；当前平台不支持，已忽略。"
            );
        } else {
            let cwd = env::current_dir().ok();
            let profile = pi_permissions::SandboxProfile {
                workspace_root: cwd.as_ref().map(|p| p.display().to_string()),
                extra_read_roots: vec![
                    "/etc/ssl".into(),
                    "/etc/resolv.conf".into(),
                    "/etc/hosts".into(),
                ],
                allow_network: true,
            };
            let plan = LandlockPlan::from_profile(
                &profile,
                &[
                    pi_permissions::Capability::ReadFile,
                    pi_permissions::Capability::WriteFile,
                    pi_permissions::Capability::Network,
                ],
            );
            match restrict_self(&plan) {
                LandlockOutcome::Applied { compatibility } => {
                    eprintln!("[landlock] 已生效：{compatibility}");
                }
                LandlockOutcome::NotApplied { reason } => {
                    eprintln!("[landlock] 未生效：{reason}");
                }
                LandlockOutcome::Unsupported => {}
            }
        }
    }

    if cli.rpc {
        return rpc::run_stdio(&mut agent);
    }

    if interactive {
        return pi_tui::run_interactive_with_theme(agent, session_id, cli.theme.clone());
    }

    // Disable stream for plain --print so the agent runs the synthetic
    // single-shot path and emits AssistantMessage instead of a flood of
    // deltas. Streaming stays on by default for the interactive TUI.
    if cli.print {
        config.stream = false;
    }

    let turn = agent.run_single_turn(&session_id, &prompt)?;

    let mut last_was_delta = false;
    for event in turn.events {
        match event {
            Event::AssistantDelta(delta) if cli.print => {
                use std::io::Write;
                print!("{delta}");
                std::io::stdout().flush().ok();
                last_was_delta = true;
            }
            Event::AssistantMessage(message) if !cli.print => println!("{message}"),
            Event::ToolFinished { name, output } => {
                println!("[tool:{name}]\n{output}");
            }
            Event::ToolStarted { .. } => {}
            Event::Usage(usage) if !cli.print => {
                eprintln!(
                    "[usage] in={} out={} total={} cache_read={}",
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.total_tokens,
                    usage.cache_read_tokens
                );
            }
            _ => {}
        }
    }
    if cli.print && last_was_delta {
        println!();
    }

    Ok(())
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

fn print_aliases() {
    println!("alias\tprovider\tmodel");
    for (alias, provider, model) in pi_providers::aliases::aliases_table() {
        println!("{alias}\t{provider}\t{model}");
    }
    println!();
    println!("用法：pi --model sonnet 等价于 --provider anthropic --model claude-sonnet-4-6");
}

fn print_tools() {
    for schema in ToolRuntime::builtin().schemas() {
        let mutation = if schema.mutates {
            "mutates"
        } else {
            "read-only"
        };
        println!(
            "{}\t{}\t{}\t{}",
            schema.name, mutation, schema.input_shape, schema.description
        );
        if let Some(parameters) = &schema.parameters {
            println!(
                "\tparameters: {}",
                serde_json::to_string(parameters).unwrap_or_default()
            );
        }
    }
}

fn load_agent_profile(name: &str) -> PiResult<String> {
    let cwd = env::current_dir().map_err(|err| {
        PiError::new(PiErrorKind::Io, format!("无法读取 cwd：{err}"))
    })?;
    let candidates = [
        cwd.join(".pi").join("agents").join(format!("{name}.md")),
        cwd.join(".pi").join("agents").join(format!("{name}.toml")),
        cwd.join(".pi").join("agents").join(name),
    ];
    for path in candidates {
        if path.exists() {
            let text = std::fs::read_to_string(&path).map_err(|err| {
                PiError::new(
                    PiErrorKind::Io,
                    format!("读取 {} 失败：{err}", path.display()),
                )
            })?;
            return Ok(text.trim().to_string());
        }
    }
    Err(PiError::new(
        PiErrorKind::NotFound,
        format!(
            "未找到 agent profile：.pi/agents/{name}.(md|toml)。\n\
            使用 `pi --list-resources` 或检查工作区 .pi/agents/ 目录。"
        ),
    ))
}

fn print_resources() -> PiResult<()> {
    // Local resources from .pi/resources/*
    let cwd = env::current_dir().ok();
    if let Some(root) = &cwd {
        for resource in pi_ext::wrapper::ToolBridge::load_workspace_resources(root) {
            println!(
                "local\t{}\t{}\t{} bytes",
                resource.uri,
                resource.mime_type.unwrap_or_default(),
                resource.body.len()
            );
        }
    }
    // MCP-aggregated resources.
    if let Some(root) = &cwd {
        if let Ok(manager) = pi_mcp::McpManager::load_workspace(root) {
            for (server_id, resource) in manager.all_resources() {
                println!(
                    "mcp:{}\t{}\t{}\t{}",
                    server_id,
                    resource.uri,
                    resource.mime_type.clone().unwrap_or_default(),
                    resource.description.clone().unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}

fn print_prompts() -> PiResult<()> {
    let cwd = env::current_dir().ok();
    if let Some(root) = &cwd {
        for prompt in pi_ext::wrapper::ToolBridge::load_workspace_prompts(root) {
            println!("local\t{}\t{}", prompt.name, prompt.description.unwrap_or_default());
        }
    }
    if let Some(root) = &cwd {
        if let Ok(manager) = pi_mcp::McpManager::load_workspace(root) {
            for (server_id, prompt) in manager.all_prompts() {
                println!(
                    "mcp:{}\t{}\t{}",
                    server_id,
                    prompt.name,
                    prompt.description.clone().unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}

fn print_sessions(store: &JsonlSessionStore) -> PiResult<()> {
    let sessions = store.list()?;
    if sessions.is_empty() {
        println!("没有会话");
        return Ok(());
    }

    for session in sessions {
        let excerpt = session.last_user_excerpt.as_deref().unwrap_or("");
        println!(
            "{}\tmessages: {}\tupdated_ms: {}\t{}",
            session.id, session.message_count, session.updated_ms, excerpt
        );
    }
    Ok(())
}

fn print_doctor(also_probe: bool) -> PiResult<()> {
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

    println!("rust_version\t{}", env!("CARGO_PKG_RUST_VERSION"));
    println!("pi_version\t{VERSION}");

    if also_probe {
        println!();
        print_probe()?;
    }
    Ok(())
}

fn print_probe() -> PiResult<()> {
    println!("Provider liveness probe (--probe):");
    for report in pi_providers::probe_all() {
        let label = match report.outcome {
            pi_providers::ProbeOutcome::Ok => "ok".to_string(),
            pi_providers::ProbeOutcome::AuthFailed(reason) => format!("auth_fail({reason})"),
            pi_providers::ProbeOutcome::Unreachable(reason) => format!("unreachable({reason})"),
            pi_providers::ProbeOutcome::MissingCredential => "missing_credential".to_string(),
            pi_providers::ProbeOutcome::Unsupported => "unsupported".to_string(),
        };
        println!("probe\t{}\t{}", report.provider, label);
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
    println!(
        "command\t{command}\t{}",
        if status { "found" } else { "missing" }
    );
}
