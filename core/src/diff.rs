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

/// Character-/word-level span inside a paired delete/insert line. See
/// `diff-intraline-core-pair`. Byte offsets are into the line's UTF-8 bytes
/// *without* its trailing newline (the wire format strips newlines per the
/// note in `compute`). For `Equal`, both sides are populated; for `Insert`
/// only the after-side range is meaningful (before fields are zero) and
/// vice versa for `Delete`. v1 ships char-level via `similar::TextDiff::
/// from_chars`; the wire format keeps both before/after ranges so word-
/// level (`diff-intraline-char-level-v1`) can drop in without a reshape.
// status: diff-intraline-char-level-v1
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntralineSpan {
    pub op: DiffOp,
    pub byte_start_before: u32,
    pub byte_end_before: u32,
    pub byte_start_after: u32,
    pub byte_end_after: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub op: DiffOp,
    pub line: String,
    pub before_line_no: Option<u32>,
    pub after_line_no: Option<u32>,
    /// Character-level spans for paired delete/insert lines when the caller
    /// asked for intraline (`compute_with_intraline(.., true)`). `None` for
    /// equal lines, unpaired deletes/inserts, and every line when intraline
    /// wasn't requested.
    // status: diff-intraline-ipc-flag
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intraline_spans: Option<Vec<IntralineSpan>>,
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
    compute_with_intraline(before, after, false)
}

/// Same as `compute` but populates `DiffLine::intraline_spans` for each
/// paired delete/insert line when `intraline` is true. A pair is a delete
/// line immediately followed by an insert line in the same hunk; unpaired
/// deletes/inserts and equal lines are left untouched. v1 char-level only
/// per `diff-intraline-char-level-v1`.
// status: diff-intraline-core-pair
pub fn compute_with_intraline(before: &str, after: &str, intraline: bool) -> DiffResult {
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
            intraline_spans: None,
        });
    }
    if lines.is_empty() {
        return DiffResult { hunks: Vec::new() };
    }
    if intraline {
        // Pair scan: a Delete immediately followed by an Insert is a pair.
        // Mutate via indices so the borrow checker stays happy with two
        // simultaneous &muts (one Delete + one Insert).
        let mut i = 0;
        while i + 1 < lines.len() {
            if lines[i].op == DiffOp::Delete && lines[i + 1].op == DiffOp::Insert {
                let spans = compute_intraline(&lines[i].line, &lines[i + 1].line);
                lines[i].intraline_spans = Some(spans.clone());
                lines[i + 1].intraline_spans = Some(spans);
                i += 2;
            } else {
                i += 1;
            }
        }
    }
    DiffResult {
        hunks: vec![DiffHunk { lines }],
    }
}

/// Char-level diff over a single paired delete/insert line. Returns the
/// equal/insert/delete spans as `IntralineSpan`s with both before- and
/// after-side byte offsets populated where meaningful (insert spans leave
/// before zeroed; delete spans leave after zeroed).
// status: diff-intraline-core-pair
pub fn compute_intraline(before_line: &str, after_line: &str) -> Vec<IntralineSpan> {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .diff_chars(before_line, after_line);
    let mut out: Vec<IntralineSpan> = Vec::new();
    let mut b_off: u32 = 0;
    let mut a_off: u32 = 0;
    for change in diff.iter_all_changes() {
        let text = change.value();
        let len = text.len() as u32;
        match change.tag() {
            ChangeTag::Equal => {
                out.push(IntralineSpan {
                    op: DiffOp::Equal,
                    byte_start_before: b_off,
                    byte_end_before: b_off + len,
                    byte_start_after: a_off,
                    byte_end_after: a_off + len,
                });
                b_off += len;
                a_off += len;
            }
            ChangeTag::Delete => {
                out.push(IntralineSpan {
                    op: DiffOp::Delete,
                    byte_start_before: b_off,
                    byte_end_before: b_off + len,
                    byte_start_after: 0,
                    byte_end_after: 0,
                });
                b_off += len;
            }
            ChangeTag::Insert => {
                out.push(IntralineSpan {
                    op: DiffOp::Insert,
                    byte_start_before: 0,
                    byte_end_before: 0,
                    byte_start_after: a_off,
                    byte_end_after: a_off + len,
                });
                a_off += len;
            }
        }
    }
    out
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
    fn intraline_pairs_delete_then_insert() {
        // "foo bar" → "foo baz": one paired Delete/Insert. The delete side
        // covers byte 6 ("r"), the insert side covers byte 6 ("z"); equal
        // spans together cover bytes 0..6 ("foo ba") on both sides.
        let r = compute_with_intraline("foo bar\n", "foo baz\n", true);
        let lines = &r.hunks[0].lines;
        let del = lines.iter().find(|l| l.op == DiffOp::Delete).unwrap();
        let ins = lines.iter().find(|l| l.op == DiffOp::Insert).unwrap();
        let dspans = del.intraline_spans.as_ref().expect("del spans");
        let ispans = ins.intraline_spans.as_ref().expect("ins spans");
        assert_eq!(dspans, ispans, "paired lines carry the same span list");
        let equal_bytes_before: u32 = dspans
            .iter()
            .filter(|s| s.op == DiffOp::Equal)
            .map(|s| s.byte_end_before - s.byte_start_before)
            .sum();
        let equal_bytes_after: u32 = dspans
            .iter()
            .filter(|s| s.op == DiffOp::Equal)
            .map(|s| s.byte_end_after - s.byte_start_after)
            .sum();
        assert_eq!(equal_bytes_before, 6);
        assert_eq!(equal_bytes_after, 6);
        let del_spans: Vec<&IntralineSpan> =
            dspans.iter().filter(|s| s.op == DiffOp::Delete).collect();
        let ins_spans: Vec<&IntralineSpan> =
            dspans.iter().filter(|s| s.op == DiffOp::Insert).collect();
        assert_eq!(del_spans.len(), 1);
        assert_eq!(ins_spans.len(), 1);
        assert_eq!(del_spans[0].byte_start_before, 6);
        assert_eq!(del_spans[0].byte_end_before, 7);
        assert_eq!(ins_spans[0].byte_start_after, 6);
        assert_eq!(ins_spans[0].byte_end_after, 7);
    }

    #[test]
    fn intraline_off_leaves_spans_none() {
        let r = compute_with_intraline("a\n", "b\n", false);
        for line in &r.hunks[0].lines {
            assert!(line.intraline_spans.is_none());
        }
    }

    #[test]
    fn intraline_unpaired_lines_skipped() {
        // Pure insert (no matching Delete to pair with): no spans.
        let r = compute_with_intraline("a\nc\n", "a\nNEW\nc\n", true);
        for line in &r.hunks[0].lines {
            assert!(line.intraline_spans.is_none());
        }
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
