//! Pure conflict-marker parsing + resolution for the in-editor VS Code–style
//! git-conflict resolver (`docs/git.md` [[spec:git-conflict-inline-markers]]).
//!
//! A `git merge` conflict writes standard markers into the `.md`. With
//! `merge.conflictStyle = zdiff3` a region looks like:
//!
//! ```text
//! <<<<<<< ours
//! our text
//! ||||||| base
//! base text
//! =======
//! their text
//! >>>>>>> theirs
//! ```
//!
//! and in the classic style (no base) like:
//!
//! ```text
//! <<<<<<< ours
//! our text
//! =======
//! their text
//! >>>>>>> theirs
//! ```
//!
//! Everything here is a pure function over `&str` with no egui / editor / git
//! dependency, so the parsing and the per-region resolution are unit-testable
//! headlessly. The buffer panel ([`super::mod`]) reads these to decorate each
//! region and to rewrite a single region on an Accept-* click; the editor binding
//! mirrors that rewrite into the `working` layer like any other edit.
//!
//! These markers are the *git transport's* conflict surface — distinct from the
//! structured layered-doc conflict surface in `hiker_core::merge`. This module
//! is git-specific because it must understand the zdiff3 `|||||||` base section,
//! which the unified surface never emits.
//!
//! status: git-conflict-inline-markers

use std::ops::Range;

/// The seven-character marker run that opens the "ours" (current) half.
const MARK_OURS: &str = "<<<<<<<";
/// The seven-character marker run that opens the zdiff3 "base" half.
const MARK_BASE: &str = "|||||||";
/// The seven-character marker run that separates "ours"/"base" from "theirs".
const MARK_SEP: &str = "=======";
/// The seven-character marker run that closes the "theirs" (incoming) half.
const MARK_THEIRS: &str = ">>>>>>>";

/// Which side of a conflict region an Accept-* button keeps.
///
/// Mirrors VS Code's verbs: `Current` keeps our text, `Incoming` keeps theirs,
/// `Both` keeps ours followed by theirs (the base section is always dropped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Accept Current — keep the "ours" half, drop base + theirs.
    Current,
    /// Accept Incoming — keep the "theirs" half, drop ours + base.
    Incoming,
    /// Accept Both — keep "ours" then "theirs", drop base.
    Both,
}

/// One parsed conflict region, carrying byte ranges into the source text.
///
/// `region` spans the whole block from the first byte of the `<<<<<<<` line
/// through the newline that ends the `>>>>>>>` line (or to end-of-text when the
/// close marker is the final line with no trailing newline). The content ranges
/// (`ours`, `base`, `theirs`) name the bytes *between* the marker lines — each is
/// empty (`start == end`) when that side contributed no text. `base` is `None`
/// in the classic (non-zdiff3) style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRegion {
    /// The whole region, marker lines included.
    pub region: Range<usize>,
    /// The `ours` / current content between `<<<<<<<` and `|||||||`/`=======`.
    pub ours: Range<usize>,
    /// The zdiff3 `base` content between `|||||||` and `=======`; `None` in the
    /// classic style.
    pub base: Option<Range<usize>>,
    /// The `theirs` / incoming content between `=======` and `>>>>>>>`.
    pub theirs: Range<usize>,
}

/// Fast check for whether `text` carries at least one complete conflict region.
///
/// True iff some line starts with the `<<<<<<<` open marker and a later line
/// starts with the matching `>>>>>>>` close marker — the same shape
/// [`parse_conflicts`] recovers, so this is `true` exactly when that returns a
/// non-empty vec. Used to gate the source-not-live-preview rendering and to
/// decide whether to run the (more expensive) full parse.
#[must_use]
pub fn has_conflict_markers(text: &str) -> bool {
    let mut saw_open = false;
    for line in text.lines() {
        if line.starts_with(MARK_OURS) {
            saw_open = true;
        } else if saw_open && line.starts_with(MARK_THEIRS) {
            return true;
        }
    }
    false
}

/// A line scanned from the source, with its byte offsets.
///
/// `start`/`end` are the byte range of the line *content* (the terminator is not
/// included); `next` is the offset of the first byte of the following line (==
/// `text.len()` for the last line), i.e. content + any `\n`. Marker detection
/// keys off the seven-char run at `start`.
struct Line {
    start: usize,
    end: usize,
    next: usize,
}

/// Split `text` into [`Line`]s carrying byte offsets. `\n`-terminated; a final
/// line without a trailing newline is still emitted.
fn scan_lines(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            // Trim a trailing `\r` from the content range so CRLF files parse.
            let end = if i > start && text.as_bytes()[i - 1] == b'\r' { i - 1 } else { i };
            out.push(Line { start, end, next: i + 1 });
            start = i + 1;
        }
    }
    if start < text.len() {
        out.push(Line { start, end: text.len(), next: text.len() });
    }
    out
}

/// Parse every complete conflict region in `text`, in document order.
///
/// Handles both zdiff3 (with a `|||||||` base section) and classic (no base)
/// regions, and multiple regions in one document. Tolerant of garbage: a `<<<<<<<`
/// open with no following `=======`/`>>>>>>>` before the next open (or before
/// end-of-text) is skipped rather than panicking, so a half-typed or nested
/// marker never eats content. The scan resumes after the close of each
/// well-formed region (or after the open line of a malformed one).
#[must_use]
pub fn parse_conflicts(text: &str) -> Vec<ConflictRegion> {
    let lines = scan_lines(text);
    let mut regions = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        if !line_is(text, &lines[i], MARK_OURS) {
            i += 1;
            continue;
        }
        match parse_one(text, &lines, i) {
            Some((region, next_i)) => {
                regions.push(region);
                i = next_i;
            }
            // Malformed open: skip just this line and keep scanning, so a later
            // well-formed region is still recovered.
            None => i += 1,
        }
    }
    regions
}

/// Whether `line`'s content starts with marker run `mark`.
fn line_is(text: &str, line: &Line, mark: &str) -> bool {
    text[line.start..line.end].starts_with(mark)
}

/// Parse a single region whose open `<<<<<<<` marker is at `lines[open]`.
///
/// Returns the region plus the index of the first line *after* the close marker
/// (where scanning resumes), or `None` if the block is malformed (no `=======`
/// or no `>>>>>>>` before the next open / end-of-text).
fn parse_one(
    text: &str,
    lines: &[Line],
    open: usize,
) -> Option<(ConflictRegion, usize)> {
    // Content between markers runs from the byte after the open line's terminator
    // up to the start of the next marker line. An empty side has start == end.
    let ours_start = lines[open].next;
    let mut idx = open + 1;
    let mut base_marker: Option<usize> = None;
    let mut sep_marker: Option<usize> = None;
    let mut close_marker: Option<usize> = None;

    while idx < lines.len() {
        let l = &lines[idx];
        if line_is(text, l, MARK_OURS) {
            // A new region opened before this one closed → this one is malformed.
            break;
        } else if base_marker.is_none() && sep_marker.is_none() && line_is(text, l, MARK_BASE) {
            base_marker = Some(idx);
        } else if sep_marker.is_none() && line_is(text, l, MARK_SEP) {
            sep_marker = Some(idx);
        } else if sep_marker.is_some() && line_is(text, l, MARK_THEIRS) {
            close_marker = Some(idx);
            break;
        }
        idx += 1;
    }

    let sep = sep_marker?;
    let close = close_marker?;
    Some((
        build_region(lines, open, base_marker, sep, close, ours_start),
        close + 1,
    ))
}

/// Assemble a [`ConflictRegion`] from the resolved marker-line indices.
fn build_region(
    lines: &[Line],
    open: usize,
    base_marker: Option<usize>,
    sep: usize,
    close: usize,
    ours_start: usize,
) -> ConflictRegion {
    // `ours` ends at the base marker if present, else at the separator.
    let ours_end = base_marker.map_or(lines[sep].start, |b| lines[b].start);
    let base = base_marker.map(|b| lines[b].next..lines[sep].start);
    let theirs = lines[sep].next..lines[close].start;
    ConflictRegion {
        region: lines[open].start..lines[close].next,
        ours: ours_start..ours_end,
        base,
        theirs,
    }
}

/// The replacement text a `choice` substitutes for `region`'s whole-region byte
/// range (markers removed): the kept side(s) of the conflict. `Both` emits ours
/// then theirs (base always dropped). This is exactly what [`resolve_region`]
/// splices in place of `region.region`.
fn resolution_text(text: &str, region: &ConflictRegion, choice: Choice) -> String {
    let ours = slice(text, &region.ours);
    let theirs = slice(text, &region.theirs);
    match choice {
        Choice::Current => ours.to_string(),
        Choice::Incoming => theirs.to_string(),
        Choice::Both => join_both(ours, theirs),
    }
}

/// Return a copy of `text` with the single `region` replaced by the content for
/// `choice`, all markers removed.
///
/// `Both` emits ours then theirs; the base section is always dropped. The
/// replacement preserves the trailing newline shape: when the region's bytes end
/// in a newline (the common case — the `>>>>>>>` line is `\n`-terminated), the
/// kept content keeps its own trailing newline so the following line stays on its
/// own row; an empty kept side collapses to nothing, splicing the surrounding
/// text together. Other regions in `text` are left untouched (resolve them with
/// their own call), so an out-of-range region is returned as `text` unchanged.
#[must_use]
pub fn resolve_region(text: &str, region: &ConflictRegion, choice: Choice) -> String {
    let r = &region.region;
    if r.start > text.len() || r.end > text.len() || r.start > r.end {
        return text.to_string();
    }
    let kept = resolution_text(text, region, choice);
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..r.start]);
    out.push_str(&kept);
    out.push_str(&text[r.end..]);
    out
}

/// Slice `text` by `range`, clamped to char boundaries / length so a stale range
/// never panics.
fn slice<'a>(text: &'a str, range: &Range<usize>) -> &'a str {
    let s = range.start.min(text.len());
    let e = range.end.min(text.len()).max(s);
    if text.is_char_boundary(s) && text.is_char_boundary(e) {
        &text[s..e]
    } else {
        ""
    }
}

/// Concatenate the two kept sides for `Choice::Both`, ours first then theirs,
/// inserting a separating newline only when ours has content that doesn't
/// already end in one (so two non-empty halves never run together on one line,
/// and an empty half doesn't introduce a blank line).
fn join_both(ours: &str, theirs: &str) -> String {
    if ours.is_empty() {
        return theirs.to_string();
    }
    if theirs.is_empty() {
        return ours.to_string();
    }
    let mut out = String::with_capacity(ours.len() + theirs.len() + 1);
    out.push_str(ours);
    if !ours.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(theirs);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zdiff3() -> &'static str {
        "before\n\
<<<<<<< ours\n\
our text\n\
||||||| base\n\
base text\n\
=======\n\
their text\n\
>>>>>>> theirs\n\
after\n"
    }

    fn classic() -> &'static str {
        "before\n\
<<<<<<< ours\n\
our text\n\
=======\n\
their text\n\
>>>>>>> theirs\n\
after\n"
    }

    #[test]
    fn has_markers_true_and_false() {
        assert!(has_conflict_markers(zdiff3()));
        assert!(has_conflict_markers(classic()));
        assert!(!has_conflict_markers("just some\nplain text\n"));
        // An open with no close is not a complete region.
        assert!(!has_conflict_markers("<<<<<<< ours\nour text\n=======\n"));
    }

    #[test]
    fn parses_zdiff3_region() {
        let text = zdiff3();
        let regions = parse_conflicts(text);
        assert_eq!(regions.len(), 1);
        let c = &regions[0];
        assert_eq!(&text[c.ours.clone()], "our text\n");
        assert_eq!(&text[c.base.clone().unwrap()], "base text\n");
        assert_eq!(&text[c.theirs.clone()], "their text\n");
        // The whole region runs from the `<<<` line through the `>>>` line's nl.
        assert!(text[c.region.clone()].starts_with("<<<<<<< ours\n"));
        assert!(text[c.region.clone()].ends_with(">>>>>>> theirs\n"));
        // `after` is outside the region.
        assert!(!text[c.region.clone()].contains("after"));
    }

    #[test]
    fn parses_classic_region_without_base() {
        let text = classic();
        let regions = parse_conflicts(text);
        assert_eq!(regions.len(), 1);
        let c = &regions[0];
        assert_eq!(c.base, None);
        assert_eq!(&text[c.ours.clone()], "our text\n");
        assert_eq!(&text[c.theirs.clone()], "their text\n");
    }

    #[test]
    fn parses_multiple_regions() {
        let text = "a\n\
<<<<<<< ours\n\
one\n\
=======\n\
two\n\
>>>>>>> theirs\n\
mid\n\
<<<<<<< ours\n\
three\n\
||||||| base\n\
b\n\
=======\n\
four\n\
>>>>>>> theirs\n\
z\n";
        let regions = parse_conflicts(text);
        assert_eq!(regions.len(), 2);
        assert_eq!(&text[regions[0].ours.clone()], "one\n");
        assert_eq!(&text[regions[0].theirs.clone()], "two\n");
        assert_eq!(regions[0].base, None);
        assert_eq!(&text[regions[1].ours.clone()], "three\n");
        assert_eq!(&text[regions[1].base.clone().unwrap()], "b\n");
        assert_eq!(&text[regions[1].theirs.clone()], "four\n");
    }

    #[test]
    fn resolve_current_keeps_ours() {
        let text = zdiff3();
        let region = &parse_conflicts(text)[0];
        let out = resolve_region(text, region, Choice::Current);
        assert_eq!(out, "before\nour text\nafter\n");
        assert!(!has_conflict_markers(&out));
    }

    #[test]
    fn resolve_incoming_keeps_theirs() {
        let text = zdiff3();
        let region = &parse_conflicts(text)[0];
        let out = resolve_region(text, region, Choice::Incoming);
        assert_eq!(out, "before\ntheir text\nafter\n");
        assert!(!has_conflict_markers(&out));
    }

    #[test]
    fn resolve_both_keeps_ours_then_theirs() {
        let text = classic();
        let region = &parse_conflicts(text)[0];
        let out = resolve_region(text, region, Choice::Both);
        assert_eq!(out, "before\nour text\ntheir text\nafter\n");
        assert!(!has_conflict_markers(&out));
    }

    #[test]
    fn resolve_one_of_many_leaves_the_rest() {
        let text = "a\n\
<<<<<<< ours\n\
one\n\
=======\n\
two\n\
>>>>>>> theirs\n\
mid\n\
<<<<<<< ours\n\
three\n\
=======\n\
four\n\
>>>>>>> theirs\n\
z\n";
        let regions = parse_conflicts(text);
        // Resolve the SECOND region; the first must remain markered.
        let out = resolve_region(text, &regions[1], Choice::Current);
        assert!(out.contains("<<<<<<< ours\none\n"));
        assert!(out.contains("\nthree\nz\n"));
        assert!(!out.contains("four"));
    }

    #[test]
    fn resolve_both_with_empty_ours_drops_blank_line() {
        // A delete-vs-edit region: our side contributed nothing.
        let text = "x\n\
<<<<<<< ours\n\
=======\n\
their text\n\
>>>>>>> theirs\n\
y\n";
        let region = &parse_conflicts(text)[0];
        assert_eq!(&text[region.ours.clone()], "");
        let out = resolve_region(text, region, Choice::Both);
        assert_eq!(out, "x\ntheir text\ny\n");
    }

    #[test]
    fn malformed_unterminated_region_is_skipped() {
        // Open + sep but no close before EOF → not a complete region.
        let text = "p\n<<<<<<< ours\nour\n=======\ntheir\nq\n";
        assert!(parse_conflicts(text).is_empty());
        assert!(!has_conflict_markers(text));
    }

    #[test]
    fn malformed_region_does_not_swallow_following_good_one() {
        // First open never closes (a second open appears first); the second
        // region is well-formed and must still be recovered.
        let text = "<<<<<<< ours\ndangling\n\
<<<<<<< ours\n\
good ours\n\
=======\n\
good theirs\n\
>>>>>>> theirs\n";
        let regions = parse_conflicts(text);
        assert_eq!(regions.len(), 1);
        assert_eq!(&text[regions[0].ours.clone()], "good ours\n");
    }

    #[test]
    fn open_with_no_close_at_eof_is_tolerated() {
        let text = "<<<<<<< ours\nonly ours\n";
        assert!(parse_conflicts(text).is_empty());
    }

    #[test]
    fn resolve_region_with_stale_range_returns_unchanged() {
        let text = "hello\n";
        let bogus = ConflictRegion {
            region: 100..200,
            ours: 100..150,
            base: None,
            theirs: 150..200,
        };
        assert_eq!(resolve_region(text, &bogus, Choice::Current), text);
    }
}
