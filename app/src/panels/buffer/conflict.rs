//! Conflict detection for the inline agent-edit review overlay.
//!
//! An agent pending hunk is a *conflict* when the region it touches has also
//! been edited by the user in the `working` layer (per `op-log.md`'s "Merge
//! and conflicts" and `patch-review.md`'s "Conflicts"). Both sides are
//! expressed in `working` (editable-buffer) byte coordinates, so detection is
//! a range-overlap test; resolution ("Keep theirs") needs the matching
//! `accepted`-side text so the user's overlapping edit can be reverted before
//! the agent op lands.
//!
//! Everything here is a pure function over the two materialized texts
//! (`accepted`, `working`) and a hunk's byte range — no editor or op-log state
//! — so the detection and the "Keep theirs" extraction are unit-testable
//! headlessly.

use std::ops::Range;

use editor_core::diff::{lines, HunkKind};

/// One region the user changed in `working` relative to `accepted`, with both
/// the `working`-coordinate span (where the user's edit lives in the editable
/// buffer) and the `accepted`-coordinate span (the canonical text that span
/// replaced). "Keep theirs" reverts `working` over `working_range` back to the
/// `accepted` bytes named by `accepted_range`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEdit {
    pub working_range: Range<usize>,
    pub accepted_range: Range<usize>,
}

/// Two half-open byte ranges overlap when each starts before the other ends.
/// Touching-but-not-crossing ranges (`a.end == b.start`) do *not* overlap;
/// a conflict needs a genuinely shared span. Zero-width hunk ranges (pure
/// insertions, where `byte_start == byte_end`) never overlap anything, so a
/// bare agent insertion is treated as disjoint — matching the rule that only a
/// shared *region* is a conflict.
pub const fn ranges_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    // A zero-width range has no span to share, so it never overlaps.
    if a.start >= a.end || b.start >= b.end {
        return false;
    }
    a.start < b.end && b.start < a.end
}

/// Map a half-open line range onto a half-open byte range in `text`. A line's
/// byte offset is the sum of the byte lengths of all preceding lines
/// (line terminators included, since `str::lines` drops them but the diff's
/// line indices count physical lines). The end clamps to `text.len()`.
fn line_range_to_bytes(text: &str, line_range: &Range<usize>) -> Range<usize> {
    let mut offsets = Vec::with_capacity(line_range.end.saturating_sub(line_range.start) + 1);
    let mut acc = 0usize;
    offsets.push(0usize);
    for line in text.split_inclusive('\n') {
        acc += line.len();
        offsets.push(acc);
    }
    // `offsets[i]` is the byte offset of the start of physical line `i`;
    // `offsets.last()` is `text.len()`. Clamp both ends into range.
    let last = *offsets.last().unwrap_or(&0);
    let start = offsets.get(line_range.start).copied().unwrap_or(last);
    let end = offsets.get(line_range.end).copied().unwrap_or(last);
    start..end.max(start)
}

/// The regions the user has edited in `working` relative to `accepted`, each
/// as a [`UserEdit`] carrying both the `working`-side and `accepted`-side byte
/// spans. Computed from `diff(accepted, working)`: every non-context hunk is a
/// user edit (the buffer's `working` layer is authored only by the user, so
/// any divergence from `accepted` is theirs).
pub fn user_edit_ranges(accepted: &str, working: &str) -> Vec<UserEdit> {
    lines(accepted, working)
        .into_iter()
        .filter(|h| !matches!(h.kind, HunkKind::Context))
        .map(|h| UserEdit {
            working_range: line_range_to_bytes(working, &h.right_lines),
            accepted_range: line_range_to_bytes(accepted, &h.left_lines),
        })
        .collect()
}

/// Whether an agent hunk (named by its `working`-coordinate byte range)
/// overlaps any user edit — i.e. the user and the agent both changed the same
/// region. Disjoint hunks (auto-merged) return `false` and keep their plain
/// Accept/Reject affordance.
pub fn hunk_overlaps_user_edit(hunk_working: &Range<usize>, user_edits: &[UserEdit]) -> bool {
    user_edits
        .iter()
        .any(|e| ranges_overlap(hunk_working, &e.working_range))
}

/// Compute the `apply_working_edit` arguments that "Keep theirs" needs to
/// revert the user's conflicting edit back to `accepted` before the agent op
/// is accepted. Returns `(working_start, working_len, replacement_text)` for
/// the user edit overlapping `hunk_working`: the editable-buffer span to
/// overwrite and the `accepted` bytes to write there. Slicing respects UTF-8
/// boundaries (the `accepted_range` came from line offsets, which always land
/// on char boundaries; the guard returns `None` rather than panicking if a
/// non-boundary ever slips through). `None` when no user edit overlaps.
pub fn keep_theirs_edit(
    accepted: &str,
    hunk_working: &Range<usize>,
    user_edits: &[UserEdit],
) -> Option<(usize, usize, String)> {
    let edit = user_edits
        .iter()
        .find(|e| ranges_overlap(hunk_working, &e.working_range))?;
    let a = edit.accepted_range.start.min(accepted.len());
    let b = edit.accepted_range.end.min(accepted.len()).max(a);
    let replacement = accepted.get(a..b)?.to_string();
    let working_len = edit.working_range.end.saturating_sub(edit.working_range.start);
    Some((edit.working_range.start, working_len, replacement))
}

#[cfg(test)]
mod tests {
    use super::{
        hunk_overlaps_user_edit, keep_theirs_edit, ranges_overlap, user_edit_ranges, UserEdit,
    };

    #[test]
    fn overlap_basics() {
        assert!(ranges_overlap(&(0..5), &(3..8)));
        assert!(ranges_overlap(&(3..8), &(0..5)));
        assert!(!ranges_overlap(&(0..5), &(5..8)), "touching is not overlap");
        assert!(!ranges_overlap(&(0..5), &(6..8)), "disjoint");
        assert!(!ranges_overlap(&(5..5), &(0..10)), "zero-width never overlaps");
    }

    #[test]
    fn disjoint_hunk_is_not_a_conflict() {
        // User edited line 0; agent hunk sits on line 2 — different regions.
        let accepted = "alpha\nbravo\ncharlie\n";
        let working = "ALPHA\nbravo\ncharlie\n";
        let edits = user_edit_ranges(accepted, working);
        // The agent hunk's working range is line 2 ("charlie").
        let hunk = 12..20;
        assert!(!hunk_overlaps_user_edit(&hunk, &edits));
    }

    #[test]
    fn overlapping_hunk_is_a_conflict() {
        // User and agent both touch line 0.
        let accepted = "alpha\nbravo\n";
        let working = "ALPHA\nbravo\n";
        let edits = user_edit_ranges(accepted, working);
        let hunk = 0..6; // line 0 in working coords
        assert!(hunk_overlaps_user_edit(&hunk, &edits));
    }

    #[test]
    fn keep_theirs_recovers_accepted_text_for_the_overlap() {
        // accepted line 0 = "alpha\n"; user changed it to "ALPHA\n" in working.
        // Keep theirs must overwrite the working span [0,6) with the accepted
        // bytes "alpha\n" so the agent op lands against canonical text.
        let accepted = "alpha\nbravo\n";
        let working = "ALPHA\nbravo\n";
        let edits = user_edit_ranges(accepted, working);
        let hunk = 0..6;
        let (start, len, text) = keep_theirs_edit(accepted, &hunk, &edits)
            .expect("overlap should yield a keep-theirs edit");
        assert_eq!(start, 0);
        assert_eq!(len, 6, "working span length of the user's edited line");
        assert_eq!(text, "alpha\n", "accepted bytes for that region");
    }

    #[test]
    fn keep_theirs_none_when_disjoint() {
        let edits = vec![UserEdit { working_range: 0..6, accepted_range: 0..6 }];
        // Hunk well past the user edit.
        assert!(keep_theirs_edit("alpha\n", &(20..24), &edits).is_none());
    }
}
