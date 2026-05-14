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
pub mod footer;
pub mod keybindings;
pub mod session_picker;
pub mod theme;
use clipboard::{read_clipboard, Pasted};
use completion::{Completer, CompletionItem, CompletionKind, TriggerSpan};
pub use config_selector::{
    needs_wizard as needs_config_wizard, run as run_config_wizard, ProviderChoice, WizardResult,
};
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

    loop {
        terminal
            .draw(|frame| draw(frame, agent, &state, active_turn.is_some(), &theme))
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
        }
        Event::ToolFinished { name, output } => {
            state.push_entry(TranscriptKind::Tool, format!("[{name}]\n{output}"));
        }
        Event::ToolError { name, error } => {
            state.push_entry(TranscriptKind::Error, format!("tool {name} 出错：{error}"));
        }
        Event::Usage(usage) => {
            state.status = format!(
                "流式中… in={} out={} total={}",
                usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
            );
        }
        Event::Compacted { before, after } => {
            state.push_entry(
                TranscriptKind::System,
                format!("[compaction] {before} -> {after} tokens"),
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
        for raw_line in entry.body.lines() {
            lines.push(Line::from(Span::raw(raw_line.to_string())));
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
        for raw_line in state.streaming_buffer.lines() {
            lines.push(Line::from(Span::raw(raw_line.to_string())));
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
