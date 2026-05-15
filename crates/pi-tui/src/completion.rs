//! Autocomplete engine for the interactive TUI.
//!
//! Two trigger forms:
//!
//! - `/` at the start of a word → slash command completion. Sources come
//!   from the agent's `SlashRegistry` (built-in + workspace `.pi/commands/`).
//! - `@` inside a word → file completion. Sources come from a lazy walk of
//!   the workspace, capped at `MAX_FILE_INDEX` entries with directories
//!   pruned by name (`.git`, `target`, `node_modules`, `.pi-rust`).
//!
//! Both flows share a single `Completer` so the TUI just feeds it the current
//! input and cursor position and gets back a candidate list. Selection
//! handling lives in the TUI (Tab/Enter/Esc semantics).
//!
//! Fuzzy ranking is a subsequence matcher with two bonuses:
//! - Word-boundary or path-separator characters score 3× a normal hit.
//! - Earlier hits score higher than later hits.
//!
//! This is good enough to make `@cli/m` rank `crates/pi-cli/src/main.rs` first
//! without pulling a third-party fuzzy crate.

use std::path::{Path, PathBuf};

use pi_agent::SlashRegistry;
use walkdir::WalkDir;

const MAX_FILE_INDEX: usize = 5000;
const PRUNE_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".pi-rust",
    "dist",
    "build",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    SlashCommand,
    FilePath,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub kind: CompletionKind,
    pub display: String,
    /// What to replace the trigger range with when accepted.
    pub insert: String,
    /// Optional secondary text shown after the display label.
    pub hint: Option<String>,
}

/// Range in `input` (byte offsets) that the active trigger covers.
/// Replacing this range with the chosen candidate's `insert` is the apply step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerSpan {
    pub start: usize,
    pub end: usize,
    pub kind: CompletionKind,
}

pub struct Completer {
    file_index: Vec<PathBuf>,
    workspace_root: PathBuf,
}

impl Completer {
    pub fn new(workspace_root: impl AsRef<Path>) -> Self {
        let root = workspace_root.as_ref().to_path_buf();
        let file_index = scan_files(&root);
        Self {
            file_index,
            workspace_root: root,
        }
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn file_index_len(&self) -> usize {
        self.file_index.len()
    }

    /// Look at `input` up to `cursor` and decide whether we're in a trigger
    /// span. Returns `None` if no completion should fire.
    pub fn detect_trigger(&self, input: &str, cursor: usize) -> Option<TriggerSpan> {
        let cursor = cursor.min(input.len());
        let prefix = &input[..cursor];

        // @ file trigger — scan back to the nearest whitespace or start.
        if let Some(at_pos) = prefix.rfind('@') {
            // Reject if there's whitespace between `@` and the cursor — means
            // the user already finished the file token.
            let after_at = &prefix[at_pos + 1..];
            if !after_at.contains(char::is_whitespace) {
                // Also require `@` to be at start of input or preceded by
                // whitespace, so emails like `me@host` don't trigger.
                let at_start = at_pos == 0
                    || prefix[..at_pos]
                        .chars()
                        .last()
                        .map_or(true, |c| c.is_whitespace());
                if at_start {
                    return Some(TriggerSpan {
                        start: at_pos,
                        end: cursor,
                        kind: CompletionKind::FilePath,
                    });
                }
            }
        }

        // / slash command trigger — only when the whole input starts with `/`
        // and contains no whitespace yet (single command, no args).
        if let Some(stripped) = prefix.strip_prefix('/') {
            if !stripped.contains(char::is_whitespace) {
                return Some(TriggerSpan {
                    start: 0,
                    end: cursor,
                    kind: CompletionKind::SlashCommand,
                });
            }
        }

        None
    }

    pub fn candidates(
        &self,
        input: &str,
        span: TriggerSpan,
        slash: &SlashRegistry,
        max: usize,
    ) -> Vec<CompletionItem> {
        let query = &input[span.start..span.end];
        match span.kind {
            CompletionKind::SlashCommand => self.slash_candidates(query, slash, max),
            CompletionKind::FilePath => self.file_candidates(query, max),
        }
    }

    fn slash_candidates(
        &self,
        query: &str,
        slash: &SlashRegistry,
        max: usize,
    ) -> Vec<CompletionItem> {
        // Query includes the leading '/'. Strip it for matching.
        let needle = query.trim_start_matches('/');
        let mut scored: Vec<(i32, CompletionItem)> = Vec::new();
        for command in slash.list() {
            let name = command.name.trim_start_matches('/');
            if let Some(score) = fuzzy_score(name, needle) {
                scored.push((
                    score,
                    CompletionItem {
                        kind: CompletionKind::SlashCommand,
                        display: command.name.to_string(),
                        insert: command.name.to_string(),
                        hint: Some(command.description.to_string()),
                    },
                ));
            }
        }
        scored.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        scored.into_iter().take(max).map(|(_, item)| item).collect()
    }

    fn file_candidates(&self, query: &str, max: usize) -> Vec<CompletionItem> {
        let needle = query.trim_start_matches('@');
        let mut scored: Vec<(i32, CompletionItem)> = Vec::new();
        for path in &self.file_index {
            let display = path.display().to_string();
            if let Some(score) = fuzzy_score(&display, needle) {
                scored.push((
                    score,
                    CompletionItem {
                        kind: CompletionKind::FilePath,
                        display: display.clone(),
                        insert: format!("@{display}"),
                        hint: None,
                    },
                ));
            }
        }
        scored.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        scored.into_iter().take(max).map(|(_, item)| item).collect()
    }

    pub fn apply(&self, input: &mut String, span: TriggerSpan, item: &CompletionItem) -> usize {
        let new_cursor = span.start + item.insert.len();
        input.replace_range(span.start..span.end, &item.insert);
        new_cursor
    }

    /// Read the file referenced by `@<path>` so the agent can use it as
    /// context. Empty input returns `Ok(String::new())` so the caller can
    /// fold the result unconditionally.
    pub fn read_reference(&self, reference: &str) -> std::io::Result<String> {
        let path = reference.trim_start_matches('@');
        if path.is_empty() {
            return Ok(String::new());
        }
        let resolved = self.workspace_root.join(path);
        std::fs::read_to_string(resolved)
    }
}

fn scan_files(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(1024);
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            if PRUNE_DIRS.contains(&name.as_ref()) {
                return false;
            }
            true
        });
    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path().strip_prefix(root).unwrap_or(entry.path());
        out.push(path.to_path_buf());
        if out.len() >= MAX_FILE_INDEX {
            break;
        }
    }
    out.sort();
    out
}

/// Subsequence fuzzy match. Returns `None` when no match, or `Some(score)`
/// where higher is better. Score is built from:
/// - Base 100 for matching at all.
/// - +20 per character that lands on a word boundary or after `/` `_` `-`.
/// - -1 per gap character (encourages denser matches).
fn fuzzy_score(haystack: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(50);
    }
    let haystack_lower = haystack.to_ascii_lowercase();
    let needle_lower = needle.to_ascii_lowercase();
    let haystack_bytes = haystack_lower.as_bytes();
    let needle_bytes = needle_lower.as_bytes();
    let mut score: i32 = 100;
    let mut hi = 0usize;
    let mut prev_match: Option<usize> = None;
    for &nb in needle_bytes {
        let mut found = None;
        while hi < haystack_bytes.len() {
            if haystack_bytes[hi] == nb {
                found = Some(hi);
                hi += 1;
                break;
            }
            hi += 1;
        }
        let pos = found?;
        let boundary =
            pos == 0 || matches!(haystack_bytes[pos - 1], b'/' | b'_' | b'-' | b'.' | b' ');
        if boundary {
            score += 20;
        }
        if let Some(prev) = prev_match {
            score -= (pos - prev - 1) as i32;
        }
        prev_match = Some(pos);
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn workspace() -> (tempfile::TempDir, Completer) {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir_all(root.join("crates/pi-cli/src")).unwrap();
        fs::write(root.join("crates/pi-cli/src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("crates/pi-cli/src/rpc.rs"), "// rpc").unwrap();
        fs::write(root.join("README.md"), "# pi").unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("target/debug/pi"), "skip").unwrap();
        let completer = Completer::new(root);
        (dir, completer)
    }

    #[test]
    fn slash_trigger_detected_at_start() {
        let (_dir, completer) = workspace();
        let span = completer.detect_trigger("/he", 3).expect("trigger");
        assert_eq!(span.kind, CompletionKind::SlashCommand);
        assert_eq!(span.start, 0);
        assert_eq!(span.end, 3);
    }

    #[test]
    fn at_trigger_detected_after_space() {
        let (_dir, completer) = workspace();
        let span = completer.detect_trigger("look @cli/", 10).expect("trigger");
        assert_eq!(span.kind, CompletionKind::FilePath);
        assert_eq!(&"look @cli/"[span.start..span.end], "@cli/");
    }

    #[test]
    fn email_does_not_trigger_at_file() {
        let (_dir, completer) = workspace();
        assert!(completer.detect_trigger("me@host", 7).is_none());
    }

    #[test]
    fn file_completion_finds_pi_cli_main() {
        let (_dir, completer) = workspace();
        let input = "@cli/main";
        let span = completer
            .detect_trigger(input, input.len())
            .expect("trigger");
        let items = completer.candidates(input, span, &SlashRegistry::builtin(), 5);
        assert!(items.iter().any(|item| item.display.contains("main.rs")));
    }

    #[test]
    fn prune_target_dir() {
        let (_dir, completer) = workspace();
        assert!(!completer.file_index.iter().any(|p| p.starts_with("target")));
    }

    #[test]
    fn fuzzy_score_prefers_word_boundary_hits() {
        let with_boundary = fuzzy_score("pi-cli/src/main.rs", "main").unwrap();
        let without_boundary = fuzzy_score("pi-cli/src/main.rs", "in").unwrap();
        assert!(with_boundary > without_boundary);
    }

    #[test]
    fn slash_candidates_match_builtin_commands() {
        let (_dir, completer) = workspace();
        let span = completer.detect_trigger("/he", 3).expect("trigger");
        let items = completer.candidates("/he", span, &SlashRegistry::builtin(), 5);
        assert!(items.iter().any(|item| item.display == "/help"));
    }

    #[test]
    fn apply_replaces_trigger_range_and_returns_cursor() {
        let (_dir, completer) = workspace();
        let mut input = String::from("/he");
        let span = completer
            .detect_trigger(&input, input.len())
            .expect("trigger");
        let item = CompletionItem {
            kind: CompletionKind::SlashCommand,
            display: "/help".to_string(),
            insert: "/help".to_string(),
            hint: None,
        };
        let cursor = completer.apply(&mut input, span, &item);
        assert_eq!(input, "/help");
        assert_eq!(cursor, 5);
    }

    #[test]
    fn read_reference_reads_workspace_file() {
        let (_dir, completer) = workspace();
        let content = completer.read_reference("@README.md").expect("read");
        assert!(content.contains("pi"));
    }
}
