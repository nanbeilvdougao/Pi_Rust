//! Configurable keybindings for the interactive TUI.
//!
//! Layered, same shape as `pi-agent::settings`:
//! 1. Built-in defaults (this module).
//! 2. `~/.pi-rust/keybindings.toml` (user-global override).
//! 3. `<workspace>/.pi/keybindings.toml` (project override).
//!
//! Each action is a stable string id (`"submit"`, `"quit"`, `"newline"`,
//! `"clear"`, `"paste"`, `"complete"`, `"cancel-complete"`, `"history-prev"`,
//! `"history-next"`, `"cursor-left"`, `"cursor-right"`, `"backspace"`). A
//! single action can bind to multiple key chords; on lookup we walk the
//! ordered list and return the first match. This lets users keep the default
//! Ctrl+J for newline AND add Alt+Enter on top.
//!
//! The TOML config:
//!
//! ```toml
//! [bindings]
//! submit       = ["Enter"]
//! quit         = ["Ctrl+C"]
//! newline      = ["Ctrl+J", "Alt+Enter"]
//! clear        = ["Ctrl+L"]
//! paste        = ["Ctrl+V"]
//! complete     = ["Tab"]
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};

pub const ACTIONS: &[&str] = &[
    "submit",
    "quit",
    "newline",
    "clear",
    "paste",
    "complete",
    "cancel-complete",
    "history-prev",
    "history-next",
    "cursor-left",
    "cursor-right",
    "backspace",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct KeyBindingsFile {
    #[serde(default)]
    pub bindings: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBindings {
    map: BTreeMap<String, Vec<KeyChord>>,
}

impl Default for KeyBindings {
    fn default() -> Self {
        let mut map: BTreeMap<String, Vec<KeyChord>> = BTreeMap::new();
        map.insert("submit".into(), vec![KeyChord::plain(KeyCode::Enter)]);
        map.insert(
            "quit".into(),
            vec![KeyChord::with_mods(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            )],
        );
        map.insert(
            "newline".into(),
            vec![KeyChord::with_mods(
                KeyCode::Char('j'),
                KeyModifiers::CONTROL,
            )],
        );
        map.insert(
            "clear".into(),
            vec![KeyChord::with_mods(
                KeyCode::Char('l'),
                KeyModifiers::CONTROL,
            )],
        );
        map.insert(
            "paste".into(),
            vec![KeyChord::with_mods(
                KeyCode::Char('v'),
                KeyModifiers::CONTROL,
            )],
        );
        map.insert("complete".into(), vec![KeyChord::plain(KeyCode::Tab)]);
        map.insert(
            "cancel-complete".into(),
            vec![KeyChord::plain(KeyCode::Esc)],
        );
        map.insert("history-prev".into(), vec![KeyChord::plain(KeyCode::Up)]);
        map.insert("history-next".into(), vec![KeyChord::plain(KeyCode::Down)]);
        map.insert("cursor-left".into(), vec![KeyChord::plain(KeyCode::Left)]);
        map.insert("cursor-right".into(), vec![KeyChord::plain(KeyCode::Right)]);
        map.insert(
            "backspace".into(),
            vec![KeyChord::plain(KeyCode::Backspace)],
        );
        Self { map }
    }
}

impl KeyBindings {
    pub fn load_layered(workspace_root: Option<&Path>) -> Self {
        let mut bindings = Self::default();
        if let Ok(home) = std::env::var("HOME") {
            let user = Path::new(&home).join(".pi-rust").join("keybindings.toml");
            if let Some(layer) = read_file(&user) {
                bindings.apply(&layer);
            }
        }
        if let Some(root) = workspace_root {
            let workspace = root.join(".pi").join("keybindings.toml");
            if let Some(layer) = read_file(&workspace) {
                bindings.apply(&layer);
            }
        }
        bindings
    }

    pub fn apply(&mut self, file: &KeyBindingsFile) {
        for (action, chords) in &file.bindings {
            let parsed: Vec<KeyChord> = chords.iter().filter_map(|s| KeyChord::parse(s)).collect();
            if !parsed.is_empty() {
                self.map.insert(action.clone(), parsed);
            }
        }
    }

    pub fn matches(&self, action: &str, event: &KeyEvent) -> bool {
        self.map
            .get(action)
            .map(|chords| chords.iter().any(|chord| chord.matches(event)))
            .unwrap_or(false)
    }

    /// Reverse lookup: which action does this event trigger, if any. Used by
    /// the footer hint to render the current binding.
    pub fn action_for(&self, event: &KeyEvent) -> Option<&str> {
        for (action, chords) in &self.map {
            if chords.iter().any(|chord| chord.matches(event)) {
                return Some(action.as_str());
            }
        }
        None
    }

    pub fn chords_for(&self, action: &str) -> Vec<String> {
        self.map
            .get(action)
            .map(|cs| cs.iter().map(|c| c.display()).collect())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyChord {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyChord {
    pub fn plain(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    pub fn with_mods(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    pub fn matches(&self, event: &KeyEvent) -> bool {
        if event.code != self.code {
            // Char keys are case-insensitive when CONTROL/ALT pressed.
            match (event.code, self.code) {
                (KeyCode::Char(a), KeyCode::Char(b))
                    if a.eq_ignore_ascii_case(&b) && !self.modifiers.is_empty() => {}
                _ => return false,
            }
        }
        // Allow extra SHIFT modifier on plain char keys so neither shape
        // accidentally drops shifted input.
        let mut event_mods = event.modifiers;
        if matches!(self.code, KeyCode::Char(_)) && !self.modifiers.contains(KeyModifiers::SHIFT) {
            event_mods.remove(KeyModifiers::SHIFT);
        }
        event_mods == self.modifiers
    }

    pub fn parse(input: &str) -> Option<Self> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return None;
        }
        let mut modifiers = KeyModifiers::NONE;
        let mut parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();
        let code_str = parts.pop()?;
        for part in parts {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                "alt" | "option" | "meta" => modifiers |= KeyModifiers::ALT,
                "shift" => modifiers |= KeyModifiers::SHIFT,
                "super" | "cmd" | "command" => modifiers |= KeyModifiers::SUPER,
                _ => return None,
            }
        }
        let code = parse_code(code_str)?;
        Some(Self { code, modifiers })
    }

    pub fn display(&self) -> String {
        let mut out = String::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            out.push_str("Ctrl+");
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            out.push_str("Alt+");
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            out.push_str("Shift+");
        }
        if self.modifiers.contains(KeyModifiers::SUPER) {
            out.push_str("Super+");
        }
        out.push_str(&display_code(&self.code));
        out
    }
}

fn parse_code(s: &str) -> Option<KeyCode> {
    Some(match s {
        "Enter" | "enter" | "Return" | "return" => KeyCode::Enter,
        "Tab" | "tab" => KeyCode::Tab,
        "Esc" | "esc" | "Escape" | "escape" => KeyCode::Esc,
        "Backspace" | "backspace" => KeyCode::Backspace,
        "Space" | "space" => KeyCode::Char(' '),
        "Up" | "up" => KeyCode::Up,
        "Down" | "down" => KeyCode::Down,
        "Left" | "left" => KeyCode::Left,
        "Right" | "right" => KeyCode::Right,
        "Home" | "home" => KeyCode::Home,
        "End" | "end" => KeyCode::End,
        "PageUp" | "pageup" => KeyCode::PageUp,
        "PageDown" | "pagedown" => KeyCode::PageDown,
        "Delete" | "delete" => KeyCode::Delete,
        other if other.starts_with('F') || other.starts_with('f') => {
            let digits: String = other.chars().filter(|c| c.is_ascii_digit()).collect();
            let n: u8 = digits.parse().ok()?;
            KeyCode::F(n)
        }
        other if other.chars().count() == 1 => {
            let ch = other.chars().next()?;
            KeyCode::Char(ch.to_ascii_lowercase())
        }
        _ => return None,
    })
}

fn display_code(code: &KeyCode) -> String {
    match code {
        KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Up => "Up".to_string(),
        KeyCode::Down => "Down".to_string(),
        KeyCode::Left => "Left".to_string(),
        KeyCode::Right => "Right".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    }
}

fn read_file(path: &Path) -> Option<KeyBindingsFile> {
    let text = fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn defaults_match_classic_keys() {
        let kb = KeyBindings::default();
        assert!(kb.matches("submit", &key(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(kb.matches("quit", &key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(kb.matches("newline", &key(KeyCode::Char('j'), KeyModifiers::CONTROL)));
        assert!(!kb.matches("submit", &key(KeyCode::Char('a'), KeyModifiers::NONE)));
    }

    #[test]
    fn parse_handles_ctrl_alt_combos() {
        let chord = KeyChord::parse("Ctrl+Alt+Enter").unwrap();
        assert!(chord.matches(&key(
            KeyCode::Enter,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        )));
    }

    #[test]
    fn case_insensitive_char_match_with_modifiers() {
        let chord = KeyChord::parse("Ctrl+C").unwrap();
        assert!(chord.matches(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(chord.matches(&key(KeyCode::Char('C'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn apply_overrides_specific_action() {
        let mut kb = KeyBindings::default();
        let file: KeyBindingsFile = toml::from_str(
            r#"
            [bindings]
            submit = ["Alt+Enter"]
            "#,
        )
        .unwrap();
        kb.apply(&file);
        assert!(!kb.matches("submit", &key(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(kb.matches("submit", &key(KeyCode::Enter, KeyModifiers::ALT)));
    }

    #[test]
    fn action_for_returns_known_binding() {
        let kb = KeyBindings::default();
        let action = kb
            .action_for(&key(KeyCode::Char('l'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(action, "clear");
    }
}
