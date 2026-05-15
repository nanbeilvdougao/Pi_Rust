//! Agent loop: drives one or more provider turns and reconciles tool calls.
//!
//! Differences vs. the MVP and from pi_agent_rust / pi-rs:
//!
//! - Streaming is first-class. The agent always uses `provider.stream(...)` and
//!   adapts `StreamEvent`s into typed `Event`s for the consumer. Non-streaming
//!   providers fall through automatically because the trait default replays
//!   captured events through the sink.
//! - System prompt is composed from a configurable template that captures
//!   `cwd`, `os`, `arch`, locale, and tool inventory. The template is overridable
//!   so users can install a custom `system.md` in their `.pi/` directory.
//! - Cooperative cancellation: an `Arc<AtomicBool>` can be flipped by a UI
//!   thread to abort an in-flight stream. The `StreamSink` checks it.
//! - Context compaction is applied before each provider call: when the token
//!   estimate exceeds the configured fraction of the context window we summarize
//!   older turns using the same provider.
//! - Slash commands run before provider routing so they always work, even
//!   when the provider is unavailable.
//!
//! The whole loop is synchronous; that is deliberate. It lets the agent compose
//! cleanly with CLI batch mode, an embeddable SDK, the RPC/SDK harness, and a
//! ratatui-driven TUI thread without dragging tokio into every consumer.

#![cfg_attr(test, allow(clippy::expect_used, clippy::panic, clippy::unwrap_used))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pi_core::{
    estimate_messages_tokens, AppConfig, Event, Message, PermissionModeKind, PiError, PiErrorKind,
    PiResult, Role, StreamEvent, StreamSink, ToolInvocation, Usage,
};
use pi_permissions::{PermissionEngine, PermissionMode};
use pi_providers::{provider_for, Provider, ProviderRequest, ProviderResponse};
use pi_session::{Session, SessionStore};
use pi_tools::{ToolCall, ToolRuntime};

pub mod branch_summary;
pub mod compaction;
pub mod fs_watch;
pub mod hooks;
pub mod mcp_bridge;
pub mod settings;
pub mod skills;
pub mod slash;
pub mod source_info;
pub mod subagent;
pub mod system_prompt;
pub use branch_summary::{
    merge_summaries, Branch, BranchSummarizer, BranchSummarizerConfig, BranchSummaryEntry,
};
pub use compaction::{maybe_compact, CompactionReport};
pub use subagent::ConfigSpawner;

fn register_task_tool(tools: &mut ToolRuntime, config: AppConfig) {
    // Skip task tool registration when the parent has restricted the tool
    // allowlist and explicitly omitted `task`.
    if let Some(names) = &config.enabled_tool_names {
        if !names.iter().any(|n| n == "task") {
            return;
        }
    }
    let spawner = std::sync::Arc::new(subagent::ConfigSpawner::new(config));
    tools.register(Box::new(pi_tools::task::TaskTool::new(spawner)));
}
pub use fs_watch::{WatchedState, WorkspaceWatcher};
pub use settings::PersistedSettings;
pub use skills::{Skill, SkillSet, SkillTrigger};
pub use slash::{SlashCommand, SlashOutcome, SlashRegistry};
pub use source_info::{detect as detect_source_info, SourceInfo};

#[derive(Debug, Clone, PartialEq)]
pub struct AgentTurn {
    pub session: Session,
    pub events: Vec<Event>,
    pub usage: Usage,
}

pub struct AgentRuntime<S: SessionStore> {
    config: AppConfig,
    session_store: S,
    tools: ToolRuntime,
    permissions: PermissionEngine,
    cancel: Arc<AtomicBool>,
    slash: SlashRegistry,
    skills: SkillSet,
    watcher: Option<WorkspaceWatcher>,
    /// Optional queue of MCP `notifications/progress` events captured by the
    /// host-side `EventQueueProgressHandler`. Drained into the turn's event
    /// vec right after each tool call returns.
    mcp_progress_queue: Option<Arc<std::sync::Mutex<Vec<Event>>>>,
}

impl<S: SessionStore> AgentRuntime<S> {
    pub fn new(config: AppConfig, session_store: S) -> Self {
        let mode = permission_mode(&config.permission_mode);
        let cwd = std::env::current_dir().ok();
        let skills = cwd
            .as_deref()
            .map(SkillSet::load_workspace)
            .unwrap_or_default();
        let mut slash = SlashRegistry::builtin();
        if let Some(cwd) = cwd.as_deref() {
            slash.load_custom(cwd);
        }
        let watcher = cwd.as_deref().map(WorkspaceWatcher::start);
        let mut tools = ToolRuntime::builtin();
        register_task_tool(&mut tools, config.clone());
        Self {
            config,
            session_store,
            tools,
            permissions: PermissionEngine::new(mode),
            cancel: Arc::new(AtomicBool::new(false)),
            slash,
            skills,
            watcher,
            mcp_progress_queue: None,
        }
    }

    pub fn set_mcp_progress_queue(&mut self, queue: Arc<std::sync::Mutex<Vec<Event>>>) {
        self.mcp_progress_queue = Some(queue);
    }

    /// Access to the session store. Lets the RPC server expose
    /// `list_sessions` without forcing every store impl into AgentRuntime.
    pub fn session_store(&self) -> &S {
        &self.session_store
    }

    pub fn try_new(config: AppConfig, session_store: S) -> PiResult<Self> {
        let mut tools = match &config.enabled_tool_names {
            Some(names) => ToolRuntime::builtin_with_names(names)?,
            None => ToolRuntime::builtin(),
        };
        register_task_tool(&mut tools, config.clone());

        let mode = permission_mode(&config.permission_mode);
        let cwd = std::env::current_dir().ok();
        let skills = cwd
            .as_deref()
            .map(SkillSet::load_workspace)
            .unwrap_or_default();
        let mut slash = SlashRegistry::builtin();
        if let Some(cwd) = cwd.as_deref() {
            slash.load_custom(cwd);
        }
        let watcher = cwd.as_deref().map(WorkspaceWatcher::start);
        Ok(Self {
            config,
            session_store,
            tools,
            permissions: PermissionEngine::new(mode),
            cancel: Arc::new(AtomicBool::new(false)),
            slash,
            skills,
            watcher,
            mcp_progress_queue: None,
        })
    }

    fn refresh_from_watcher(&mut self) {
        if let Some(watcher) = &self.watcher {
            let snapshot = watcher.state();
            // Swap the most recent disk state in.
            self.skills = snapshot.skills;
            self.slash = snapshot.slash;
        }
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut AppConfig {
        &mut self.config
    }

    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn reset_cancel(&self) {
        self.cancel.store(false, Ordering::SeqCst);
    }

    pub fn run_single_turn(&mut self, session_id: &str, prompt: &str) -> PiResult<AgentTurn> {
        self.run_single_turn_with_attachments(session_id, prompt, Vec::new())
    }

    pub fn run_single_turn_with_attachments(
        &mut self,
        session_id: &str,
        prompt: &str,
        attachments: Vec<pi_core::Attachment>,
    ) -> PiResult<AgentTurn> {
        self.reset_cancel();
        self.refresh_from_watcher();
        // pre-turn hook — aborts the whole turn on non-zero exit.
        if let Ok(cwd) = std::env::current_dir() {
            let ctx = hooks::HookContext {
                session_id: session_id.to_string(),
                prompt: Some(prompt.to_string()),
                ..hooks::HookContext::default()
            };
            let outcome = hooks::run(&cwd, hooks::HookPhase::PreTurn, &ctx)?;
            if outcome.ran && outcome.aborts(hooks::HookPhase::PreTurn) {
                return Err(PiError::new(
                    PiErrorKind::PermissionDenied,
                    format!(
                        "pre-turn hook 拒绝执行（exit {}）{}",
                        outcome.exit_code,
                        if outcome.stderr.is_empty() {
                            String::new()
                        } else {
                            format!("：{}", outcome.stderr.trim())
                        }
                    ),
                ));
            }
        }
        let mut events = vec![Event::UserMessage(prompt.to_string())];
        let session_existing = self.session_store.load(session_id)?;
        // Restore session's recorded cwd before the turn so file paths in the
        // transcript stay valid across `--resume` invocations from a different
        // shell location. We never error if the directory has moved — that's
        // expected when projects rename, just warn via status.
        if let Some(cwd) = session_existing.cwd() {
            if std::path::Path::new(cwd).exists() {
                let _ = std::env::set_current_dir(cwd);
            }
        }
        let mut session = session_existing;
        let mut user_message = Message::new(Role::User, prompt);
        user_message.attachments = attachments;
        self.session_store.append(session_id, &user_message)?;
        session.push(user_message);

        if let Some(outcome) = self.slash.handle(prompt) {
            for event in outcome.events {
                events.push(event);
            }
            if let Some(assistant) = outcome.assistant {
                let message = Message::new(Role::Assistant, assistant.clone());
                self.session_store.append(session_id, &message)?;
                session.push(message);
                events.push(Event::AssistantMessage(assistant));
            }
            return Ok(AgentTurn {
                session,
                events,
                usage: Usage::default(),
            });
        }

        if self.config.tools_enabled {
            if let Some(call) = parse_tool_shortcut(prompt) {
                run_tool_inline(
                    &self.tools,
                    &mut self.permissions,
                    call,
                    session_id,
                    &mut session,
                    &mut events,
                    &self.session_store,
                )?;
                return Ok(AgentTurn {
                    session,
                    events,
                    usage: Usage::default(),
                });
            }
        }

        let provider = provider_for(&self.config.model)?;
        let tool_schemas = if self.config.tools_enabled {
            self.tools.schemas()
        } else {
            Vec::new()
        };
        let system_prompt = self.system_prompt();

        let mut total_usage = Usage::default();
        for _ in 0..self.config.max_tool_steps.max(1) {
            if self.cancel.load(Ordering::SeqCst) {
                events.push(Event::Cancelled);
                return Err(PiError::new(PiErrorKind::Cancelled, "用户取消"));
            }

            let _compact_span = pi_core::timings::span_in("agent.compaction", "agent");
            if let Some(report) = maybe_compact(
                &mut session.messages,
                &*provider,
                &self.config,
                system_prompt.as_deref(),
            )? {
                self.session_store.append(
                    session_id,
                    &Message::new(
                        Role::System,
                        format!(
                            "[compaction] before={} after={}",
                            report.before, report.after
                        ),
                    ),
                )?;
                events.push(Event::Compacted {
                    before: report.before,
                    after: report.after,
                });
            }

            let request = ProviderRequest {
                model: self.config.model.clone(),
                messages: session.messages.clone(),
                tools: tool_schemas.clone(),
                system_prompt: system_prompt.clone(),
                max_output_tokens: None,
                temperature: None,
                stream: self.config.stream,
            };

            let mut sink = AgentSink {
                events: &mut events,
                cancel: self.cancel.clone(),
            };
            let _stream_span = pi_core::timings::span_in("provider.stream", "provider");
            let response = provider.stream(request, &mut sink)?;
            drop(_stream_span);
            total_usage.merge(&response.usage);
            if response.usage.total_tokens > 0 {
                events.push(Event::Usage(response.usage.clone()));
            }

            if response.tool_calls.is_empty() {
                let assistant_message = response.message.clone();
                let assistant_content = assistant_message.content.clone();
                events.push(Event::AssistantMessage(assistant_content.clone()));
                self.session_store.append(session_id, &assistant_message)?;
                session.push(assistant_message);
                // post-turn hook — advisory, exit code is logged but not fatal.
                if let Ok(cwd) = std::env::current_dir() {
                    let ctx = hooks::HookContext {
                        session_id: session_id.to_string(),
                        prompt: Some(prompt.to_string()),
                        tool_output: Some(assistant_content),
                        ..hooks::HookContext::default()
                    };
                    let _ = hooks::run(&cwd, hooks::HookPhase::PostTurn, &ctx);
                }
                return Ok(AgentTurn {
                    session,
                    events,
                    usage: total_usage,
                });
            }

            if !self.config.tools_enabled {
                return Err(PiError::new(
                    PiErrorKind::Tool,
                    "provider 请求调用工具，但当前已禁用工具",
                ));
            }

            // Persist assistant turn that carried tool calls so the provider
            // can use it on retry / resume.
            let assistant_turn = Message::assistant_with_tool_calls(
                response.message.content.clone(),
                response.tool_calls.clone(),
            );
            self.session_store.append(session_id, &assistant_turn)?;
            session.push(assistant_turn);

            for invocation in response.tool_calls {
                if self.cancel.load(Ordering::SeqCst) {
                    events.push(Event::Cancelled);
                    return Err(PiError::new(PiErrorKind::Cancelled, "用户取消"));
                }
                let tool_call_id = invocation.id.clone();
                let call = tool_call_from_invocation(invocation);
                // pre-tool hook — abort the call if the script exits non-zero.
                let hook_ctx = hooks::HookContext {
                    session_id: session_id.to_string(),
                    tool_name: Some(call.name.clone()),
                    tool_input: Some(call.input.clone()),
                    ..hooks::HookContext::default()
                };
                if let Ok(cwd) = std::env::current_dir() {
                    let outcome = hooks::run(&cwd, hooks::HookPhase::PreTool, &hook_ctx)?;
                    if outcome.ran && outcome.aborts(hooks::HookPhase::PreTool) {
                        events.push(Event::ToolError {
                            name: call.name.clone(),
                            error: format!(
                                "pre-tool hook 拒绝执行（exit {}）{}",
                                outcome.exit_code,
                                if outcome.stderr.is_empty() {
                                    String::new()
                                } else {
                                    format!("：{}", outcome.stderr.trim())
                                }
                            ),
                        });
                        let blocked_message = Message::tool_result(
                            tool_call_id.clone(),
                            format!("BLOCKED_BY_HOOK: pre-tool exit {}", outcome.exit_code),
                        );
                        self.session_store.append(session_id, &blocked_message)?;
                        session.push(blocked_message);
                        continue;
                    }
                }
                events.push(Event::ToolStarted {
                    name: call.name.clone(),
                    input: call.input.clone(),
                });
                let _tool_span = pi_core::timings::span_in(&format!("tool.{}", call.name), "tool");
                let output = match self.tools.run(call.clone(), &mut self.permissions) {
                    Ok(output) => output,
                    Err(err) => {
                        events.push(Event::ToolError {
                            name: call.name.clone(),
                            error: err.to_string(),
                        });
                        let error_message =
                            Message::tool_result(tool_call_id.clone(), format!("ERROR: {err}"));
                        self.session_store.append(session_id, &error_message)?;
                        session.push(error_message);
                        // Still run post-tool hook on error so observers can react.
                        if let Ok(cwd) = std::env::current_dir() {
                            let mut post_ctx = hook_ctx.clone();
                            post_ctx.tool_output = Some(format!("ERROR: {err}"));
                            let _ = hooks::run(&cwd, hooks::HookPhase::PostTool, &post_ctx);
                        }
                        continue;
                    }
                };
                // Drain any progress events captured by the MCP bridge while
                // the tool was running so the TUI sees them in order.
                if let Some(queue) = &self.mcp_progress_queue {
                    if let Ok(mut buf) = queue.lock() {
                        for ev in buf.drain(..) {
                            events.push(ev);
                        }
                    }
                }
                events.push(Event::ToolFinished {
                    name: output.name.clone(),
                    output: output.output.clone(),
                });
                if let Ok(cwd) = std::env::current_dir() {
                    let mut post_ctx = hook_ctx;
                    post_ctx.tool_output = Some(output.output.clone());
                    let _ = hooks::run(&cwd, hooks::HookPhase::PostTool, &post_ctx);
                }
                let tool_message = Message::tool_result(tool_call_id, output.output);
                self.session_store.append(session_id, &tool_message)?;
                session.push(tool_message);
            }
        }

        Err(PiError::new(
            PiErrorKind::Provider,
            "provider 工具调用超过最大轮数",
        ))
    }

    pub fn tool_schemas(&self) -> Vec<pi_core::ToolSchema> {
        self.tools.schemas()
    }

    /// Mutable access to the underlying tool runtime so callers can plug in
    /// extension or MCP tools after construction.
    pub fn tools_mut(&mut self) -> &mut ToolRuntime {
        &mut self.tools
    }

    pub fn slash_commands(&self) -> Vec<&SlashCommand> {
        self.slash.list()
    }

    fn system_prompt(&self) -> Option<String> {
        let base = self
            .config
            .system_prompt
            .clone()
            .unwrap_or_else(|| system_prompt::default(&self.config, &self.tools.schemas()));
        let skills_section = self.skills.always_prompt();
        if skills_section.is_empty() {
            Some(base)
        } else {
            Some(format!("{base}{skills_section}"))
        }
    }
}

fn permission_mode(kind: &PermissionModeKind) -> PermissionMode {
    match kind {
        PermissionModeKind::ReadOnly => PermissionMode::ReadOnly,
        PermissionModeKind::ConfirmMutations => PermissionMode::ConfirmMutations,
        PermissionModeKind::TrustedWorkspace => PermissionMode::TrustedWorkspace,
        PermissionModeKind::Plan => PermissionMode::ReadOnly,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_tool_inline<S: SessionStore>(
    tools: &ToolRuntime,
    permissions: &mut PermissionEngine,
    call: ToolCall,
    session_id: &str,
    session: &mut Session,
    events: &mut Vec<Event>,
    session_store: &S,
) -> PiResult<()> {
    let hook_ctx = hooks::HookContext {
        session_id: session_id.to_string(),
        tool_name: Some(call.name.clone()),
        tool_input: Some(call.input.clone()),
        ..hooks::HookContext::default()
    };
    if let Ok(cwd) = std::env::current_dir() {
        let outcome = hooks::run(&cwd, hooks::HookPhase::PreTool, &hook_ctx)?;
        if outcome.ran && outcome.aborts(hooks::HookPhase::PreTool) {
            let err = format!(
                "pre-tool hook 拒绝执行（exit {}）{}",
                outcome.exit_code,
                if outcome.stderr.is_empty() {
                    String::new()
                } else {
                    format!("：{}", outcome.stderr.trim())
                }
            );
            events.push(Event::ToolError {
                name: call.name.clone(),
                error: err.clone(),
            });
            return Err(PiError::new(PiErrorKind::PermissionDenied, err));
        }
    }
    events.push(Event::ToolStarted {
        name: call.name.clone(),
        input: call.input.clone(),
    });
    let output = tools.run(call.clone(), permissions)?;
    events.push(Event::ToolFinished {
        name: output.name.clone(),
        output: output.output.clone(),
    });
    if let Ok(cwd) = std::env::current_dir() {
        let mut post_ctx = hook_ctx;
        post_ctx.tool_output = Some(output.output.clone());
        let _ = hooks::run(&cwd, hooks::HookPhase::PostTool, &post_ctx);
    }
    let tool_message = Message::new(Role::Tool, output.output);
    session_store.append(session_id, &tool_message)?;
    session.push(tool_message);
    Ok(())
}

struct AgentSink<'a> {
    events: &'a mut Vec<Event>,
    cancel: Arc<AtomicBool>,
}

impl<'a> StreamSink for AgentSink<'a> {
    fn emit(&mut self, event: StreamEvent) -> PiResult<()> {
        match &event {
            StreamEvent::TextDelta(delta) => {
                self.events.push(Event::AssistantDelta(delta.clone()));
            }
            StreamEvent::ThinkingDelta(delta) => {
                self.events.push(Event::ThinkingDelta(delta.clone()));
            }
            StreamEvent::UsageDelta(usage) => {
                self.events.push(Event::Usage(usage.clone()));
            }
            _ => {}
        }
        self.events.push(Event::ProviderStream(event));
        Ok(())
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

fn tool_call_from_invocation(invocation: ToolInvocation) -> ToolCall {
    ToolCall {
        name: invocation.name,
        input: invocation.input,
    }
}

fn parse_tool_shortcut(prompt: &str) -> Option<ToolCall> {
    let rest = prompt.strip_prefix("/tool ")?;
    let (name, input) = rest.split_once(' ')?;
    Some(ToolCall {
        name: name.to_string(),
        input: input.to_string(),
    })
}

/// Estimate the prompt token footprint of the messages we would send.
pub fn estimate_request_tokens(messages: &[Message]) -> u32 {
    estimate_messages_tokens(messages)
}

/// Build a one-off provider request without running the full agent. Useful for
/// the compaction path and for SDK callers that want to drive their own loop.
pub fn build_request(
    config: &AppConfig,
    messages: Vec<Message>,
    tools: Vec<pi_core::ToolSchema>,
    system_prompt: Option<String>,
) -> ProviderRequest {
    ProviderRequest {
        model: config.model.clone(),
        messages,
        tools,
        system_prompt,
        max_output_tokens: None,
        temperature: None,
        stream: config.stream,
    }
}

/// Re-export for callers that want to drive provider directly.
pub fn run_provider(
    provider: &dyn Provider,
    request: ProviderRequest,
) -> PiResult<ProviderResponse> {
    provider.complete(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tool_shortcut_handles_basic_form() {
        let call = parse_tool_shortcut("/tool ls .").expect("call");
        assert_eq!(call.name, "ls");
        assert_eq!(call.input, ".");
    }

    #[test]
    fn parse_tool_shortcut_returns_none_for_plain_prompt() {
        assert!(parse_tool_shortcut("hello").is_none());
    }
}
