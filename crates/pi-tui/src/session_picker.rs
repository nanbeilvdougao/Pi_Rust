//! Interactive session picker.
//!
//! Shared between `pi --resume` (no ID) and `pi sessions` (no other args).
//! Renders a ratatui list of sessions sorted newest-first, lets the user
//! pick one with arrow keys + Enter, and returns the chosen id. Esc / Ctrl+C
//! returns `None` so the caller can fall back to default behavior or exit
//! gracefully.
//!
//! We deliberately keep this a free function rather than a long-running TUI
//! mode so the same code path serves both call sites and is also
//! straightforward to skip (e.g. when stdout is not a TTY).

use std::io;
use std::time::Duration;

use crossterm::{
    event::{self, Event as CrosstermEvent, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use pi_session::{JsonlSessionStore, SessionSummary};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};

/// Return value from `pick`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickResult {
    Selected(String),
    NewSession,
    Cancelled,
}

/// Render an interactive list of sessions and return the user's choice.
/// Errors propagate from terminal setup; an empty session list returns
/// `PickResult::NewSession` immediately without entering the alt-screen.
pub fn pick(store: &JsonlSessionStore) -> io::Result<PickResult> {
    let sessions = store
        .list()
        .map_err(|err| io::Error::other(err.to_string()))?;
    if sessions.is_empty() {
        return Ok(PickResult::NewSession);
    }
    if !is_tty() {
        // Non-TTY stdout: print sessions as text and ask via stdin? We keep
        // it simple — return cancelled, caller falls back to `--list-sessions`.
        return Ok(PickResult::Cancelled);
    }

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let outcome = render_loop(&mut terminal, &sessions);
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    outcome
}

fn is_tty() -> bool {
    use crossterm::tty::IsTty;
    io::stdout().is_tty()
}

fn render_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    sessions: &[SessionSummary],
) -> io::Result<PickResult> {
    let mut selected: usize = 0;
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(5), Constraint::Length(3)])
                .split(area);

            let items: Vec<ListItem<'static>> = sessions
                .iter()
                .map(|summary| {
                    let excerpt = summary
                        .last_user_excerpt
                        .clone()
                        .unwrap_or_else(|| "(空会话)".to_string());
                    ListItem::new(format!(
                        "{}  ·  {} 条消息  ·  {}",
                        summary.id, summary.message_count, excerpt
                    ))
                })
                .collect();
            let mut state = ListState::default();
            state.select(Some(selected));
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" 选择会话 (↑/↓ 移动, Enter 选中, n 新建, Esc 取消) "),
                )
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
            frame.render_stateful_widget(list, chunks[0], &mut state);

            let hint = Paragraph::new(Line::from(vec![
                Span::raw("提示："),
                Span::raw("Enter 进入选中会话，n 新建会话，Esc 或 Ctrl+C 取消。"),
            ]))
            .block(Block::default().borders(Borders::ALL).title(" 操作 "));
            frame.render_widget(hint, chunks[1]);
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let CrosstermEvent::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Down if selected + 1 < sessions.len() => selected += 1,
                    KeyCode::Home => selected = 0,
                    KeyCode::End => selected = sessions.len().saturating_sub(1),
                    KeyCode::Enter => {
                        return Ok(PickResult::Selected(sessions[selected].id.clone()));
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') => {
                        return Ok(PickResult::NewSession);
                    }
                    KeyCode::Esc => return Ok(PickResult::Cancelled),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(PickResult::Cancelled);
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{Message, Role};
    use pi_session::SessionStore;
    use tempfile::tempdir;

    #[test]
    fn empty_store_returns_new_session_without_tty() {
        let dir = tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path());
        // No sessions written → fast path returns NewSession without touching
        // the terminal, so this test works under cargo test (non-TTY).
        let result = pick(&store).unwrap();
        assert_eq!(result, PickResult::NewSession);
    }

    #[test]
    fn non_tty_with_sessions_returns_cancelled() {
        let dir = tempdir().unwrap();
        let store = JsonlSessionStore::new(dir.path());
        store
            .append("alpha", &Message::new(Role::User, "你好"))
            .unwrap();
        let result = pick(&store).unwrap();
        // cargo test runs with stdout piped, so is_tty() → false; the picker
        // takes the cancel path.
        assert_eq!(result, PickResult::Cancelled);
    }
}
