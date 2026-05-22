//! Line + intraline diff. Returns structured hunks suitable for converting
//! into decorations (gutter markers, line backgrounds, intraline highlights)
//! or laying out side-by-side.

use similar::{ChangeTag, TextDiff};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HunkKind {
    Context,
    Added,
    Removed,
    Modified,
}

#[derive(Clone, Debug)]
pub struct Hunk {
    pub left_lines: std::ops::Range<usize>,
    pub right_lines: std::ops::Range<usize>,
    pub kind: HunkKind,
    /// Word-level intraline diffs over the joined hunk text. Pairs of
    /// (left_byte_range, right_byte_range) within their respective sides.
    pub intraline: Vec<(std::ops::Range<usize>, std::ops::Range<usize>)>,
}

/// One line-pair within a Modified hunk after refinement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinePair {
    /// Both sides present — render as intraline diff (right with char marks).
    Modified { left_offset: u32, right_offset: u32 },
    /// Only on the left — show as removed text inline/in block.
    Removed { left_offset: u32 },
    /// Only on the right — show with full-line added bg.
    Added { right_offset: u32 },
}

/// Char-level intraline diff result for one Modified pair.
#[derive(Clone, Debug, Default)]
pub struct Char {
    /// Char-level insertions: byte ranges into the *right* line.
    pub inserts: Vec<std::ops::Range<usize>>,
    /// Char-level deletions: byte ranges into the *left* line.
    pub deletes: Vec<std::ops::Range<usize>>,
    /// Similarity ratio 0..1 over chars (1 == identical).
    pub similarity: f32,
}

/// Pair up the lines of a Modified hunk by greedy similarity matching.
/// Lines that match strongly (>= `pair_threshold`) become a `Modified` pair;
/// otherwise unmatched left lines become `Removed` and unmatched right lines
/// become `Added`.
pub fn refine_modified_hunk(
    left_lines: &[&str],
    right_lines: &[&str],
    pair_threshold: f32,
) -> Vec<LinePair> {
    let mut out = Vec::new();
    let mut li = 0usize;
    let mut ri = 0usize;
    while li < left_lines.len() || ri < right_lines.len() {
        if li >= left_lines.len() {
            out.push(LinePair::Added { right_offset: ri as u32 });
            ri += 1;
            continue;
        }
        if ri >= right_lines.len() {
            out.push(LinePair::Removed { left_offset: li as u32 });
            li += 1;
            continue;
        }
        let here = similarity(left_lines[li], right_lines[ri]);
        if here >= pair_threshold {
            out.push(LinePair::Modified { left_offset: li as u32, right_offset: ri as u32 });
            li += 1;
            ri += 1;
            continue;
        }
        // Look one ahead on each side. Prefer the side whose current line
        // can find a partner sooner.
        let l_partner = first_match_index(left_lines[li], right_lines, ri, pair_threshold);
        let r_partner = first_match_index(right_lines[ri], left_lines, li, pair_threshold);
        match (l_partner, r_partner) {
            (Some(lp), Some(rp)) => {
                // Whichever side has the closer partner is the "kept" side;
                // the other side's current line is orphaned.
                if (lp - ri) <= (rp - li) {
                    out.push(LinePair::Added { right_offset: ri as u32 });
                    ri += 1;
                } else {
                    out.push(LinePair::Removed { left_offset: li as u32 });
                    li += 1;
                }
            }
            (Some(_), None) => {
                // Left line has a future partner on the right — emit Added now.
                out.push(LinePair::Added { right_offset: ri as u32 });
                ri += 1;
            }
            (None, Some(_)) => {
                // Right line has a future partner on the left — emit Removed now.
                out.push(LinePair::Removed { left_offset: li as u32 });
                li += 1;
            }
            (None, None) => {
                // No future matches either way — emit both as orphans, advance both.
                out.push(LinePair::Removed { left_offset: li as u32 });
                out.push(LinePair::Added { right_offset: ri as u32 });
                li += 1;
                ri += 1;
            }
        }
    }
    out
}

fn similarity(a: &str, b: &str) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    TextDiff::from_chars(a, b).ratio()
}

fn first_match_index(needle: &str, hay: &[&str], from: usize, threshold: f32) -> Option<usize> {
    for (i, line) in hay.iter().enumerate().skip(from) {
        if similarity(needle, line) >= threshold {
            return Some(i);
        }
    }
    None
}

/// Compute a char-level diff between two single lines, returning the byte
/// ranges of inserts (into `right`) and deletes (into `left`), plus the
/// similarity ratio.
pub fn chars(left: &str, right: &str) -> Char {
    let diff = TextDiff::from_chars(left, right);
    let mut inserts: Vec<std::ops::Range<usize>> = Vec::new();
    let mut deletes: Vec<std::ops::Range<usize>> = Vec::new();
    let (mut li, mut ri) = (0usize, 0usize);
    let mut cur_ins: Option<std::ops::Range<usize>> = None;
    let mut cur_del: Option<std::ops::Range<usize>> = None;
    for change in diff.iter_all_changes() {
        let ch = change.value();
        let n = ch.len();
        match change.tag() {
            ChangeTag::Equal => {
                if let Some(r) = cur_ins.take() { inserts.push(r); }
                if let Some(r) = cur_del.take() { deletes.push(r); }
                li += n;
                ri += n;
            }
            ChangeTag::Insert => {
                let r = cur_ins.get_or_insert(ri..ri);
                r.end = ri + n;
                ri += n;
            }
            ChangeTag::Delete => {
                let r = cur_del.get_or_insert(li..li);
                r.end = li + n;
                li += n;
            }
        }
    }
    if let Some(r) = cur_ins { inserts.push(r); }
    if let Some(r) = cur_del { deletes.push(r); }
    Char { inserts, deletes, similarity: diff.ratio() }
}

pub fn lines(left: &str, right: &str) -> Vec<Hunk> {
    let diff = TextDiff::from_lines(left, right);
    let mut hunks: Vec<Hunk> = Vec::new();
    let (mut li, mut ri) = (0usize, 0usize);
    let mut pending: Option<Hunk> = None;
    let mut pending_left_text = String::new();
    let mut pending_right_text = String::new();

    let flush = |hunks: &mut Vec<Hunk>,
                 pending: &mut Option<Hunk>,
                 pending_left_text: &mut String,
                 pending_right_text: &mut String| {
        if let Some(mut h) = pending.take() {
            if h.kind == HunkKind::Modified {
                let wdiff = TextDiff::from_words(
                    pending_left_text.as_str(),
                    pending_right_text.as_str(),
                );
                let mut out: Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> = Vec::new();
                let (mut wli, mut wri) = (0usize, 0usize);
                let mut left_run: Option<std::ops::Range<usize>> = None;
                let mut right_run: Option<std::ops::Range<usize>> = None;
                let emit =
                    |out: &mut Vec<_>,
                     left_run: &mut Option<std::ops::Range<usize>>,
                     right_run: &mut Option<std::ops::Range<usize>>| {
                        if let (Some(l), Some(r)) = (left_run.clone(), right_run.clone()) {
                            out.push((l, r));
                            *left_run = None;
                            *right_run = None;
                        }
                    };
                for change in wdiff.iter_all_changes() {
                    let word = change.value();
                    let wlen = word.len();
                    match change.tag() {
                        ChangeTag::Equal => {
                            emit(&mut out, &mut left_run, &mut right_run);
                            wli += wlen;
                            wri += wlen;
                        }
                        ChangeTag::Delete => {
                            let r = left_run.get_or_insert(wli..wli);
                            r.end = wli + wlen;
                            if right_run.is_none() {
                                right_run = Some(wri..wri);
                            }
                            wli += wlen;
                        }
                        ChangeTag::Insert => {
                            let r = right_run.get_or_insert(wri..wri);
                            r.end = wri + wlen;
                            if left_run.is_none() {
                                left_run = Some(wli..wli);
                            }
                            wri += wlen;
                        }
                    }
                }
                emit(&mut out, &mut left_run, &mut right_run);
                h.intraline = out;
            }
            hunks.push(h);
            pending_left_text.clear();
            pending_right_text.clear();
        }
    };

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {
                flush(&mut hunks, &mut pending, &mut pending_left_text, &mut pending_right_text);
                hunks.push(Hunk {
                    left_lines: li..li + 1,
                    right_lines: ri..ri + 1,
                    kind: HunkKind::Context,
                    intraline: Vec::new(),
                });
                li += 1;
                ri += 1;
            }
            ChangeTag::Delete => {
                let h = pending.get_or_insert(Hunk {
                    left_lines: li..li,
                    right_lines: ri..ri,
                    kind: HunkKind::Removed,
                    intraline: Vec::new(),
                });
                if h.kind == HunkKind::Added {
                    h.kind = HunkKind::Modified;
                }
                h.left_lines.end = li + 1;
                pending_left_text.push_str(change.value());
                li += 1;
            }
            ChangeTag::Insert => {
                let h = pending.get_or_insert(Hunk {
                    left_lines: li..li,
                    right_lines: ri..ri,
                    kind: HunkKind::Added,
                    intraline: Vec::new(),
                });
                if h.kind == HunkKind::Removed {
                    h.kind = HunkKind::Modified;
                }
                h.right_lines.end = ri + 1;
                pending_right_text.push_str(change.value());
                ri += 1;
            }
        }
    }
    flush(&mut hunks, &mut pending, &mut pending_left_text, &mut pending_right_text);
    // Merge adjacent Context hunks so callers see one entry per unchanged run.
    let mut merged: Vec<Hunk> = Vec::with_capacity(hunks.len());
    for h in hunks {
        if let Some(last) = merged.last_mut() {
            if last.kind == HunkKind::Context
                && h.kind == HunkKind::Context
                && last.left_lines.end == h.left_lines.start
                && last.right_lines.end == h.right_lines.start
            {
                last.left_lines.end = h.left_lines.end;
                last.right_lines.end = h.right_lines.end;
                continue;
            }
        }
        merged.push(h);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_lines_only_context() {
        let h = lines("a\nb\nc\n", "a\nb\nc\n");
        assert!(h.iter().all(|h| h.kind == HunkKind::Context));
        // After merging, all-equal collapses to a single Context hunk.
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].left_lines, 0..3);
    }

    #[test]
    fn pure_add() {
        let h = lines("a\nb\n", "a\nb\nc\n");
        assert!(h.iter().any(|h| h.kind == HunkKind::Added));
    }

    #[test]
    fn pure_remove() {
        let h = lines("a\nb\nc\n", "a\nb\n");
        assert!(h.iter().any(|h| h.kind == HunkKind::Removed));
    }

    #[test]
    fn modified_with_intraline() {
        let h = lines("hello world\n", "hello rust\n");
        let m = h.iter().find(|h| h.kind == HunkKind::Modified).unwrap();
        assert!(!m.intraline.is_empty());
    }

    #[test]
    fn refine_pairs_similar_lines() {
        let left = vec!["fn greet(name: &str) {", "    println!(\"hi\");"];
        let right = vec!["fn greet(name: &str) -> String {", "    format!(\"hi\")"];
        let pairs = refine_modified_hunk(&left, &right, 0.5);
        // Both lines should pair up (similar prefixes / shared substrings).
        let modified = pairs.iter().filter(|p| matches!(p, LinePair::Modified { .. })).count();
        assert_eq!(modified, 2, "both lines should pair: {pairs:?}");
    }

    #[test]
    fn refine_suppresses_redundant_removal() {
        // Left line is contained in the right line — should pair, not orphan.
        let left = vec!["    greet(\"world\");"];
        let right = vec!["    let g = greet(\"world\");", "    println!(\"{g}\");"];
        let pairs = refine_modified_hunk(&left, &right, 0.5);
        let removed = pairs.iter().filter(|p| matches!(p, LinePair::Removed { .. })).count();
        let added = pairs.iter().filter(|p| matches!(p, LinePair::Added { .. })).count();
        let modified = pairs.iter().filter(|p| matches!(p, LinePair::Modified { .. })).count();
        // Should produce one Modified pair (left ↔ right[0]) plus one Added (right[1]).
        assert_eq!(modified, 1, "left should pair with right[0]: {pairs:?}");
        assert_eq!(added, 1);
        assert_eq!(removed, 0, "no orphaned removals: {pairs:?}");
    }

    #[test]
    fn refine_keeps_orphans_when_no_match() {
        let left = vec!["zzz unique 1", "zzz unique 2"];
        let right = vec!["totally different content here"];
        let pairs = refine_modified_hunk(&left, &right, 0.5);
        let removed = pairs.iter().filter(|p| matches!(p, LinePair::Removed { .. })).count();
        assert!(removed >= 1, "non-matching left lines should be Removed: {pairs:?}");
    }

    #[test]
    fn char_diff_inserts() {
        let cd = chars("hello", "hello world");
        let total_insert: usize = cd.inserts.iter().map(|r| r.end - r.start).sum();
        assert_eq!(total_insert, 6); // " world"
    }
}
