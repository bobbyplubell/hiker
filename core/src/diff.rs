// status: diff-core-module
//
// Pure line-diff computation. Mirrors the discipline used elsewhere in core
// (`rusqlite` confined to `store`, `fastembed` to `embed`, `notify` to
// `watcher`): the `similar` crate is confined to this module and never
// leaks past the `compute` boundary. Callers see only `DiffResult` /
// `DiffHunk` / `DiffLine` values.
//
// No I/O, no async, no state — just text → diff. See docs/diff.md.

use serde::{Deserialize, Serialize};
use similar::{Algorithm, ChangeTag, TextDiff};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffOp {
    Equal,
    Insert,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub op: DiffOp,
    pub line: String,
    pub before_line_no: Option<u32>,
    pub after_line_no: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffResult {
    pub hunks: Vec<DiffHunk>,
}

/// Compute a unified-diff representation of `before` → `after`.
///
/// v1 emits a single hunk containing every line — context, inserts, and
/// deletes — so the UI can render the whole file with changes highlighted
/// inline. Identical inputs return one hunk full of `Equal` lines, so the
/// caller still sees the file. The list-of-hunks shape stays in the wire
/// format so a future consumer (large diffs, MCP) can opt into a grouped
/// representation by calling a different entry point without reshaping the
/// existing call sites.
pub fn compute(before: &str, after: &str) -> DiffResult {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .diff_lines(before, after);
    let mut lines: Vec<DiffLine> = Vec::new();
    for change in diff.iter_all_changes() {
        let kind = match change.tag() {
            ChangeTag::Equal => DiffOp::Equal,
            ChangeTag::Insert => DiffOp::Insert,
            ChangeTag::Delete => DiffOp::Delete,
        };
        // `similar` keeps the trailing newline on each line; strip it so
        // the wire format carries one logical line per entry.
        let mut text = change.value().to_string();
        if text.ends_with('\n') {
            text.pop();
            if text.ends_with('\r') {
                text.pop();
            }
        }
        lines.push(DiffLine {
            op: kind,
            line: text,
            before_line_no: change.old_index().map(|i| (i + 1) as u32),
            after_line_no: change.new_index().map(|i| (i + 1) as u32),
        });
    }
    if lines.is_empty() {
        return DiffResult { hunks: Vec::new() };
    }
    DiffResult {
        hunks: vec![DiffHunk { lines }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_inputs_produce_all_equal_lines() {
        let r = compute("a\nb\nc\n", "a\nb\nc\n");
        assert_eq!(r.hunks.len(), 1);
        assert!(r.hunks[0].lines.iter().all(|l| l.op == DiffOp::Equal));
    }

    #[test]
    fn empty_inputs_produce_no_hunks() {
        let r = compute("", "");
        assert!(r.hunks.is_empty());
    }

    #[test]
    fn pure_insert_produces_one_hunk_with_inserts_and_context() {
        let r = compute("a\nb\nc\n", "a\nb\nNEW\nc\n");
        assert_eq!(r.hunks.len(), 1);
        let kinds: Vec<DiffOp> = r.hunks[0].lines.iter().map(|l| l.op).collect();
        assert!(kinds.contains(&DiffOp::Insert));
        assert!(kinds.contains(&DiffOp::Equal));
    }

    #[test]
    fn pure_delete_carries_before_line_numbers() {
        let r = compute("a\nb\nc\n", "a\nc\n");
        assert_eq!(r.hunks.len(), 1);
        let removed: Vec<&DiffLine> = r.hunks[0]
            .lines
            .iter()
            .filter(|l| l.op == DiffOp::Delete)
            .collect();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].line, "b");
        assert_eq!(removed[0].before_line_no, Some(2));
        assert_eq!(removed[0].after_line_no, None);
    }

    #[test]
    fn distant_changes_keep_unchanged_lines_visible() {
        // v1 emits a single hunk; the unchanged middle lines must still
        // appear so the UI can render the whole file with changes inline.
        let mut before = String::from("X\n");
        for i in 0..20 {
            before.push_str(&format!("line-{i}\n"));
        }
        before.push_str("Y\n");
        let mut after = String::from("X-CHANGED\n");
        for i in 0..20 {
            after.push_str(&format!("line-{i}\n"));
        }
        after.push_str("Y-CHANGED\n");
        let r = compute(&before, &after);
        assert_eq!(r.hunks.len(), 1);
        let equals = r.hunks[0]
            .lines
            .iter()
            .filter(|l| l.op == DiffOp::Equal)
            .count();
        assert!(equals >= 20, "expected ≥20 equal context lines, got {equals}");
    }

    #[test]
    fn newline_stripping_handles_crlf() {
        let r = compute("a\r\nb\r\n", "a\r\nB\r\n");
        let changed: Vec<&DiffLine> = r.hunks[0]
            .lines
            .iter()
            .filter(|l| l.op != DiffOp::Equal)
            .collect();
        for l in changed {
            assert!(!l.line.ends_with('\n'));
            assert!(!l.line.ends_with('\r'));
        }
    }
}
