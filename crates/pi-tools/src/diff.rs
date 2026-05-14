//! Tiny unified-diff renderer for tool previews. Backed by `similar`.

use similar::{ChangeTag, TextDiff};

pub fn unified(before: &str, after: &str, path: &str) -> String {
    let diff = TextDiff::from_lines(before, after);
    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
    for group in diff.grouped_ops(3) {
        if group.is_empty() {
            continue;
        }
        let (mut a_start, mut a_end, mut b_start, mut b_end) =
            (usize::MAX, 0usize, usize::MAX, 0usize);
        for op in &group {
            a_start = a_start.min(op.old_range().start);
            a_end = a_end.max(op.old_range().end);
            b_start = b_start.min(op.new_range().start);
            b_end = b_end.max(op.new_range().end);
        }
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            a_start + 1,
            a_end - a_start,
            b_start + 1,
            b_end - b_start
        ));
        for op in group {
            for change in diff.iter_changes(&op) {
                let prefix = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                out.push_str(prefix);
                out.push_str(change.value());
                if !change.value().ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_minus_and_plus_lines() {
        let diff = unified("a\nb\n", "a\nB\n", "file.txt");
        assert!(diff.contains("-b"));
        assert!(diff.contains("+B"));
    }
}
