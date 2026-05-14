//! TUI theme.
//!
//! Three built-ins: `dark` (default), `light`, `solarized`. A `.pi/theme.toml`
//! (workspace) or `~/.pi-rust/theme.toml` (user) overrides individual roles.
//!
//! Color names accept either an ANSI name (`yellow`, `lightcyan`, …) or a hex
//! literal (`#1e1e2e`). Unknown values fall back to `reset` so a typo never
//! breaks the UI.

use std::fs;
use std::path::Path;

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ThemeFile {
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub colors: Colors,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Colors {
    pub user: Option<String>,
    pub assistant: Option<String>,
    pub tool: Option<String>,
    pub system: Option<String>,
    pub error: Option<String>,
    pub status: Option<String>,
    pub accent: Option<String>,
    pub hint: Option<String>,
    pub border: Option<String>,
    pub selection: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub user: Color,
    pub assistant: Color,
    pub tool: Color,
    pub system: Color,
    pub error: Color,
    pub status: Color,
    pub accent: Color,
    pub hint: Color,
    pub border: Color,
    pub selection: Color,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            user: Color::Cyan,
            assistant: Color::Green,
            tool: Color::Magenta,
            system: Color::DarkGray,
            error: Color::Red,
            status: Color::Cyan,
            accent: Color::Yellow,
            hint: Color::Gray,
            border: Color::Gray,
            selection: Color::DarkGray,
        }
    }

    pub fn light() -> Self {
        Self {
            user: Color::Blue,
            assistant: Color::Green,
            tool: Color::Magenta,
            system: Color::Gray,
            error: Color::Red,
            status: Color::Blue,
            accent: Color::Yellow,
            hint: Color::DarkGray,
            border: Color::DarkGray,
            selection: Color::Gray,
        }
    }

    pub fn solarized() -> Self {
        Self {
            user: hex("#268bd2"),
            assistant: hex("#859900"),
            tool: hex("#d33682"),
            system: hex("#586e75"),
            error: hex("#dc322f"),
            status: hex("#268bd2"),
            accent: hex("#b58900"),
            hint: hex("#93a1a1"),
            border: hex("#586e75"),
            selection: hex("#073642"),
        }
    }

    pub fn from_base(name: &str) -> Self {
        match name.to_ascii_lowercase().as_str() {
            "light" => Self::light(),
            "solarized" => Self::solarized(),
            _ => Self::dark(),
        }
    }

    pub fn apply(&mut self, overrides: &Colors) {
        if let Some(c) = parse_color(overrides.user.as_deref()) {
            self.user = c;
        }
        if let Some(c) = parse_color(overrides.assistant.as_deref()) {
            self.assistant = c;
        }
        if let Some(c) = parse_color(overrides.tool.as_deref()) {
            self.tool = c;
        }
        if let Some(c) = parse_color(overrides.system.as_deref()) {
            self.system = c;
        }
        if let Some(c) = parse_color(overrides.error.as_deref()) {
            self.error = c;
        }
        if let Some(c) = parse_color(overrides.status.as_deref()) {
            self.status = c;
        }
        if let Some(c) = parse_color(overrides.accent.as_deref()) {
            self.accent = c;
        }
        if let Some(c) = parse_color(overrides.hint.as_deref()) {
            self.hint = c;
        }
        if let Some(c) = parse_color(overrides.border.as_deref()) {
            self.border = c;
        }
        if let Some(c) = parse_color(overrides.selection.as_deref()) {
            self.selection = c;
        }
    }

    pub fn load_layered(workspace_root: Option<&Path>, override_name: Option<&str>) -> Self {
        let mut theme = Self::dark();
        // user
        if let Ok(home) = std::env::var("HOME") {
            let path = Path::new(&home).join(".pi-rust").join("theme.toml");
            if let Some(file) = read(&path) {
                if let Some(base) = file.base.as_deref() {
                    theme = Self::from_base(base);
                }
                theme.apply(&file.colors);
            }
        }
        if let Some(root) = workspace_root {
            let path = root.join(".pi").join("theme.toml");
            if let Some(file) = read(&path) {
                if let Some(base) = file.base.as_deref() {
                    theme = Self::from_base(base);
                }
                theme.apply(&file.colors);
            }
        }
        if let Some(name) = override_name {
            theme = Self::from_base(name);
        }
        theme
    }
}

fn read(path: &Path) -> Option<ThemeFile> {
    let text = fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

fn parse_color(value: Option<&str>) -> Option<Color> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(stripped) = value.strip_prefix('#') {
        return Some(parse_hex(stripped));
    }
    Some(match value.to_ascii_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" | "purple" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        "reset" => Color::Reset,
        _ => return None,
    })
}

fn parse_hex(input: &str) -> Color {
    if input.len() != 6 {
        return Color::Reset;
    }
    let r = u8::from_str_radix(&input[0..2], 16).unwrap_or(0);
    let g = u8::from_str_radix(&input[2..4], 16).unwrap_or(0);
    let b = u8::from_str_radix(&input[4..6], 16).unwrap_or(0);
    Color::Rgb(r, g, b)
}

fn hex(s: &str) -> Color {
    parse_color(Some(s)).unwrap_or(Color::Reset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_theme_defaults() {
        let theme = Theme::dark();
        assert_eq!(theme.user, Color::Cyan);
        assert_eq!(theme.assistant, Color::Green);
    }

    #[test]
    fn from_base_picks_built_in() {
        assert_eq!(Theme::from_base("light").user, Color::Blue);
        assert_eq!(Theme::from_base("solarized").user, hex("#268bd2"));
        assert_eq!(Theme::from_base("xxx").user, Theme::dark().user);
    }

    #[test]
    fn overrides_modify_theme() {
        let mut theme = Theme::dark();
        let file: ThemeFile = toml::from_str(concat!(
            "base = \"dark\"\n",
            "[colors]\n",
            "user = \"yellow\"\n",
            "assistant = \"#ff0099\"\n",
        ))
        .unwrap();
        theme.apply(&file.colors);
        assert_eq!(theme.user, Color::Yellow);
        assert_eq!(theme.assistant, Color::Rgb(0xff, 0x00, 0x99));
    }

    #[test]
    fn hex_parser_handles_six_digits() {
        assert_eq!(hex("#1e1e2e"), Color::Rgb(0x1e, 0x1e, 0x2e));
    }
}
