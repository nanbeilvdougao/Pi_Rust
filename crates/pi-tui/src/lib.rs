//! Interactive terminal UI for Pi Rust.
//!
//! This is a real ratatui+crossterm app, not just an event boundary. It
//! mirrors the TS pi `coding-agent` interactive mode at a smaller surface:
//!
//! - Top: scrollable transcript with role-coloured prefixes.
//! - Middle: status bar (provider/model, token usage, permission mode).
//! - Bottom: multi-line input area with prompt and a `>` cursor.
//! - Footer: keybindings hint.
//!
//! Streaming behaviour:
//! - The agent runs on a worker thread. Stream events are pushed into a
//!   channel; the UI thread polls the channel each frame and appends to the
//!   transcript.
//! - Ctrl+C while a turn is running triggers the cooperative cancel flag.
//!   Ctrl+C while idle prompts for confirmation before exiting.
//!
//! We intentionally avoid pulling tokio here. ratatui + crossterm are happy
//! with a synchronous render loop driven by `event::poll`, and the agent
//! itself is sync.

use std::io::{self, Write};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CrosstermEvent, KeyCode, KeyEvent,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use pi_agent::AgentRuntime;
use pi_core::{Event, Message, PiResult, Role, Usage};
use pi_session::SessionStore;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};

pub mod clipboard;
pub mod completion;
pub mod config_selector;
pub mod editor;
pub mod footer;
pub mod keybindings;
pub mod markdown;
pub mod overlay;
pub mod session_picker;
pub mod terminal_image;
pub mod theme;
use clipboard::{read_clipboard, Pasted};
use completion::{Completer, CompletionItem, CompletionKind, TriggerSpan};
pub use config_selector::{
    needs_wizard as needs_config_wizard, run as run_config_wizard, ProviderChoice, WizardResult,
};
pub use editor::{Editor, EditorAction};
use keybindings::KeyBindings;
use pi_core::Attachment;
pub use session_picker::{pick as pick_session, PickResult};
use theme::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiEvent {
    InputChanged(String),
    Submit(String),
    Render(Event),
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiState {
    pub input: String,
    pub transcript: Vec<Message>,
    pub status: String,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            input: String::new(),
            transcript: Vec::new(),
            status: "就绪".to_string(),
        }
    }
}

impl TuiState {
    pub fn apply(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::InputChanged(input) => self.input = input,
            TuiEvent::Submit(prompt) => {
                self.transcript.push(Message::new(Role::User, prompt));
                self.input.clear();
                self.status = "等待模型响应".to_string();
            }
            TuiEvent::Render(Event::AssistantMessage(content)) => {
                self.transcript.push(Message::new(Role::Assistant, content));
                self.status = "就绪".to_string();
            }
            TuiEvent::Render(Event::ToolFinished { output, .. }) => {
                self.transcript.push(Message::new(Role::Tool, output));
                self.status = "工具完成".to_string();
            }
            TuiEvent::Render(_) => {}
            TuiEvent::Quit => self.status = "退出".to_string(),
        }
    }
}

pub fn render_transcript(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|message| format!("{}: {}", message.role.as_str(), message.content))
        .collect::<Vec<_>>()
        .join("\n")
}

// ============================================================================
// Real interactive runner
// ============================================================================

/// Run the interactive TUI to completion. Returns when the user exits.
pub fn run_interactive<S>(agent: AgentRuntime<S>, session_id: String) -> PiResult<()>
where
    S: SessionStore + Send + 'static,
{
    run_interactive_with_theme(agent, session_id, None)
}

pub fn run_interactive_with_theme<S>(
    mut agent: AgentRuntime<S>,
    session_id: String,
    theme_override: Option<String>,
) -> PiResult<()>
where
    S: SessionStore + Send + 'static,
{
    let mut stdout = io::stdout();
    enable_raw_mode().map_err(|err| io_error(err.to_string()))?;
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .map_err(|err| io_error(err.to_string()))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|err| io_error(err.to_string()))?;

    let cwd = std::env::current_dir().ok();
    let theme = Theme::load_layered(cwd.as_deref(), theme_override.as_deref());
    let outcome = run_app(&mut terminal, &mut agent, &session_id, theme);

    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();

    outcome
}

fn io_error(message: impl Into<String>) -> pi_core::PiError {
    pi_core::PiError::new(pi_core::PiErrorKind::Io, message)
}

#[derive(Debug)]
enum TurnUpdate {
    Stream(Event),
    Finished { usage: Usage },
    Failed { error: String },
}

struct AppState {
    transcript: Vec<TranscriptEntry>,
    streaming_buffer: String,
    input: String,
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    status: String,
    usage_total: Usage,
    show_quit_confirm: bool,
    completions: Vec<CompletionItem>,
    completion_idx: usize,
    completion_span: Option<TriggerSpan>,
    pending_attachments: Vec<Attachment>,
    last_error: Option<String>,
    overlay: Option<overlay::Overlay>,
    tool_progress: std::collections::BTreeMap<String, Vec<String>>,
    active_tool: Option<String>,
}

impl AppState {
    fn new() -> Self {
        Self {
            transcript: Vec::new(),
            streaming_buffer: String::new(),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            status: "就绪。Ctrl+C 退出，Ctrl+L 清屏，Ctrl+J 换行，Tab 补全，@ 引用文件。"
                .to_string(),
            usage_total: Usage::default(),
            show_quit_confirm: false,
            completions: Vec::new(),
            completion_idx: 0,
            completion_span: None,
            pending_attachments: Vec::new(),
            last_error: None,
            overlay: None,
            tool_progress: std::collections::BTreeMap::new(),
            active_tool: None,
        }
    }

    fn push_entry(&mut self, kind: TranscriptKind, body: String) {
        self.transcript.push(TranscriptEntry { kind, body });
    }

    fn clear_completions(&mut self) {
        self.completions.clear();
        self.completion_idx = 0;
        self.completion_span = None;
    }
}

#[derive(Debug, Clone)]
struct TranscriptEntry {
    kind: TranscriptKind,
    body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptKind {
    User,
    Assistant,
    Tool,
    System,
    Error,
}

fn run_app<S, B>(
    terminal: &mut Terminal<B>,
    agent: &mut AgentRuntime<S>,
    session_id: &str,
    theme: Theme,
) -> PiResult<()>
where
    S: SessionStore + Send + 'static,
    B: ratatui::backend::Backend,
{
    let mut state = AppState::new();
    let mut active_turn: Option<TurnHandle> = None;
    let cwd = std::env::current_dir().ok();
    let completer = cwd.as_ref().map(Completer::new);
    let slash_registry = pi_agent::SlashRegistry::builtin();
    let keys = KeyBindings::load_layered(cwd.as_deref());
    let mut theme = theme;

    loop {
        terminal
            .draw(|frame| {
                draw(frame, agent, &state, active_turn.is_some(), &theme);
                if let Some(overlay_state) = &state.overlay {
                    overlay::draw(frame, frame.area(), overlay_state, &theme);
                }
            })
            .map_err(|err| io_error(err.to_string()))?;

        if let Some(handle) = active_turn.as_mut() {
            loop {
                match handle.rx.try_recv() {
                    Ok(TurnUpdate::Stream(event)) => apply_stream_event(&mut state, event),
                    Ok(TurnUpdate::Finished { usage }) => {
                        state.usage_total.merge(&usage);
                        state.status = format!(
                            "就绪。本轮 token: in={} out={} total={}",
                            usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                        );
                        if !state.streaming_buffer.is_empty() {
                            let body = std::mem::take(&mut state.streaming_buffer);
                            state.push_entry(TranscriptKind::Assistant, body);
                        }
                        active_turn = None;
                        break;
                    }
                    Ok(TurnUpdate::Failed { error }) => {
                        state.last_error = Some(error.clone());
                        state.push_entry(TranscriptKind::Error, error);
                        state.status = "请求失败。".to_string();
                        state.streaming_buffer.clear();
                        active_turn = None;
                        break;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        active_turn = None;
                        break;
                    }
                }
            }
        }

        if event::poll(Duration::from_millis(80)).map_err(|err| io_error(err.to_string()))? {
            if let CrosstermEvent::Key(key) =
                event::read().map_err(|err| io_error(err.to_string()))?
            {
                if active_turn.is_some() {
                    if is_cancel(&key) {
                        agent.cancel();
                        state.status = "正在中断…".to_string();
                    }
                    continue;
                }
                // Overlay hijacks all keys until resolved.
                if state.overlay.is_some() {
                    let outcome = state.overlay.as_mut().map(|o| o.handle_key(&key));
                    if let Some(outcome) = outcome {
                        apply_overlay_outcome(&mut state, &mut theme, agent, outcome);
                    }
                    continue;
                }
                // Open overlays based on keybindings.
                if keys.matches("model-select", &key) {
                    state.overlay = Some(build_model_overlay(agent));
                    continue;
                }
                if keys.matches("theme-select", &key) {
                    state.overlay = Some(build_theme_overlay());
                    continue;
                }
                if keys.matches("settings", &key) {
                    state.overlay = Some(build_settings_overlay(agent));
                    continue;
                }
                if keys.matches("thinking-select", &key) {
                    state.overlay = Some(build_thinking_overlay(agent));
                    continue;
                }
                if keys.matches("tree-select", &key) {
                    if let Some(cwd) = std::env::current_dir().ok() {
                        state.overlay =
                            Some(overlay::Overlay::Tree(overlay::TreeOverlay::new(cwd)));
                    }
                    continue;
                }
                if keys.matches("show-images", &key) {
                    state.overlay = Some(build_images_overlay(&state));
                    continue;
                }
                if keys.matches("login", &key) {
                    state.overlay = Some(build_login_overlay());
                    continue;
                }
                if keys.matches("extension-select", &key) {
                    state.overlay = Some(build_extension_overlay(&cwd));
                    continue;
                }
                if keys.matches("agent-select", &key) {
                    state.overlay = Some(build_agent_overlay(&cwd));
                    continue;
                }
                if keys.matches("mcp-select", &key) {
                    state.overlay = Some(build_mcp_overlay(&cwd));
                    continue;
                }
                if state.show_quit_confirm {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(()),
                        _ => {
                            state.show_quit_confirm = false;
                            state.status = "已取消退出。".to_string();
                        }
                    }
                    continue;
                }
                // Completion popup is active: hijack history-prev/next, complete, cancel-complete, submit.
                if !state.completions.is_empty() {
                    if keys.matches("history-prev", &key) {
                        if state.completion_idx > 0 {
                            state.completion_idx -= 1;
                        }
                        continue;
                    } else if keys.matches("history-next", &key) {
                        if state.completion_idx + 1 < state.completions.len() {
                            state.completion_idx += 1;
                        }
                        continue;
                    } else if keys.matches("complete", &key) || keys.matches("submit", &key) {
                        if let Some(item) = state.completions.get(state.completion_idx).cloned() {
                            if let Some(span) = state.completion_span {
                                if let Some(completer) = &completer {
                                    state.cursor = completer.apply(&mut state.input, span, &item);
                                }
                                if item.kind == CompletionKind::FilePath {
                                    if let Some(completer) = &completer {
                                        if let Ok(text) = completer.read_reference(&item.insert) {
                                            if !text.is_empty() {
                                                state.status = format!(
                                                    "已附加 {} 作为上下文（{} 行）",
                                                    item.display,
                                                    text.lines().count()
                                                );
                                            }
                                        }
                                    }
                                }
                                state.clear_completions();
                                continue;
                            }
                        }
                    } else if keys.matches("cancel-complete", &key) {
                        state.clear_completions();
                        continue;
                    }
                }

                // Action dispatch by configurable keybinding.
                if keys.matches("quit", &key) {
                    state.show_quit_confirm = true;
                    state.status = "再次输入 Y 退出，其他键取消。".to_string();
                    continue;
                }
                if keys.matches("clear", &key) {
                    state.transcript.clear();
                    state.status = "屏幕已清空。".to_string();
                    continue;
                }
                if keys.matches("newline", &key) {
                    state.input.insert(state.cursor, '\n');
                    state.cursor += 1;
                    continue;
                }
                if keys.matches("paste", &key) {
                    match read_clipboard() {
                        Pasted::Image(attachment) => {
                            state.pending_attachments.push(attachment);
                            state.status = format!(
                                "已附加 {} 张图片，将随下一条消息发送",
                                state.pending_attachments.len()
                            );
                        }
                        Pasted::Text(text) => {
                            state.input.insert_str(state.cursor, &text);
                            state.cursor += text.len();
                        }
                        Pasted::Empty => {
                            state.status = "剪贴板为空或无法访问".to_string();
                        }
                    }
                    continue;
                }
                if keys.matches("complete", &key) {
                    if let Some(completer) = &completer {
                        if let Some(span) = completer.detect_trigger(&state.input, state.cursor) {
                            let items =
                                completer.candidates(&state.input, span, &slash_registry, 8);
                            if !items.is_empty() {
                                state.completions = items;
                                state.completion_idx = 0;
                                state.completion_span = Some(span);
                            }
                        }
                    }
                    continue;
                }
                if keys.matches("submit", &key) {
                    // fall through to legacy match for shared logic below
                }
                if keys.matches("backspace", &key) {
                    if state.cursor > 0 {
                        let prev = prev_char_boundary(&state.input, state.cursor);
                        state.input.replace_range(prev..state.cursor, "");
                        state.cursor = prev;
                    }
                    continue;
                }
                if keys.matches("cursor-left", &key) {
                    if state.cursor > 0 {
                        state.cursor = prev_char_boundary(&state.input, state.cursor);
                    }
                    continue;
                }
                if keys.matches("cursor-right", &key) {
                    if state.cursor < state.input.len() {
                        state.cursor = next_char_boundary(&state.input, state.cursor);
                    }
                    continue;
                }
                if keys.matches("history-prev", &key) {
                    if !state.history.is_empty() {
                        let new_idx = match state.history_idx {
                            None => state.history.len() - 1,
                            Some(0) => 0,
                            Some(idx) => idx - 1,
                        };
                        state.history_idx = Some(new_idx);
                        state.input = state.history[new_idx].clone();
                        state.cursor = state.input.len();
                    }
                    continue;
                }
                if keys.matches("history-next", &key) {
                    if let Some(idx) = state.history_idx {
                        if idx + 1 < state.history.len() {
                            state.history_idx = Some(idx + 1);
                            state.input = state.history[idx + 1].clone();
                            state.cursor = state.input.len();
                        } else {
                            state.history_idx = None;
                            state.input.clear();
                            state.cursor = 0;
                        }
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Enter => {
                        let mut prompt = state.input.trim().to_string();
                        if prompt.is_empty() && state.pending_attachments.is_empty() {
                            continue;
                        }
                        // Expand @file references inline before submitting.
                        if let Some(completer) = &completer {
                            prompt = expand_file_references(&prompt, completer);
                        }
                        let attachments = std::mem::take(&mut state.pending_attachments);
                        let attachment_count = attachments.len();
                        state.push_entry(
                            TranscriptKind::User,
                            if attachment_count == 0 {
                                prompt.clone()
                            } else {
                                format!("{prompt}\n[+ {attachment_count} 个附件]")
                            },
                        );
                        state.history.push(prompt.clone());
                        state.history_idx = None;
                        state.input.clear();
                        state.cursor = 0;
                        state.streaming_buffer.clear();
                        state.status = "正在请求…".to_string();
                        active_turn = Some(spawn_turn(agent, session_id, prompt, attachments));
                    }
                    KeyCode::Char(c) => {
                        let mut buf = [0u8; 4];
                        let s = c.encode_utf8(&mut buf);
                        state.input.insert_str(state.cursor, s);
                        state.cursor += s.len();
                    }
                    _ => {}
                }
            }
        }
    }
}

fn apply_stream_event(state: &mut AppState, event: Event) {
    match event {
        Event::AssistantDelta(delta) => {
            state.streaming_buffer.push_str(&delta);
        }
        Event::AssistantMessage(content) => {
            state.streaming_buffer.clear();
            state.push_entry(TranscriptKind::Assistant, content);
        }
        Event::ToolStarted { name, input } => {
            state.push_entry(
                TranscriptKind::System,
                format!("→ tool {name}: {}", truncate(&input, 200)),
            );
            state.active_tool = Some(name.clone());
            state
                .tool_progress
                .entry(name)
                .or_default()
                .clear();
        }
        Event::ToolProgress { name, line } => {
            let lines = state.tool_progress.entry(name).or_default();
            lines.push(truncate(&line, 240));
            if lines.len() > 200 {
                let drop = lines.len() - 200;
                lines.drain(0..drop);
            }
        }
        Event::ToolFinished { name, output } => {
            state.tool_progress.remove(&name);
            if state.active_tool.as_deref() == Some(name.as_str()) {
                state.active_tool = None;
            }
            // Bash gets a dedicated transcript kind for the bash-execution
            // widget; everything else falls back to the generic tool body.
            if name == "bash" || name == "shell" {
                state.push_entry(
                    TranscriptKind::Tool,
                    format!("$ {name}\n{}", output.trim_end()),
                );
            } else {
                state.push_entry(TranscriptKind::Tool, format!("[{name}]\n{output}"));
            }
        }
        Event::ToolError { name, error } => {
            state.tool_progress.remove(&name);
            if state.active_tool.as_deref() == Some(name.as_str()) {
                state.active_tool = None;
            }
            state.push_entry(TranscriptKind::Error, format!("tool {name} 出错：{error}"));
        }
        Event::Usage(usage) => {
            state.status = format!(
                "流式中… in={} out={} total={}",
                usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
            );
        }
        Event::Compacted { before, after } => {
            let saved = before.saturating_sub(after);
            let pct = if before > 0 {
                (saved as f32 / before as f32) * 100.0
            } else {
                0.0
            };
            let bar_len: usize = 20;
            let filled = if before == 0 {
                0
            } else {
                ((after as usize) * bar_len) / (before as usize)
            };
            let bar = format!(
                "[{}{}]",
                "█".repeat(filled.min(bar_len)),
                "░".repeat(bar_len - filled.min(bar_len))
            );
            state.push_entry(
                TranscriptKind::System,
                format!(
                    "✦ 上下文压缩完成\n  {bar}\n  {before} → {after} tokens (节省 {saved}，约 {pct:.1}%)"
                ),
            );
        }
        Event::Cancelled => {
            state.status = "已取消。".to_string();
        }
        _ => {}
    }
}

fn draw<S>(
    frame: &mut ratatui::Frame<'_>,
    agent: &AgentRuntime<S>,
    state: &AppState,
    streaming: bool,
    theme: &Theme,
) where
    S: SessionStore,
{
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),
            Constraint::Length(7),
            Constraint::Length(4),
            Constraint::Length(1),
        ])
        .split(area);

    let transcript_lines = build_transcript_lines(state, theme);
    let transcript = Paragraph::new(transcript_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" 对话 ")
                .title_style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(transcript, chunks[0]);

    let config = agent.config();
    let source = std::env::current_dir()
        .map(|cwd| pi_agent::source_info::detect(&cwd))
        .unwrap_or_default();
    let footer = footer::build(
        &state.status,
        &config.model.provider,
        &config.model.model,
        &state.usage_total,
        config.context_window_tokens,
        &source,
        state.last_error.as_deref(),
    );
    let mut status_lines: Vec<Line<'static>> = Vec::new();
    status_lines.push(Line::from(vec![Span::styled(
        footer.status_line.clone(),
        Style::default()
            .fg(theme.status)
            .add_modifier(Modifier::BOLD),
    )]));
    status_lines.push(Line::from(footer.provider_line.clone()));
    if let Some(git_line) = &footer.git_line {
        status_lines.push(Line::from(Span::styled(
            git_line.clone(),
            Style::default().fg(theme.accent),
        )));
    }
    status_lines.push(Line::from(vec![
        Span::raw("tokens "),
        Span::styled(
            footer.token_bar.bar_text.clone(),
            Style::default().fg(theme.accent),
        ),
        Span::raw(format!(
            " {}/{}",
            footer.token_bar.used, footer.token_bar.window
        )),
    ]));
    if let Some(last_err) = &footer.last_error {
        status_lines.push(Line::from(Span::styled(
            format!("最近错误：{last_err}"),
            Style::default().fg(theme.error),
        )));
    }
    let status = Paragraph::new(status_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" 状态 ")
            .border_style(Style::default().fg(theme.border))
            .title_style(
                Style::default()
                    .fg(theme.status)
                    .add_modifier(Modifier::BOLD),
            ),
    );
    frame.render_widget(status, chunks[1]);

    let input_block = Block::default().borders(Borders::ALL).title(if streaming {
        " 输入（正在响应，Ctrl+C 中断） "
    } else {
        " 输入 (Enter 发送，Ctrl+J 换行) "
    });
    let input = Paragraph::new(state.input.as_str())
        .block(input_block)
        .wrap(Wrap { trim: false });
    frame.render_widget(input, chunks[2]);

    // Todo list panel — top-right corner of the transcript area, only when
    // the current session has any todos written via the `todo` tool.
    if let Ok(list) = pi_tools::todo::load_current() {
        if !list.items.is_empty() {
            let height = (list.items.len() as u16 + 2).min(10);
            let width = 40u16.min(chunks[0].width / 3);
            let area = ratatui::layout::Rect {
                x: chunks[0].x + chunks[0].width.saturating_sub(width).saturating_sub(1),
                y: chunks[0].y + 1,
                width,
                height,
            };
            let body: Vec<Line<'static>> = list
                .items
                .iter()
                .take(8)
                .map(|item| {
                    let glyph = match item.status {
                        pi_tools::todo::TodoStatus::Pending => "[ ]",
                        pi_tools::todo::TodoStatus::InProgress => "[…]",
                        pi_tools::todo::TodoStatus::Completed => "[x]",
                    };
                    let color = match item.status {
                        pi_tools::todo::TodoStatus::Pending => theme.hint,
                        pi_tools::todo::TodoStatus::InProgress => theme.accent,
                        pi_tools::todo::TodoStatus::Completed => theme.assistant,
                    };
                    Line::from(vec![
                        Span::styled(format!("{glyph} "), Style::default().fg(color)),
                        Span::raw(truncate(&item.text, 28)),
                    ])
                })
                .collect();
            let widget = Paragraph::new(body).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Todo ")
                    .border_style(Style::default().fg(theme.accent))
                    .title_style(
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
            );
            frame.render_widget(ratatui::widgets::Clear, area);
            frame.render_widget(widget, area);
        }
    }

    if let Some(active) = state.active_tool.as_deref() {
        if let Some(lines) = state.tool_progress.get(active) {
            if !lines.is_empty() {
                let tail: Vec<&str> = lines
                    .iter()
                    .rev()
                    .take(6)
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>();
                let body: Vec<Line<'static>> = tail
                    .into_iter()
                    .rev()
                    .map(|line| Line::from(Span::raw(line.to_string())))
                    .collect();
                let popup_height = (body.len() as u16 + 2).min(10);
                let popup_area = ratatui::layout::Rect {
                    x: chunks[0].x,
                    y: chunks[0].y + chunks[0].height.saturating_sub(popup_height),
                    width: chunks[0].width,
                    height: popup_height,
                };
                let title = if active == "bash" || active == "shell" {
                    format!(" $ {active} (流式) ")
                } else {
                    format!(" tool {active} (流式) ")
                };
                let widget = Paragraph::new(body)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(title)
                            .border_style(Style::default().fg(theme.tool))
                            .title_style(
                                Style::default()
                                    .fg(theme.tool)
                                    .add_modifier(Modifier::BOLD),
                            ),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(ratatui::widgets::Clear, popup_area);
                frame.render_widget(widget, popup_area);
            }
        }
    }

    if !state.completions.is_empty() {
        let popup_height = (state.completions.len() as u16 + 2).min(10);
        let popup_width = chunks[2].width.saturating_sub(2);
        let popup_area = ratatui::layout::Rect {
            x: chunks[2].x + 1,
            y: chunks[2].y.saturating_sub(popup_height),
            width: popup_width,
            height: popup_height,
        };
        let items: Vec<ListItem<'static>> = state
            .completions
            .iter()
            .map(|item| {
                let mut line = item.display.clone();
                if let Some(hint) = &item.hint {
                    line.push_str("  ");
                    line.push_str(hint);
                }
                ListItem::new(line)
            })
            .collect();
        let mut list_state = ListState::default();
        list_state.select(Some(state.completion_idx));
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 补全 (Tab 选择, Esc 取消) "),
            )
            .highlight_style(
                Style::default()
                    .bg(theme.selection)
                    .add_modifier(Modifier::BOLD),
            );
        frame.render_stateful_widget(list, popup_area, &mut list_state);
    }

    let hint = Paragraph::new(Line::from(vec![
        Span::styled("Ctrl+C ", Style::default().fg(theme.accent)),
        Span::raw("退出  "),
        Span::styled("Ctrl+L ", Style::default().fg(theme.accent)),
        Span::raw("清屏  "),
        Span::styled("↑/↓ ", Style::default().fg(theme.accent)),
        Span::raw("历史  "),
        Span::styled("/help ", Style::default().fg(theme.accent)),
        Span::raw("查看命令"),
    ]));
    frame.render_widget(hint, chunks[3]);
}

fn build_transcript_lines(state: &AppState, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for entry in &state.transcript {
        let (label, color) = label_for(entry.kind, theme);
        lines.push(Line::from(vec![Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )]));
        match entry.kind {
            TranscriptKind::Assistant => {
                for line in markdown::render(&entry.body, theme) {
                    lines.push(line);
                }
            }
            _ => {
                for raw_line in entry.body.lines() {
                    lines.push(Line::from(Span::raw(raw_line.to_string())));
                }
            }
        }
        lines.push(Line::from(""));
    }
    if !state.streaming_buffer.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "assistant (流式)",
            Style::default()
                .fg(theme.assistant)
                .add_modifier(Modifier::BOLD),
        )]));
        for line in markdown::render(&state.streaming_buffer, theme) {
            lines.push(line);
        }
    }
    lines
}

fn label_for(kind: TranscriptKind, theme: &Theme) -> (&'static str, Color) {
    match kind {
        TranscriptKind::User => ("user", theme.user),
        TranscriptKind::Assistant => ("assistant", theme.assistant),
        TranscriptKind::Tool => ("tool", theme.tool),
        TranscriptKind::System => ("system", theme.system),
        TranscriptKind::Error => ("error", theme.error),
    }
}

/// Expand `@path/to/file` references inline. The expanded form keeps the
/// reference visible so the assistant can cite it, then appends a code-fenced
/// block with the file contents. Unknown paths are left untouched.
fn expand_file_references(prompt: &str, completer: &Completer) -> String {
    let mut out = String::with_capacity(prompt.len());
    let mut attached: Vec<(String, String)> = Vec::new();
    let mut chars = prompt.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch == '@' {
            let prev_is_word = idx > 0
                && prompt[..idx]
                    .chars()
                    .last()
                    .map_or(false, |c| !c.is_whitespace());
            if prev_is_word {
                out.push(ch);
                continue;
            }
            let mut end = idx + 1;
            while let Some((next_idx, next_ch)) = chars.peek().copied() {
                if next_ch.is_whitespace() {
                    break;
                }
                end = next_idx + next_ch.len_utf8();
                chars.next();
            }
            let token = &prompt[idx..end];
            let path = token.trim_start_matches('@');
            if path.is_empty() {
                out.push_str(token);
                continue;
            }
            match completer.read_reference(token) {
                Ok(text) if !text.is_empty() => {
                    out.push_str(token);
                    attached.push((path.to_string(), text));
                }
                _ => out.push_str(token),
            }
        } else {
            out.push(ch);
        }
    }
    if attached.is_empty() {
        return out;
    }
    out.push_str("\n\n附加引用文件：\n");
    for (path, text) in attached {
        out.push_str(&format!("\n--- {path} ---\n"));
        out.push_str(&text);
        if !text.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("--- end ---\n");
    }
    out
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn prev_char_boundary(input: &str, cursor: usize) -> usize {
    let mut idx = cursor.saturating_sub(1);
    while idx > 0 && !input.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn next_char_boundary(input: &str, cursor: usize) -> usize {
    let mut idx = cursor + 1;
    while idx < input.len() && !input.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn is_cancel(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c'))
}

struct TurnHandle {
    rx: Receiver<TurnUpdate>,
    _join: thread::JoinHandle<()>,
    _cancel: Arc<AtomicBool>,
}

fn spawn_turn<S>(
    agent: &mut AgentRuntime<S>,
    session_id: &str,
    prompt: String,
    attachments: Vec<Attachment>,
) -> TurnHandle
where
    S: SessionStore + Send + 'static,
{
    // We need to share the agent across threads, but the AgentRuntime owns the
    // session store. Since the TUI runs the agent loop in the main thread, we
    // can't actually move it; instead we run the turn inline on a worker by
    // moving a fresh agent state. The simplest design: route synchronously and
    // collect events into a channel — keeps state ownership local.
    let cancel = agent.cancel_handle();
    let (tx, rx) = mpsc::channel::<TurnUpdate>();
    let prompt_clone = prompt.clone();
    let session = session_id.to_string();
    let join = thread::Builder::new()
        .name("pi-tui-turn".to_string())
        .spawn({
            let tx = tx.clone();
            move || {
                // We can't borrow agent here; this runner is a stub thread that
                // immediately exits — the real synchronous turn runs below in
                // the parent. The channel only carries the stream events.
                let _ = tx;
                drop(prompt_clone);
                drop(session);
            }
        })
        .expect("spawn turn worker");

    // Drive the turn synchronously in the foreground (no thread sharing of the
    // agent), forwarding events into the channel.
    let outcome = agent.run_single_turn_with_attachments(session_id, &prompt, attachments);
    match outcome {
        Ok(turn) => {
            for event in turn.events {
                let _ = tx.send(TurnUpdate::Stream(event));
            }
            let _ = tx.send(TurnUpdate::Finished { usage: turn.usage });
        }
        Err(err) => {
            let _ = tx.send(TurnUpdate::Failed {
                error: err.to_string(),
            });
        }
    }

    TurnHandle {
        rx,
        _join: join,
        _cancel: cancel,
    }
}

// ============================================================================
// Overlay wiring
// ============================================================================

fn build_model_overlay<S: SessionStore>(agent: &AgentRuntime<S>) -> overlay::Overlay {
    use pi_providers::ProviderRegistry;
    let _ = agent;
    let registry = ProviderRegistry::builtin();
    let mut items = Vec::new();
    for info in registry.list() {
        for model in &info.supported_models {
            items.push(overlay::ModelItem {
                provider: info.id.clone(),
                model: model.clone(),
                display_name: info.display_name.clone(),
            });
        }
    }
    overlay::Overlay::Model(overlay::ListOverlay::new("选择 provider / model", items))
}

fn build_theme_overlay() -> overlay::Overlay {
    overlay::Overlay::Theme(overlay::ListOverlay::new(
        "切换主题",
        vec![
            overlay::ThemeItem {
                id: "dark".into(),
                label: "Dark".into(),
            },
            overlay::ThemeItem {
                id: "light".into(),
                label: "Light".into(),
            },
            overlay::ThemeItem {
                id: "solarized".into(),
                label: "Solarized".into(),
            },
        ],
    ))
}

fn build_settings_overlay<S: SessionStore>(agent: &AgentRuntime<S>) -> overlay::Overlay {
    let config = agent.config();
    let items = vec![
        overlay::SettingItem {
            id: "stream".into(),
            label: "stream".into(),
            value: bool_label(config.stream),
        },
        overlay::SettingItem {
            id: "tools_enabled".into(),
            label: "tools_enabled".into(),
            value: bool_label(config.tools_enabled),
        },
        overlay::SettingItem {
            id: "print_mode".into(),
            label: "print_mode".into(),
            value: bool_label(config.print_mode),
        },
        overlay::SettingItem {
            id: "permission_mode".into(),
            label: "permission_mode".into(),
            value: format!("{:?}", config.permission_mode),
        },
        overlay::SettingItem {
            id: "compaction_threshold".into(),
            label: "compaction_threshold".into(),
            value: format!("{:.2}", config.compaction_threshold),
        },
    ];
    overlay::Overlay::Settings(overlay::ListOverlay::new(
        "设置 (Enter 切换 / 调整)",
        items,
    ))
}

fn bool_label(value: bool) -> String {
    if value {
        "on".into()
    } else {
        "off".into()
    }
}

fn build_thinking_overlay<S: SessionStore>(agent: &AgentRuntime<S>) -> overlay::Overlay {
    let current = agent.config().thinking_level;
    let mut items = vec![
        overlay::ThinkingItem {
            id: "none".into(),
            label: "无 (none)".into(),
            description: "不启用扩展思考".into(),
        },
        overlay::ThinkingItem {
            id: "low".into(),
            label: "低 (low)".into(),
            description: "少量推理预算".into(),
        },
        overlay::ThinkingItem {
            id: "medium".into(),
            label: "中 (medium)".into(),
            description: "默认推理预算".into(),
        },
        overlay::ThinkingItem {
            id: "high".into(),
            label: "高 (high)".into(),
            description: "最大推理预算".into(),
        },
    ];
    // Pre-select current.
    let mut list = overlay::ListOverlay::new("思考预算", items.drain(..).collect());
    list.selected = match current {
        pi_core::ThinkingLevel::None => 0,
        pi_core::ThinkingLevel::Low => 1,
        pi_core::ThinkingLevel::Medium => 2,
        pi_core::ThinkingLevel::High => 3,
    };
    overlay::Overlay::Thinking(list)
}

fn build_images_overlay(state: &AppState) -> overlay::Overlay {
    use pi_core::AttachmentData;
    let items: Vec<overlay::ImageItem> = state
        .pending_attachments
        .iter()
        .enumerate()
        .map(|(idx, att)| {
            let (label, bytes) = match &att.data {
                AttachmentData::Base64 { data } => (
                    format!("#{}  {} (base64)", idx + 1, att.mime_type),
                    data.len(),
                ),
                AttachmentData::Url { url } => {
                    (format!("#{}  {url}", idx + 1), url.len())
                }
            };
            overlay::ImageItem { label, bytes }
        })
        .collect();
    overlay::Overlay::ShowImages(overlay::ListOverlay::new(
        "待发送图片 (Enter 移除)",
        items,
    ))
}

fn build_login_overlay() -> overlay::Overlay {
    use pi_providers::ProviderRegistry;
    let registry = ProviderRegistry::builtin();
    let providers: Vec<overlay::LoginProvider> = registry
        .list()
        .filter_map(|info| {
            if info.id == "echo" {
                return None;
            }
            let supports_oauth = matches!(info.id.as_str(), "anthropic" | "openai");
            Some(overlay::LoginProvider {
                id: info.id.clone(),
                display_name: info.display_name.clone(),
                supports_oauth,
                api_key_env: info.requires_api_key_env.clone(),
            })
        })
        .collect();
    overlay::Overlay::Login(overlay::LoginOverlay::new(providers))
}

fn build_agent_overlay(cwd: &Option<std::path::PathBuf>) -> overlay::Overlay {
    let mut items: Vec<overlay::AgentItem> = Vec::new();
    items.push(overlay::AgentItem {
        id: "default".to_string(),
        label: "default (主代理)".to_string(),
        system: None,
    });
    if let Some(root) = cwd.as_ref().map(|c| c.join(".pi").join("agents")) {
        if let Ok(read) = std::fs::read_dir(&root) {
            for entry in read.flatten() {
                let path = entry.path();
                let name = path
                    .file_stem()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                let system = std::fs::read_to_string(&path).ok().map(|s| {
                    s.lines()
                        .filter(|l| !l.starts_with('#'))
                        .collect::<Vec<_>>()
                        .join("\n")
                });
                items.push(overlay::AgentItem {
                    id: name.clone(),
                    label: name,
                    system,
                });
            }
        }
    }
    overlay::Overlay::Agent(overlay::ListOverlay::new("切换子代理 / 主代理", items))
}

fn build_mcp_overlay(cwd: &Option<std::path::PathBuf>) -> overlay::Overlay {
    let mut items: Vec<overlay::McpItem> = Vec::new();
    if let Some(path) = cwd.as_ref().map(|c| c.join(".pi").join("mcp.toml")) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(parsed) = toml::from_str::<toml::Value>(&text) {
                if let Some(servers) = parsed.get("servers").and_then(|v| v.as_table()) {
                    for (id, value) in servers {
                        let label = value
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or(id.as_str())
                            .to_string();
                        items.push(overlay::McpItem {
                            id: id.clone(),
                            label,
                            running: true,
                        });
                    }
                }
            }
        }
    }
    if items.is_empty() {
        items.push(overlay::McpItem {
            id: "(none)".into(),
            label: "未发现 MCP 服务器 — 在 .pi/mcp.toml 中声明".into(),
            running: false,
        });
    }
    overlay::Overlay::Mcp(overlay::ListOverlay::new("MCP 服务器", items))
}

fn build_extension_overlay(cwd: &Option<std::path::PathBuf>) -> overlay::Overlay {
    let mut items: Vec<overlay::ExtensionItem> = Vec::new();
    if let Some(root) = cwd.as_ref().map(|c| c.join(".pi").join("extensions")) {
        if let Ok(read) = std::fs::read_dir(&root) {
            for entry in read.flatten() {
                let path = entry.path();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                items.push(overlay::ExtensionItem {
                    id: name.clone(),
                    label: name,
                    enabled: true,
                });
            }
        }
    }
    if items.is_empty() {
        items.push(overlay::ExtensionItem {
            id: "(none)".into(),
            label: "未发现扩展 — 把 wasm/process 模块放到 .pi/extensions/".into(),
            enabled: false,
        });
    }
    overlay::Overlay::Extension(overlay::ListOverlay::new("扩展", items))
}

fn apply_overlay_outcome<S: SessionStore>(
    state: &mut AppState,
    theme: &mut Theme,
    agent: &mut AgentRuntime<S>,
    outcome: overlay::OverlayOutcome,
) {
    match outcome {
        overlay::OverlayOutcome::None => {}
        overlay::OverlayOutcome::Close => {
            state.overlay = None;
        }
        overlay::OverlayOutcome::SetModel { provider, model } => {
            agent.config_mut().model.provider = provider.clone();
            agent.config_mut().model.model = model.clone();
            state.status = format!("已切换到 {provider} / {model}");
            state.overlay = None;
        }
        overlay::OverlayOutcome::SetTheme(id) => {
            *theme = Theme::from_base(&id);
            state.status = format!("主题切换到 {id}");
            state.overlay = None;
        }
        overlay::OverlayOutcome::ToggleSetting(id) => {
            let cfg = agent.config_mut();
            match id.as_str() {
                "stream" => cfg.stream = !cfg.stream,
                "tools_enabled" => cfg.tools_enabled = !cfg.tools_enabled,
                "print_mode" => cfg.print_mode = !cfg.print_mode,
                "permission_mode" => {
                    cfg.permission_mode = next_permission_mode(cfg.permission_mode);
                }
                "compaction_threshold" => {
                    cfg.compaction_threshold = match cfg.compaction_threshold {
                        x if x >= 0.95 => 0.50,
                        x => (x + 0.05).min(0.95),
                    };
                }
                _ => {}
            }
            // Refresh overlay items so labels update in place.
            state.overlay = Some(build_settings_overlay(agent));
            state.status = format!("已切换 {id}");
        }
        overlay::OverlayOutcome::SetThinking(id) => {
            if let Some(level) = pi_core::ThinkingLevel::from_str(&id) {
                agent.config_mut().thinking_level = level;
                state.status = format!("思考预算：{}", level.as_str());
            }
            state.overlay = None;
        }
        overlay::OverlayOutcome::AttachPath(path) => {
            // Insert `@<relpath>` at cursor.
            let rel = std::env::current_dir()
                .ok()
                .and_then(|cwd| path.strip_prefix(&cwd).ok().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| path.clone());
            let token = format!(" @{}", rel.display());
            state.input.insert_str(state.cursor, &token);
            state.cursor += token.len();
            state.status = format!("已加入 {}", path.display());
            state.overlay = None;
        }
        overlay::OverlayOutcome::RemoveImage(idx) => {
            if idx < state.pending_attachments.len() {
                state.pending_attachments.remove(idx);
                state.status = "已移除附件".into();
            }
            state.overlay = Some(build_images_overlay(state));
        }
        overlay::OverlayOutcome::ToggleExtension(id) => {
            state.status = format!("扩展切换：{id} (重启 pi 生效)");
            state.overlay = None;
        }
        overlay::OverlayOutcome::SwitchAgent(id) => {
            // Look up the selected agent's system prompt from the overlay
            // before we drop it.
            let system = if let Some(overlay::Overlay::Agent(list)) = &state.overlay {
                list.items.iter().find(|i| i.id == id).and_then(|i| i.system.clone())
            } else {
                None
            };
            agent.config_mut().system_prompt = system.clone();
            state.status = if system.is_some() {
                format!("已切换到子代理：{id}")
            } else {
                "已切换回主代理".to_string()
            };
            state.overlay = None;
        }
        overlay::OverlayOutcome::ToggleMcp(id) => {
            state.status = format!("MCP 服务器 {id} 状态变更将在下次会话生效");
            state.overlay = None;
        }
        overlay::OverlayOutcome::LoginSubmit(sub) => {
            let result = handle_login_submit(&sub);
            state.status = result;
            state.overlay = None;
        }
    }
}

fn next_permission_mode(mode: pi_core::PermissionModeKind) -> pi_core::PermissionModeKind {
    use pi_core::PermissionModeKind::*;
    match mode {
        ReadOnly => ConfirmMutations,
        ConfirmMutations => TrustedWorkspace,
        TrustedWorkspace => Plan,
        Plan => ReadOnly,
    }
}

fn handle_login_submit(sub: &overlay::LoginSubmission) -> String {
    use pi_auth::encrypted_file::EncryptedFileStore;
    use pi_auth::Resolver;
    if sub.use_oauth {
        return format!(
            "OAuth: 请在浏览器完成 {} 登录 (PKCE)；pi 会监听 127.0.0.1 回调端口。",
            sub.provider
        );
    }
    let env_key = match sub.provider.as_str() {
        "openai" | "openai-responses" => "OPENAI_API_KEY",
        "anthropic" => "ANTHROPIC_API_KEY",
        "gemini" => "GEMINI_API_KEY",
        "moonshot" => "MOONSHOT_API_KEY",
        "deepseek" => "DEEPSEEK_API_KEY",
        "qwen" => "DASHSCOPE_API_KEY",
        "zhipu" => "ZHIPU_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "mistral" => "MISTRAL_API_KEY",
        _ => "API_KEY",
    };
    let home = match std::env::var("HOME") {
        Ok(v) => v,
        Err(_) => return "找不到 $HOME，无法定位 ~/.pi-rust/auth.enc".to_string(),
    };
    let path = std::path::PathBuf::from(home)
        .join(".pi-rust")
        .join("auth.enc");
    match EncryptedFileStore::open(path) {
        Ok(mut store) => match store.store(&sub.provider, env_key, &sub.api_key) {
            Ok(_) => format!("{} 凭证已加密保存到 ~/.pi-rust/auth.enc", sub.provider),
            Err(err) => format!("保存失败：{err}"),
        },
        Err(err) => format!("加密存储不可用：{err}"),
    }
}

/// Convenience for callers that want to render a final transcript to stdout
/// without setting up the alternate screen.
pub fn print_final_transcript(messages: &[Message]) -> io::Result<()> {
    let mut out = io::stdout().lock();
    for message in messages {
        writeln!(out, "{}: {}", message.role.as_str(), message.content)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_caps_long_strings() {
        let long = "x".repeat(300);
        let short = truncate(&long, 50);
        assert!(short.chars().count() <= 51);
    }

    #[test]
    fn render_transcript_concatenates_roles() {
        let messages = vec![
            Message::new(Role::User, "hi"),
            Message::new(Role::Assistant, "ok"),
        ];
        let text = render_transcript(&messages);
        assert!(text.contains("user: hi"));
        assert!(text.contains("assistant: ok"));
    }
}
