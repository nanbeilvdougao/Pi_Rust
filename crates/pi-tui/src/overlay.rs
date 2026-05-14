//! Modal overlays for the interactive TUI.
//!
//! All overlays share a common shape: render a centered modal over the main
//! transcript view, accept ↑/↓/Enter/Esc for list-style pickers, plus a few
//! richer per-overlay flows (login dialog text input, tree view expansion).
//!
//! Variants:
//! - `ModelSelector` — pick provider+model from the built-in registry. Ctrl+M.
//!   Parity target: `packages/tui/src/components/model-selector.ts`.
//! - `ThemeSelector` — switch between dark/light/solarized at runtime. Ctrl+T.
//!   Parity target: `packages/tui/src/components/theme-selector.ts`.
//! - `Login` — OAuth+api-key entry, with PKCE for Anthropic/OpenAI. Ctrl+G.
//!   Parity target: `packages/tui/src/components/login-dialog.ts`.
//! - `Settings` — toggle agent-side knobs (auto-compact, allow-bash, …). F3.
//!   Parity target: `packages/tui/src/components/settings-selector.ts`.
//! - `Thinking` — pick thinking budget (none/low/medium/high). F2.
//!   Parity target: `packages/tui/src/components/thinking-selector.ts`.
//! - `Tree` — workspace file tree, navigate + attach as `@path`. Ctrl+P.
//!   Parity target: `packages/tui/src/components/tree-selector.ts`.
//! - `ShowImages` — list pending image attachments + remove. Alt+I.
//!   Parity target: `packages/tui/src/components/show-images-selector.ts`.
//! - `Extension` — toggle/launch installed extensions. Ctrl+E.
//!   Parity target: `packages/tui/src/components/extension-selector.ts`.
//!
//! Result type: each overlay returns an `OverlayOutcome` so the main loop can
//! apply the user's choice (e.g. swap models, write to pi-auth, push @path
//! into the input buffer).

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::theme::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Model(ListOverlay<ModelItem>),
    Theme(ListOverlay<ThemeItem>),
    Login(LoginOverlay),
    Settings(ListOverlay<SettingItem>),
    Thinking(ListOverlay<ThinkingItem>),
    Tree(TreeOverlay),
    ShowImages(ListOverlay<ImageItem>),
    Extension(ListOverlay<ExtensionItem>),
    Agent(ListOverlay<AgentItem>),
    Mcp(ListOverlay<McpItem>),
}

/// What the overlay produced when the user pressed Enter (or otherwise
/// finalized). The main loop applies side effects (e.g. `agent.set_model`)
/// then drops the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOutcome {
    None,
    Close,
    SetModel { provider: String, model: String },
    SetTheme(String),
    LoginSubmit(LoginSubmission),
    ToggleSetting(String),
    SetThinking(String),
    AttachPath(PathBuf),
    RemoveImage(usize),
    ToggleExtension(String),
    SwitchAgent(String),
    ToggleMcp(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginSubmission {
    pub provider: String,
    pub api_key: String,
    pub use_oauth: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListOverlay<T> {
    pub title: String,
    pub items: Vec<T>,
    pub selected: usize,
}

impl<T: Labelled> ListOverlay<T> {
    pub fn new(title: impl Into<String>, items: Vec<T>) -> Self {
        Self {
            title: title.into(),
            items,
            selected: 0,
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
    }

    pub fn current(&self) -> Option<&T> {
        self.items.get(self.selected)
    }
}

pub trait Labelled {
    fn label(&self) -> String;
    fn hint(&self) -> Option<String> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelItem {
    pub provider: String,
    pub model: String,
    pub display_name: String,
}

impl Labelled for ModelItem {
    fn label(&self) -> String {
        format!("{}  ·  {}", self.provider, self.model)
    }
    fn hint(&self) -> Option<String> {
        Some(self.display_name.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeItem {
    pub id: String,
    pub label: String,
}

impl Labelled for ThemeItem {
    fn label(&self) -> String {
        self.label.clone()
    }
    fn hint(&self) -> Option<String> {
        Some(self.id.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingItem {
    pub id: String,
    pub label: String,
    pub value: String,
}

impl Labelled for SettingItem {
    fn label(&self) -> String {
        format!("{}: {}", self.label, self.value)
    }
    fn hint(&self) -> Option<String> {
        Some(self.id.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThinkingItem {
    pub id: String,
    pub label: String,
    pub description: String,
}

impl Labelled for ThinkingItem {
    fn label(&self) -> String {
        self.label.clone()
    }
    fn hint(&self) -> Option<String> {
        Some(self.description.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageItem {
    pub label: String,
    pub bytes: usize,
}

impl Labelled for ImageItem {
    fn label(&self) -> String {
        self.label.clone()
    }
    fn hint(&self) -> Option<String> {
        Some(format!("{} 字节", self.bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionItem {
    pub id: String,
    pub label: String,
    pub enabled: bool,
}

impl Labelled for ExtensionItem {
    fn label(&self) -> String {
        let mark = if self.enabled { "[x]" } else { "[ ]" };
        format!("{} {}", mark, self.label)
    }
    fn hint(&self) -> Option<String> {
        Some(self.id.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentItem {
    pub id: String,
    pub label: String,
    pub system: Option<String>,
}

impl Labelled for AgentItem {
    fn label(&self) -> String {
        self.label.clone()
    }
    fn hint(&self) -> Option<String> {
        Some(self.id.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpItem {
    pub id: String,
    pub label: String,
    pub running: bool,
}

impl Labelled for McpItem {
    fn label(&self) -> String {
        let mark = if self.running { "●" } else { "○" };
        format!("{mark} {}", self.label)
    }
    fn hint(&self) -> Option<String> {
        Some(self.id.clone())
    }
}

// -----------------------------------------------------------------------------
// Login overlay (richer than a list - has provider list + text input)
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginOverlay {
    pub providers: Vec<LoginProvider>,
    pub selected: usize,
    pub input: String,
    pub stage: LoginStage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginProvider {
    pub id: String,
    pub display_name: String,
    pub supports_oauth: bool,
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginStage {
    PickProvider,
    PickMethod,
    EnterApiKey,
}

impl LoginOverlay {
    pub fn new(providers: Vec<LoginProvider>) -> Self {
        Self {
            providers,
            selected: 0,
            input: String::new(),
            stage: LoginStage::PickProvider,
        }
    }

    pub fn current(&self) -> Option<&LoginProvider> {
        self.providers.get(self.selected)
    }

    pub fn move_up(&mut self) {
        if self.stage == LoginStage::PickProvider && self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.stage == LoginStage::PickProvider && self.selected + 1 < self.providers.len() {
            self.selected += 1;
        }
    }
}

// -----------------------------------------------------------------------------
// Tree overlay (workspace file tree)
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeOverlay {
    pub root: PathBuf,
    pub entries: Vec<TreeEntry>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: PathBuf,
    pub depth: usize,
    pub is_dir: bool,
}

impl TreeOverlay {
    pub fn new(root: PathBuf) -> Self {
        let entries = scan_tree(&root, 0, 3);
        Self {
            root,
            entries,
            selected: 0,
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
        }
    }

    pub fn current(&self) -> Option<&TreeEntry> {
        self.entries.get(self.selected)
    }
}

fn scan_tree(root: &std::path::Path, depth: usize, max_depth: usize) -> Vec<TreeEntry> {
    let mut out = Vec::new();
    walk(root, depth, max_depth, &mut out);
    out
}

fn walk(dir: &std::path::Path, depth: usize, max_depth: usize, out: &mut Vec<TreeEntry>) {
    if depth > max_depth {
        return;
    }
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut entries: Vec<_> = read.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') || name_str == "target" || name_str == "node_modules" {
            continue;
        }
        let path = entry.path();
        let is_dir = path.is_dir();
        out.push(TreeEntry {
            path: path.clone(),
            depth,
            is_dir,
        });
        if is_dir && depth < max_depth {
            walk(&path, depth + 1, max_depth, out);
        }
    }
}

// -----------------------------------------------------------------------------
// Key handling - dispatch based on the active overlay variant
// -----------------------------------------------------------------------------

impl Overlay {
    pub fn handle_key(&mut self, key: &KeyEvent) -> OverlayOutcome {
        if matches!(key.code, KeyCode::Esc) {
            return OverlayOutcome::Close;
        }
        if matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL) {
            return OverlayOutcome::Close;
        }
        match self {
            Overlay::Model(list) => handle_list(list, key, |item| OverlayOutcome::SetModel {
                provider: item.provider.clone(),
                model: item.model.clone(),
            }),
            Overlay::Theme(list) => {
                handle_list(list, key, |item| OverlayOutcome::SetTheme(item.id.clone()))
            }
            Overlay::Settings(list) => handle_list(list, key, |item| {
                OverlayOutcome::ToggleSetting(item.id.clone())
            }),
            Overlay::Thinking(list) => {
                handle_list(list, key, |item| OverlayOutcome::SetThinking(item.id.clone()))
            }
            Overlay::ShowImages(list) => {
                let idx = list.selected;
                handle_list(list, key, move |_| OverlayOutcome::RemoveImage(idx))
            }
            Overlay::Extension(list) => handle_list(list, key, |item| {
                OverlayOutcome::ToggleExtension(item.id.clone())
            }),
            Overlay::Agent(list) => {
                handle_list(list, key, |item| OverlayOutcome::SwitchAgent(item.id.clone()))
            }
            Overlay::Mcp(list) => {
                handle_list(list, key, |item| OverlayOutcome::ToggleMcp(item.id.clone()))
            }
            Overlay::Login(login) => handle_login(login, key),
            Overlay::Tree(tree) => handle_tree(tree, key),
        }
    }
}

fn handle_list<T, F>(list: &mut ListOverlay<T>, key: &KeyEvent, on_select: F) -> OverlayOutcome
where
    T: Labelled,
    F: FnOnce(&T) -> OverlayOutcome,
{
    match key.code {
        KeyCode::Up => {
            list.move_up();
            OverlayOutcome::None
        }
        KeyCode::Down => {
            list.move_down();
            OverlayOutcome::None
        }
        KeyCode::Home => {
            list.selected = 0;
            OverlayOutcome::None
        }
        KeyCode::End => {
            list.selected = list.items.len().saturating_sub(1);
            OverlayOutcome::None
        }
        KeyCode::Enter => match list.current() {
            Some(item) => on_select(item),
            None => OverlayOutcome::Close,
        },
        _ => OverlayOutcome::None,
    }
}

fn handle_login(login: &mut LoginOverlay, key: &KeyEvent) -> OverlayOutcome {
    match login.stage {
        LoginStage::PickProvider => match key.code {
            KeyCode::Up => {
                login.move_up();
                OverlayOutcome::None
            }
            KeyCode::Down => {
                login.move_down();
                OverlayOutcome::None
            }
            KeyCode::Enter => {
                if login.current().is_some() {
                    let supports_oauth =
                        login.current().map(|p| p.supports_oauth).unwrap_or(false);
                    login.stage = if supports_oauth {
                        LoginStage::PickMethod
                    } else {
                        LoginStage::EnterApiKey
                    };
                }
                OverlayOutcome::None
            }
            _ => OverlayOutcome::None,
        },
        LoginStage::PickMethod => match key.code {
            KeyCode::Char('o') | KeyCode::Char('O') => {
                if let Some(provider) = login.current().cloned() {
                    return OverlayOutcome::LoginSubmit(LoginSubmission {
                        provider: provider.id,
                        api_key: String::new(),
                        use_oauth: true,
                    });
                }
                OverlayOutcome::None
            }
            KeyCode::Char('k') | KeyCode::Char('K') => {
                login.stage = LoginStage::EnterApiKey;
                OverlayOutcome::None
            }
            KeyCode::Backspace | KeyCode::Left => {
                login.stage = LoginStage::PickProvider;
                OverlayOutcome::None
            }
            _ => OverlayOutcome::None,
        },
        LoginStage::EnterApiKey => match key.code {
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                login.input.push(c);
                OverlayOutcome::None
            }
            KeyCode::Backspace => {
                login.input.pop();
                OverlayOutcome::None
            }
            KeyCode::Enter => {
                if !login.input.is_empty() {
                    if let Some(provider) = login.current().cloned() {
                        return OverlayOutcome::LoginSubmit(LoginSubmission {
                            provider: provider.id,
                            api_key: std::mem::take(&mut login.input),
                            use_oauth: false,
                        });
                    }
                }
                OverlayOutcome::None
            }
            _ => OverlayOutcome::None,
        },
    }
}

fn handle_tree(tree: &mut TreeOverlay, key: &KeyEvent) -> OverlayOutcome {
    match key.code {
        KeyCode::Up => {
            tree.move_up();
            OverlayOutcome::None
        }
        KeyCode::Down => {
            tree.move_down();
            OverlayOutcome::None
        }
        KeyCode::Enter => match tree.current() {
            Some(entry) if !entry.is_dir => OverlayOutcome::AttachPath(entry.path.clone()),
            _ => OverlayOutcome::None,
        },
        _ => OverlayOutcome::None,
    }
}

// -----------------------------------------------------------------------------
// Rendering
// -----------------------------------------------------------------------------

pub fn draw(frame: &mut Frame<'_>, area: Rect, overlay: &Overlay, theme: &Theme) {
    let popup = center_rect(area, 70, 70);
    frame.render_widget(Clear, popup);
    match overlay {
        Overlay::Model(list) => draw_list(frame, popup, list, theme),
        Overlay::Theme(list) => draw_list(frame, popup, list, theme),
        Overlay::Settings(list) => draw_list(frame, popup, list, theme),
        Overlay::Thinking(list) => draw_list(frame, popup, list, theme),
        Overlay::ShowImages(list) => draw_list(frame, popup, list, theme),
        Overlay::Extension(list) => draw_list(frame, popup, list, theme),
        Overlay::Agent(list) => draw_list(frame, popup, list, theme),
        Overlay::Mcp(list) => draw_list(frame, popup, list, theme),
        Overlay::Login(login) => draw_login(frame, popup, login, theme),
        Overlay::Tree(tree) => draw_tree(frame, popup, tree, theme),
    }
}

fn draw_list<T: Labelled>(frame: &mut Frame<'_>, area: Rect, list: &ListOverlay<T>, theme: &Theme) {
    let items: Vec<ListItem<'static>> = list
        .items
        .iter()
        .map(|item| {
            let mut spans = vec![Span::raw(item.label())];
            if let Some(hint) = item.hint() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(hint, Style::default().fg(theme.hint)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(list.selected));
    let widget = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} (↑/↓ 选择，Enter 确认，Esc 取消) ", list.title))
                .border_style(Style::default().fg(theme.border))
                .title_style(
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
        )
        .highlight_style(
            Style::default()
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(widget, area, &mut state);
}

fn draw_login(frame: &mut Frame<'_>, area: Rect, login: &LoginOverlay, theme: &Theme) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(4)])
        .split(area);
    let body = match login.stage {
        LoginStage::PickProvider => {
            let items: Vec<ListItem<'static>> = login
                .providers
                .iter()
                .map(|p| {
                    let auth = if p.supports_oauth {
                        "OAuth+API"
                    } else {
                        "API key"
                    };
                    ListItem::new(format!("{:<12} {}  ·  {}", p.id, p.display_name, auth))
                })
                .collect();
            let mut state = ListState::default();
            state.select(Some(login.selected));
            let widget = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" 登录 — 选择 provider ")
                        .border_style(Style::default().fg(theme.border)),
                )
                .highlight_style(
                    Style::default()
                        .bg(theme.selection)
                        .add_modifier(Modifier::BOLD),
                );
            frame.render_stateful_widget(widget, chunks[0], &mut state);
            None
        }
        LoginStage::PickMethod => {
            let provider = login
                .current()
                .map(|p| p.display_name.clone())
                .unwrap_or_default();
            Some(Paragraph::new(vec![
                Line::from(Span::styled(
                    format!("登录 {} —— 选择方式", provider),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from("[O] OAuth (浏览器 PKCE 流程)"),
                Line::from("[K] API key (粘贴或输入)"),
                Line::from(""),
                Line::from(Span::styled(
                    "← 返回上一步，Esc 关闭",
                    Style::default().fg(theme.hint),
                )),
            ]))
        }
        LoginStage::EnterApiKey => {
            let provider = login
                .current()
                .map(|p| p.display_name.clone())
                .unwrap_or_default();
            let mask = "•".repeat(login.input.chars().count());
            Some(Paragraph::new(vec![
                Line::from(Span::styled(
                    format!("为 {provider} 输入 API key"),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(format!("> {mask}")),
                Line::from(""),
                Line::from(Span::styled(
                    "Enter 提交，Backspace 删除，Esc 取消",
                    Style::default().fg(theme.hint),
                )),
            ]))
        }
    };
    if let Some(p) = body {
        frame.render_widget(
            p.block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" 登录 ")
                    .border_style(Style::default().fg(theme.border)),
            )
            .wrap(Wrap { trim: false }),
            chunks[0],
        );
    }
    let hint = Paragraph::new(Line::from(vec![
        Span::styled("Enter ", Style::default().fg(theme.accent)),
        Span::raw("下一步  "),
        Span::styled("Esc ", Style::default().fg(theme.accent)),
        Span::raw("取消"),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(hint, chunks[1]);
}

fn draw_tree(frame: &mut Frame<'_>, area: Rect, tree: &TreeOverlay, theme: &Theme) {
    let items: Vec<ListItem<'static>> = tree
        .entries
        .iter()
        .map(|entry| {
            let indent = "  ".repeat(entry.depth);
            let name = entry
                .path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let marker = if entry.is_dir { "▸" } else { " " };
            ListItem::new(format!("{indent}{marker} {name}"))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(tree.selected));
    let widget = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(
                    " 工作区文件树 — {} (Enter 附加文件，Esc 取消) ",
                    tree.root.display()
                ))
                .border_style(Style::default().fg(theme.border))
                .title_style(
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
        )
        .highlight_style(
            Style::default()
                .bg(theme.selection)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_stateful_widget(widget, area, &mut state);
}

fn center_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    let horiz = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vert[1]);
    horiz[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn model_overlay_select_yields_set_model() {
        let mut overlay = Overlay::Model(ListOverlay::new(
            "模型",
            vec![
                ModelItem {
                    provider: "openai".into(),
                    model: "gpt-4o".into(),
                    display_name: "GPT-4o".into(),
                },
                ModelItem {
                    provider: "anthropic".into(),
                    model: "claude-sonnet-4-6".into(),
                    display_name: "Claude Sonnet 4.6".into(),
                },
            ],
        ));
        // Move down to second, hit enter.
        overlay.handle_key(&key(KeyCode::Down));
        let outcome = overlay.handle_key(&key(KeyCode::Enter));
        assert_eq!(
            outcome,
            OverlayOutcome::SetModel {
                provider: "anthropic".into(),
                model: "claude-sonnet-4-6".into(),
            }
        );
    }

    #[test]
    fn theme_overlay_returns_chosen_id() {
        let mut overlay = Overlay::Theme(ListOverlay::new(
            "主题",
            vec![
                ThemeItem {
                    id: "dark".into(),
                    label: "Dark".into(),
                },
                ThemeItem {
                    id: "light".into(),
                    label: "Light".into(),
                },
            ],
        ));
        overlay.handle_key(&key(KeyCode::Down));
        let outcome = overlay.handle_key(&key(KeyCode::Enter));
        assert_eq!(outcome, OverlayOutcome::SetTheme("light".into()));
    }

    #[test]
    fn esc_closes_any_overlay() {
        let mut overlay = Overlay::Theme(ListOverlay::new(
            "主题",
            vec![ThemeItem {
                id: "dark".into(),
                label: "Dark".into(),
            }],
        ));
        assert_eq!(overlay.handle_key(&key(KeyCode::Esc)), OverlayOutcome::Close);
    }

    #[test]
    fn settings_overlay_toggle() {
        let mut overlay = Overlay::Settings(ListOverlay::new(
            "设置",
            vec![SettingItem {
                id: "auto_compact".into(),
                label: "Auto Compact".into(),
                value: "on".into(),
            }],
        ));
        let outcome = overlay.handle_key(&key(KeyCode::Enter));
        assert_eq!(
            outcome,
            OverlayOutcome::ToggleSetting("auto_compact".into())
        );
    }

    #[test]
    fn thinking_overlay_select() {
        let mut overlay = Overlay::Thinking(ListOverlay::new(
            "思考",
            vec![
                ThinkingItem {
                    id: "none".into(),
                    label: "无".into(),
                    description: "disabled".into(),
                },
                ThinkingItem {
                    id: "high".into(),
                    label: "高".into(),
                    description: "max thinking".into(),
                },
            ],
        ));
        overlay.handle_key(&key(KeyCode::Down));
        let outcome = overlay.handle_key(&key(KeyCode::Enter));
        assert_eq!(outcome, OverlayOutcome::SetThinking("high".into()));
    }

    #[test]
    fn extension_overlay_toggle() {
        let mut overlay = Overlay::Extension(ListOverlay::new(
            "扩展",
            vec![ExtensionItem {
                id: "my-ext".into(),
                label: "my extension".into(),
                enabled: false,
            }],
        ));
        let outcome = overlay.handle_key(&key(KeyCode::Enter));
        assert_eq!(
            outcome,
            OverlayOutcome::ToggleExtension("my-ext".into())
        );
    }

    #[test]
    fn show_images_overlay_remove_by_index() {
        let mut overlay = Overlay::ShowImages(ListOverlay::new(
            "图片",
            vec![
                ImageItem {
                    label: "a.png".into(),
                    bytes: 1024,
                },
                ImageItem {
                    label: "b.png".into(),
                    bytes: 2048,
                },
            ],
        ));
        overlay.handle_key(&key(KeyCode::Down));
        let outcome = overlay.handle_key(&key(KeyCode::Enter));
        assert_eq!(outcome, OverlayOutcome::RemoveImage(1));
    }

    #[test]
    fn login_provider_advances_to_method_when_oauth() {
        let mut overlay = Overlay::Login(LoginOverlay::new(vec![LoginProvider {
            id: "anthropic".into(),
            display_name: "Anthropic".into(),
            supports_oauth: true,
            api_key_env: Some("ANTHROPIC_API_KEY".into()),
        }]));
        overlay.handle_key(&key(KeyCode::Enter));
        if let Overlay::Login(state) = &overlay {
            assert_eq!(state.stage, LoginStage::PickMethod);
        } else {
            panic!("overlay must remain Login");
        }
    }

    #[test]
    fn login_api_key_submission() {
        let mut overlay = Overlay::Login(LoginOverlay::new(vec![LoginProvider {
            id: "openai".into(),
            display_name: "OpenAI".into(),
            supports_oauth: false,
            api_key_env: Some("OPENAI_API_KEY".into()),
        }]));
        // No oauth → straight to EnterApiKey.
        overlay.handle_key(&key(KeyCode::Enter));
        // Type some chars.
        for c in "sk-test".chars() {
            overlay.handle_key(&key(KeyCode::Char(c)));
        }
        let outcome = overlay.handle_key(&key(KeyCode::Enter));
        match outcome {
            OverlayOutcome::LoginSubmit(sub) => {
                assert_eq!(sub.provider, "openai");
                assert_eq!(sub.api_key, "sk-test");
                assert!(!sub.use_oauth);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn agent_overlay_returns_switch_agent() {
        let mut overlay = Overlay::Agent(ListOverlay::new(
            "agent",
            vec![
                AgentItem {
                    id: "default".into(),
                    label: "default".into(),
                    system: None,
                },
                AgentItem {
                    id: "researcher".into(),
                    label: "researcher".into(),
                    system: Some("research mode".into()),
                },
            ],
        ));
        overlay.handle_key(&key(KeyCode::Down));
        let outcome = overlay.handle_key(&key(KeyCode::Enter));
        assert_eq!(outcome, OverlayOutcome::SwitchAgent("researcher".into()));
    }

    #[test]
    fn mcp_overlay_returns_toggle_mcp() {
        let mut overlay = Overlay::Mcp(ListOverlay::new(
            "MCP",
            vec![McpItem {
                id: "filesystem".into(),
                label: "filesystem".into(),
                running: false,
            }],
        ));
        let outcome = overlay.handle_key(&key(KeyCode::Enter));
        assert_eq!(outcome, OverlayOutcome::ToggleMcp("filesystem".into()));
    }

    #[test]
    fn tree_attach_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.txt"), "hi").unwrap();
        let mut overlay = Overlay::Tree(TreeOverlay::new(dir.path().to_path_buf()));
        // First entry is hello.txt
        let outcome = overlay.handle_key(&key(KeyCode::Enter));
        match outcome {
            OverlayOutcome::AttachPath(p) => assert!(p.ends_with("hello.txt")),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
}
