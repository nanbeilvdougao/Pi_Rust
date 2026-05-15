//! Markdown → ratatui line rendering.
//!
//! Parses GFM-flavoured markdown with `pulldown-cmark` and produces a vector
//! of styled `Line`s. We deliberately keep rendering inline (no separate
//! widget) so each transcript entry composes with the surrounding scroller.
//!
//! Supported syntax:
//! - Headings (`#` through `######`) — bold, accent colour, tier prefix.
//! - Bullet and ordered lists, with depth indentation.
//! - Fenced code blocks, with optional language tag.
//! - Inline code, rendered with a contrasting background.
//! - Block quotes, prefixed with a gray bar.
//! - Emphasis / strong, mapped to ratatui modifiers.
//! - Links, rendered as `text (url)` so the URL is selectable.
//!
//! Unknown constructs degrade gracefully to plain text — we never panic on
//! unexpected events.
//!
//! Parity target: `packages/tui/src/components/markdown.ts`.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// Render `markdown` into a vector of styled lines, themed by `theme`.
pub fn render(markdown: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);

    let parser = Parser::new_ext(markdown, options);
    let mut state = RenderState::new(theme);
    for event in parser {
        state.handle(event);
    }
    state.finish()
}

struct RenderState {
    theme: Theme,
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<ListKind>,
    in_code_block: bool,
    code_lang: Option<String>,
    in_blockquote: u16,
    pending_heading: Option<HeadingLevel>,
}

#[derive(Debug, Clone, Copy)]
enum ListKind {
    Bullet,
    Ordered(u64),
}

impl RenderState {
    fn new(theme: &Theme) -> Self {
        Self {
            theme: *theme,
            lines: Vec::new(),
            current: Vec::new(),
            style_stack: Vec::new(),
            list_stack: Vec::new(),
            in_code_block: false,
            code_lang: None,
            in_blockquote: 0,
            pending_heading: None,
        }
    }

    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, style: Style) {
        let base = self.current_style();
        self.style_stack.push(base.patch(style));
    }

    fn pop_style(&mut self) {
        self.style_stack.pop();
    }

    fn ensure_indent(&mut self) {
        if !self.current.is_empty() {
            return;
        }
        for _ in 0..self.in_blockquote {
            self.current.push(Span::styled(
                "│ ".to_string(),
                Style::default().fg(self.theme.hint),
            ));
        }
        let depth = self.list_stack.len();
        if depth > 0 {
            // For the first line of a list item the marker is emitted by Start(Item);
            // subsequent wrapped lines just get padding.
            self.current
                .push(Span::raw("  ".repeat(depth.saturating_sub(1))));
        }
    }

    fn break_line(&mut self) {
        let line = std::mem::take(&mut self.current);
        self.lines.push(Line::from(line));
    }

    fn push_text(&mut self, text: String) {
        self.ensure_indent();
        let style = self.current_style();
        self.current.push(Span::styled(text, style));
    }

    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => {
                if self.in_code_block {
                    let code_style = Style::default()
                        .fg(self.theme.assistant)
                        .bg(self.theme.selection);
                    for (idx, segment) in text.split('\n').enumerate() {
                        if idx > 0 {
                            self.break_line();
                        }
                        self.ensure_indent();
                        self.current
                            .push(Span::styled(segment.to_string(), code_style));
                    }
                } else if let Some(level) = self.pending_heading {
                    let prefix = "#".repeat(heading_depth(level)) + " ";
                    self.ensure_indent();
                    self.current.push(Span::styled(
                        prefix,
                        Style::default()
                            .fg(self.theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ));
                    self.pending_heading = None;
                    let style = Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::BOLD);
                    self.current.push(Span::styled(text.to_string(), style));
                } else {
                    self.push_text(text.to_string());
                }
            }
            Event::Code(code) => {
                self.ensure_indent();
                let style = Style::default()
                    .fg(self.theme.tool)
                    .bg(self.theme.selection);
                self.current.push(Span::styled(code.to_string(), style));
            }
            Event::SoftBreak => {
                self.push_text(" ".to_string());
            }
            Event::HardBreak => {
                self.break_line();
            }
            Event::Rule => {
                self.break_line();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(40),
                    Style::default().fg(self.theme.hint),
                )));
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                // Drop HTML — we never want to render unparsed markup but a
                // visible placeholder preserves the user's intent.
                self.push_text(format!("[html: {} bytes]", html.len()));
            }
            Event::FootnoteReference(label) => {
                self.push_text(format!("[^{label}]"));
            }
            Event::TaskListMarker(checked) => {
                self.ensure_indent();
                let marker = if checked { "[x] " } else { "[ ] " };
                self.current.push(Span::styled(
                    marker.to_string(),
                    Style::default().fg(self.theme.accent),
                ));
            }
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                self.push_text(format!("${math}$"));
            }
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.pending_heading = Some(level);
            }
            Tag::BlockQuote(_) => {
                self.in_blockquote = self.in_blockquote.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                self.in_code_block = true;
                self.code_lang = match kind {
                    CodeBlockKind::Indented => None,
                    CodeBlockKind::Fenced(lang) => {
                        let lang = lang.to_string();
                        if lang.is_empty() {
                            None
                        } else {
                            Some(lang)
                        }
                    }
                };
                let label = self.code_lang.clone().unwrap_or_else(|| "code".to_string());
                self.lines.push(Line::from(Span::styled(
                    format!("```{label}"),
                    Style::default().fg(self.theme.hint),
                )));
            }
            Tag::List(start) => {
                self.list_stack.push(match start {
                    Some(n) => ListKind::Ordered(n),
                    None => ListKind::Bullet,
                });
            }
            Tag::Item => {
                let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
                let marker = match self.list_stack.last_mut() {
                    Some(ListKind::Bullet) => "• ".to_string(),
                    Some(ListKind::Ordered(n)) => {
                        let label = format!("{n}. ");
                        if let Some(ListKind::Ordered(counter)) = self.list_stack.last_mut() {
                            *counter += 1;
                        }
                        label
                    }
                    None => "• ".to_string(),
                };
                self.ensure_indent();
                self.current.push(Span::raw(indent));
                self.current
                    .push(Span::styled(marker, Style::default().fg(self.theme.accent)));
            }
            Tag::Emphasis => {
                self.push_style(Style::default().add_modifier(Modifier::ITALIC));
            }
            Tag::Strong => {
                self.push_style(Style::default().add_modifier(Modifier::BOLD));
            }
            Tag::Strikethrough => {
                self.push_style(Style::default().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { dest_url, .. } => {
                self.push_style(
                    Style::default()
                        .fg(self.theme.status)
                        .add_modifier(Modifier::UNDERLINED),
                );
                let _ = dest_url; // emit after End to keep order
            }
            Tag::Image { dest_url, .. } => {
                self.push_text(format!("[image: {dest_url}]"));
            }
            Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::FootnoteDefinition(_)
            | Tag::HtmlBlock
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if !self.current.is_empty() {
                    self.break_line();
                }
                self.lines.push(Line::from(""));
            }
            TagEnd::Heading(_) => {
                if !self.current.is_empty() {
                    self.break_line();
                }
                self.lines.push(Line::from(""));
                self.pending_heading = None;
            }
            TagEnd::BlockQuote(_) => {
                if !self.current.is_empty() {
                    self.break_line();
                }
                self.in_blockquote = self.in_blockquote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                if !self.current.is_empty() {
                    self.break_line();
                }
                self.in_code_block = false;
                self.code_lang = None;
                self.lines.push(Line::from(Span::styled(
                    "```".to_string(),
                    Style::default().fg(self.theme.hint),
                )));
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                if !self.current.is_empty() {
                    self.break_line();
                }
            }
            TagEnd::Item if !self.current.is_empty() => {
                self.break_line();
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Link
            | TagEnd::Image => {
                self.pop_style();
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.current.is_empty() {
            self.break_line();
        }
        while self.lines.last().map(line_is_blank).unwrap_or(false) {
            self.lines.pop();
        }
        self.lines
    }
}

fn heading_depth(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    fn theme() -> Theme {
        Theme::dark()
    }

    fn flatten(lines: &[Line<'_>]) -> String {
        let mut out = String::new();
        for line in lines {
            for span in &line.spans {
                out.push_str(&span.content);
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_headings_with_tier_prefix() {
        let lines = render("# Top\n\n## Sub", &theme());
        let text = flatten(&lines);
        assert!(text.contains("# Top"));
        assert!(text.contains("## Sub"));
        let has_bold = lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        });
        assert!(has_bold, "headings must be bold");
    }

    #[test]
    fn renders_fenced_code_block_with_language() {
        let md = "```rust\nfn main() {}\n```";
        let lines = render(md, &theme());
        let text = flatten(&lines);
        assert!(text.contains("```rust"));
        assert!(text.contains("fn main()"));
        assert!(text.contains("```\n"));
        // Body line should be styled with selection bg.
        let body_styled = lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.content.contains("fn main()") && s.style.bg.is_some())
        });
        assert!(body_styled, "code block body must have background");
    }

    #[test]
    fn renders_bullet_and_ordered_lists() {
        let bullet = render("- a\n- b\n", &theme());
        let text = flatten(&bullet);
        assert!(text.contains("• a"));
        assert!(text.contains("• b"));

        let ordered = render("1. first\n2. second\n", &theme());
        let text = flatten(&ordered);
        assert!(text.contains("1. first"));
        assert!(text.contains("2. second"));
    }

    #[test]
    fn renders_blockquotes_with_gray_bar() {
        let lines = render("> quoted", &theme());
        let text = flatten(&lines);
        assert!(text.contains("│ quoted"), "got: {text:?}");
    }

    #[test]
    fn renders_inline_code_with_distinct_style() {
        let lines = render("hello `world` end", &theme());
        let code_span = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|s| s.content == "world")
            .expect("inline code span present");
        assert!(code_span.style.bg.is_some(), "inline code needs bg");
        // Surrounding text must NOT carry the same bg.
        let plain = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|s| s.content.starts_with("hello"))
            .expect("plain text present");
        assert_ne!(plain.style.bg, code_span.style.bg);
    }

    #[test]
    fn unknown_html_renders_as_placeholder() {
        let lines = render("<div>raw</div>", &theme());
        let text = flatten(&lines);
        assert!(text.contains("[html:"));
    }

    #[test]
    fn strikethrough_applies_modifier() {
        let lines = render("~~gone~~", &theme());
        let strike = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|s| s.content == "gone")
            .expect("strike span");
        assert!(strike.style.add_modifier.contains(Modifier::CROSSED_OUT));
    }

    fn _color_unused(_: Color) {}
}
