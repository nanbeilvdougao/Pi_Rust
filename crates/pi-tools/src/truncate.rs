//! Output truncation helpers used by tools that may produce unbounded output
//! (bash, grep, find, read with no limit).
//!
//! Truncation strategy mirrors TS pi's `output_accumulator`:
//! - Cap by both byte size and line count.
//! - Keep a head and tail slice when over the cap; drop the middle.
//! - Annotate the dropped middle with a single `... <n> lines / <m> bytes elided ...`
//!   line so the assistant knows the output was truncated.

const DEFAULT_BYTE_CAP: usize = 32 * 1024;
const DEFAULT_LINE_CAP: usize = 400;
const HEAD_LINES: usize = 100;
const TAIL_LINES: usize = 100;

#[derive(Debug, Clone, Copy)]
pub struct TruncationPolicy {
    pub byte_cap: usize,
    pub line_cap: usize,
    pub head_lines: usize,
    pub tail_lines: usize,
}

impl Default for TruncationPolicy {
    fn default() -> Self {
        Self {
            byte_cap: DEFAULT_BYTE_CAP,
            line_cap: DEFAULT_LINE_CAP,
            head_lines: HEAD_LINES,
            tail_lines: TAIL_LINES,
        }
    }
}

pub fn truncate(text: &str, policy: TruncationPolicy) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let line_count = lines.len();
    let byte_count = text.len();
    if byte_count <= policy.byte_cap && line_count <= policy.line_cap {
        return text.to_string();
    }
    let head_take = policy.head_lines.min(line_count);
    let tail_take = policy.tail_lines.min(line_count.saturating_sub(head_take));
    let dropped_lines = line_count.saturating_sub(head_take + tail_take);
    let head: String = lines.iter().take(head_take).copied().collect();
    let tail_start = line_count.saturating_sub(tail_take);
    let tail: String = lines[tail_start..].concat();
    let dropped_bytes = byte_count
        .saturating_sub(head.len())
        .saturating_sub(tail.len());

    let mut out = String::with_capacity(head.len() + tail.len() + 64);
    out.push_str(&head);
    out.push_str(&format!(
        "\n... {dropped_lines} lines / {dropped_bytes} bytes elided ...\n"
    ));
    out.push_str(&tail);
    if out.len() > policy.byte_cap {
        // Hard cap by bytes in case head/tail alone overshoot.
        let cut = out
            .char_indices()
            .rev()
            .find(|(idx, _)| *idx <= policy.byte_cap);
        if let Some((idx, _)) = cut {
            out.truncate(idx);
            out.push_str("\n... output truncated ...");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_input_is_unchanged() {
        let policy = TruncationPolicy::default();
        let result = truncate("hello\n", policy);
        assert_eq!(result, "hello\n");
    }

    #[test]
    fn long_input_keeps_head_and_tail() {
        let big = (0..2000).map(|i| format!("line {i}\n")).collect::<String>();
        let policy = TruncationPolicy::default();
        let result = truncate(&big, policy);
        assert!(result.contains("line 0\n"));
        assert!(result.contains("line 1999\n"));
        assert!(result.contains("lines /"));
    }
}
