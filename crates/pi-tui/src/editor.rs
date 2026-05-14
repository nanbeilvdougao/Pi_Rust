//! Multi-line text editor for the TUI input pane.
//!
//! Replaces the previous single-buffer + cursor index with a more capable
//! widget that mirrors TS pi's `tui/editor-component.ts` plus emacs-style
//! kill-ring and undo-stack from `tui/kill-ring.ts` and `tui/undo-stack.ts`.
//!
//! Features:
//! - Logical lines (vec of strings); cursor is `(row, col)`.
//! - Word-skip motion: prev_word / next_word jumps over `\w` runs.
//! - Home / End on the current line, plus document-start / -end.
//! - Backward / forward delete; word-level delete.
//! - Kill-ring with rotating yank (`Ctrl+K` / `Ctrl+Y` / `Alt+Y`).
//! - Undo / redo stacks with coalescing of consecutive single-char inserts.
//!
//! The editor is intentionally self-contained: no ratatui or crossterm
//! imports here. The TUI's `run_app` consumes `key_event(&mut Editor,
//! &KeyBindings, KeyEvent) -> Action` and renders `editor.lines()` as a
//! `Paragraph`.

use std::collections::VecDeque;

/// Result of feeding a single key into the editor: tells the host whether
/// to submit the buffer, fire autocomplete, or do nothing observable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorAction {
    Nothing,
    Submit,
    RequestCompletion,
    CancelCompletion,
    Paste,
    Clear,
    Quit,
    HistoryPrev,
    HistoryNext,
}

#[derive(Debug, Clone)]
pub struct Editor {
    lines: Vec<String>,
    row: usize,
    col: usize, // byte offset within current line
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    kill_ring: VecDeque<String>,
    /// Marks the most recent action that inserted a single char so we can
    /// coalesce a run of typing into one undo entry.
    last_was_char_insert: bool,
}

#[derive(Debug, Clone)]
struct Snapshot {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

const KILL_RING_CAP: usize = 32;
const UNDO_CAP: usize = 200;

impl Default for Editor {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            kill_ring: VecDeque::new(),
            last_was_char_insert: false,
        }
    }
}

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn flat(&self) -> String {
        self.lines.join("\n")
    }

    pub fn set_text(&mut self, text: &str) {
        self.snapshot();
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(String::from).collect()
        };
        self.row = self.lines.len().saturating_sub(1);
        self.col = self.lines.last().map(|s| s.len()).unwrap_or(0);
        self.last_was_char_insert = false;
    }

    pub fn clear(&mut self) {
        self.snapshot();
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        self.last_was_char_insert = false;
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.is_empty())
    }

    pub fn insert_char(&mut self, ch: char) {
        if !self.last_was_char_insert {
            self.snapshot();
        }
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        self.lines[self.row].insert_str(self.col, s);
        self.col += s.len();
        self.last_was_char_insert = true;
    }

    pub fn insert_str(&mut self, s: &str) {
        self.snapshot();
        let mut first = true;
        for chunk in s.split('\n') {
            if !first {
                let tail = self.lines[self.row].split_off(self.col);
                self.row += 1;
                self.lines.insert(self.row, tail);
                self.col = 0;
            }
            self.lines[self.row].insert_str(self.col, chunk);
            self.col += chunk.len();
            first = false;
        }
        self.last_was_char_insert = false;
    }

    pub fn newline(&mut self) {
        self.snapshot();
        let tail = self.lines[self.row].split_off(self.col);
        self.row += 1;
        self.lines.insert(self.row, tail);
        self.col = 0;
        self.last_was_char_insert = false;
    }

    pub fn backspace(&mut self) {
        if self.col == 0 && self.row == 0 {
            return;
        }
        self.snapshot();
        if self.col == 0 {
            let removed = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].push_str(&removed);
        } else {
            let prev = prev_char_boundary(&self.lines[self.row], self.col);
            self.lines[self.row].replace_range(prev..self.col, "");
            self.col = prev;
        }
        self.last_was_char_insert = false;
    }

    pub fn delete_forward(&mut self) {
        if self.col == self.lines[self.row].len() && self.row + 1 >= self.lines.len() {
            return;
        }
        self.snapshot();
        if self.col == self.lines[self.row].len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        } else {
            let next = next_char_boundary(&self.lines[self.row], self.col);
            self.lines[self.row].replace_range(self.col..next, "");
        }
        self.last_was_char_insert = false;
    }

    pub fn move_left(&mut self) {
        if self.col == 0 {
            if self.row > 0 {
                self.row -= 1;
                self.col = self.lines[self.row].len();
            }
        } else {
            self.col = prev_char_boundary(&self.lines[self.row], self.col);
        }
    }

    pub fn move_right(&mut self) {
        if self.col == self.lines[self.row].len() {
            if self.row + 1 < self.lines.len() {
                self.row += 1;
                self.col = 0;
            }
        } else {
            self.col = next_char_boundary(&self.lines[self.row], self.col);
        }
    }

    pub fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.lines[self.row].len());
        }
    }

    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.lines[self.row].len());
        }
    }

    pub fn move_home(&mut self) {
        self.col = 0;
    }

    pub fn move_end(&mut self) {
        self.col = self.lines[self.row].len();
    }

    pub fn move_word_left(&mut self) {
        let line = &self.lines[self.row];
        let bytes = line.as_bytes();
        let mut idx = self.col;
        while idx > 0 && !is_word(bytes[idx - 1]) {
            idx -= 1;
        }
        while idx > 0 && is_word(bytes[idx - 1]) {
            idx -= 1;
        }
        self.col = idx;
    }

    pub fn move_word_right(&mut self) {
        let line = &self.lines[self.row];
        let bytes = line.as_bytes();
        let mut idx = self.col;
        while idx < bytes.len() && is_word(bytes[idx]) {
            idx += 1;
        }
        while idx < bytes.len() && !is_word(bytes[idx]) {
            idx += 1;
        }
        self.col = idx;
    }

    pub fn kill_to_end_of_line(&mut self) {
        self.snapshot();
        let line = &mut self.lines[self.row];
        if self.col < line.len() {
            let killed = line.split_off(self.col);
            self.push_kill_ring(killed);
        } else if self.row + 1 < self.lines.len() {
            // At line end: join next line, kill the newline character.
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
            self.push_kill_ring("\n".to_string());
        }
        self.last_was_char_insert = false;
    }

    pub fn yank(&mut self) {
        if let Some(text) = self.kill_ring.front().cloned() {
            self.insert_str(&text);
        }
    }

    pub fn yank_rotate(&mut self) {
        if self.kill_ring.len() < 2 {
            return;
        }
        if let Some(front) = self.kill_ring.pop_front() {
            self.kill_ring.push_back(front);
        }
    }

    fn push_kill_ring(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        if self.kill_ring.len() == KILL_RING_CAP {
            self.kill_ring.pop_back();
        }
        self.kill_ring.push_front(text);
    }

    pub fn undo(&mut self) {
        if let Some(snap) = self.undo.pop() {
            self.redo.push(Snapshot {
                lines: self.lines.clone(),
                row: self.row,
                col: self.col,
            });
            self.lines = snap.lines;
            self.row = snap.row;
            self.col = snap.col;
            self.last_was_char_insert = false;
        }
    }

    pub fn redo(&mut self) {
        if let Some(snap) = self.redo.pop() {
            self.undo.push(Snapshot {
                lines: self.lines.clone(),
                row: self.row,
                col: self.col,
            });
            self.lines = snap.lines;
            self.row = snap.row;
            self.col = snap.col;
            self.last_was_char_insert = false;
        }
    }

    fn snapshot(&mut self) {
        if self.undo.len() == UNDO_CAP {
            self.undo.remove(0);
        }
        self.undo.push(Snapshot {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        });
        self.redo.clear();
    }
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn prev_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.saturating_sub(1);
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn next_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace_round_trip() {
        let mut e = Editor::new();
        e.insert_char('a');
        e.insert_char('b');
        e.insert_char('c');
        assert_eq!(e.flat(), "abc");
        e.backspace();
        assert_eq!(e.flat(), "ab");
    }

    #[test]
    fn newline_splits_at_cursor() {
        let mut e = Editor::new();
        e.insert_str("hello world");
        for _ in 0..6 {
            e.move_left();
        }
        e.newline();
        assert_eq!(e.flat(), "hello\n world");
    }

    #[test]
    fn word_left_jumps_over_word() {
        let mut e = Editor::new();
        e.insert_str("foo bar baz");
        e.move_word_left();
        assert_eq!(e.cursor(), (0, 8));
    }

    #[test]
    fn kill_then_yank_recovers_text() {
        let mut e = Editor::new();
        e.insert_str("hello world");
        for _ in 0..6 {
            e.move_left();
        }
        e.kill_to_end_of_line();
        assert_eq!(e.flat(), "hello");
        e.yank();
        assert_eq!(e.flat(), "hello world");
    }

    #[test]
    fn undo_reverts_insert() {
        let mut e = Editor::new();
        e.insert_str("hello");
        let snap = e.flat();
        e.insert_str(" world");
        e.undo();
        assert_eq!(e.flat(), snap);
        e.redo();
        assert_eq!(e.flat(), "hello world");
    }

    #[test]
    fn home_and_end_jump_within_line() {
        let mut e = Editor::new();
        e.insert_str("first\nsecond");
        e.move_home();
        assert_eq!(e.cursor(), (1, 0));
        e.move_end();
        assert_eq!(e.cursor(), (1, 6));
    }

    #[test]
    fn yank_rotate_cycles_kill_ring() {
        let mut e = Editor::new();
        // Build two distinct kills by inserting two lines and killing each.
        e.insert_str("first");
        e.move_home();
        e.kill_to_end_of_line();
        e.insert_str("second");
        e.move_home();
        e.kill_to_end_of_line();
        // Newest kill ("second") yanks first.
        e.yank();
        assert_eq!(e.flat(), "second");
        e.clear();
        // Rotate puts "second" to the back, so the next yank inserts "first".
        e.yank_rotate();
        e.yank();
        assert_eq!(e.flat(), "first");
    }
}
