//! Turn hunks into decorations. No context-folding (the host can layer folds
//! on top via its own decoration provider if desired).
//!
//! Two output shapes:
//!   - `alignment_decorations(left, right, hunks, line_height)` →
//!     two `DecorationSet`s. Each side gets line-bg + word marks for its
//!     changes plus a hatched `Block` on the opposite-edit side, so a hunk
//!     where one side has more lines still aligns row-for-row across panes.
//!   - `unified_decorations(right, left_text, hunks, line_height)` →
//!     a single `DecorationSet` for the right (modified) rope, with removed
//!     lines injected as `Block(Text)` above each modified/removed hunk and
//!     intraline word marks on the added side.

use editor_core::diff::chars as char_diff;
use editor_core::diff::refine_modified_hunk;

use editor_core::diff::Hunk;

use editor_core::diff::HunkKind;

use editor_core::diff::LinePair;
use editor_core::decoration::BlockDeco;
use editor_core::decoration::BlockKind;
use editor_core::decoration::BlockSide;
use editor_core::decoration::BlockTextLine;
use editor_core::decoration::Color;
use editor_core::decoration::Decoration;
use editor_core::decoration::Set as DecorationSet;
use editor_core::decoration::GutterMarker;
use editor_core::decoration::LineStyle;
use editor_core::decoration::MarkStyle;
use editor_core::rangeset::RangeSet;
use editor_core::rope::Rope;
use editor_core::theme::Theme;
use smol_str::SmolStr;

pub const BG_ADDED: Color = Color::rgba(46, 160, 67, 38);
pub const BG_REMOVED: Color = Color::rgba(248, 81, 73, 38);
pub const BG_WORD_ADDED: Color = Color::rgba(46, 160, 67, 110);
pub const BG_WORD_REMOVED: Color = Color::rgba(248, 81, 73, 110);
pub const HATCH_COLOR: Color = Color::rgba(140, 140, 160, 70);

#[derive(Clone, Copy)]
struct DiffPalette {
    added_bg: Color,
    removed_bg: Color,
    word_added: Color,
    word_removed: Color,
    hatched: Color,
}

impl DiffPalette {
    const fn from_theme(theme: Option<&Theme>) -> Self {
        match theme {
            None => Self {
                added_bg: BG_ADDED,
                removed_bg: BG_REMOVED,
                word_added: BG_WORD_ADDED,
                word_removed: BG_WORD_REMOVED,
                hatched: HATCH_COLOR,
            },
            Some(t) => Self {
                added_bg: t.diff.added_bg,
                removed_bg: t.diff.removed_bg,
                word_added: t.diff.word_added,
                word_removed: t.diff.word_removed,
                hatched: t.diff.hatched,
            },
        }
    }
}

pub fn alignment_decorations(
    left_rope: &Rope,
    right_rope: &Rope,
    hunks: &[Hunk],
    line_height: f32,
    theme: Option<&Theme>,
) -> (DecorationSet, DecorationSet) {
    let pal = DiffPalette::from_theme(theme);
    let mut left_entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();
    let mut right_entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();

    for hunk in hunks {
        match hunk.kind {
            HunkKind::Context => {}
            HunkKind::Removed => {
                line_bg(
                    &mut left_entries,
                    left_rope,
                    &hunk.left_lines,
                    pal.removed_bg,
                    &GutterMarker::DiffRemoved,
                );
                let height = hunk.left_lines.len() as f32 * line_height;
                if height > 0.0 {
                    push_block_after_hunk(
                        &mut right_entries,
                        right_rope,
                        hunk.right_lines.start,
                        height,
                        BlockKind::Hatched(pal.hatched),
                    );
                }
            }
            HunkKind::Added => {
                line_bg(
                    &mut right_entries,
                    right_rope,
                    &hunk.right_lines,
                    pal.added_bg,
                    &GutterMarker::DiffAdded,
                );
                let height = hunk.right_lines.len() as f32 * line_height;
                if height > 0.0 {
                    push_block_after_hunk(
                        &mut left_entries,
                        left_rope,
                        hunk.left_lines.start,
                        height,
                        BlockKind::Hatched(pal.hatched),
                    );
                }
            }
            HunkKind::Modified => {
                line_bg(
                    &mut left_entries,
                    left_rope,
                    &hunk.left_lines,
                    pal.removed_bg,
                    &GutterMarker::DiffModified,
                );
                line_bg(
                    &mut right_entries,
                    right_rope,
                    &hunk.right_lines,
                    pal.added_bg,
                    &GutterMarker::DiffModified,
                );
                let left_start_byte = byte_of_line(left_rope, hunk.left_lines.start);
                let right_start_byte = byte_of_line(right_rope, hunk.right_lines.start);
                for (l, r) in &hunk.intraline {
                    let ls = left_start_byte + l.start;
                    let le = (left_start_byte + l.end).min(left_rope.len_bytes());
                    if ls < le {
                        left_entries.push((
                            ls..le,
                            Decoration::Mark(MarkStyle {
                                bg: Some(pal.word_removed),
                                ..MarkStyle::default()
                            }),
                        ));
                    }
                    let rs = right_start_byte + r.start;
                    let re = (right_start_byte + r.end).min(right_rope.len_bytes());
                    if rs < re {
                        right_entries.push((
                            rs..re,
                            Decoration::Mark(MarkStyle {
                                bg: Some(pal.word_added),
                                ..MarkStyle::default()
                            }),
                        ));
                    }
                }
                let lc = hunk.left_lines.len();
                let rc = hunk.right_lines.len();
                if lc < rc {
                    let extra = (rc - lc) as f32 * line_height;
                    push_block_after_hunk(
                        &mut left_entries,
                        left_rope,
                        hunk.left_lines.end,
                        extra,
                        BlockKind::Hatched(pal.hatched),
                    );
                } else if rc < lc {
                    let extra = (lc - rc) as f32 * line_height;
                    push_block_after_hunk(
                        &mut right_entries,
                        right_rope,
                        hunk.right_lines.end,
                        extra,
                        BlockKind::Hatched(pal.hatched),
                    );
                }
            }
        }
    }
    (
        RangeSet::from_iter(left_entries),
        RangeSet::from_iter(right_entries),
    )
}

pub fn unified_decorations(
    right_rope: &Rope,
    left_text: &str,
    hunks: &[Hunk],
    line_height: f32,
    theme: Option<&Theme>,
) -> DecorationSet {
    unified_decorations_opts(right_rope, left_text, hunks, line_height, theme, true)
}

/// Variant that gates the character-level intraline marks behind a flag,
/// per `view-intraline-diff-toggle`. When `intraline` is false, modified
/// hunks still get line-level red/green backgrounds but no per-character
/// emphasis.
pub fn unified_decorations_opts(
    right_rope: &Rope,
    left_text: &str,
    hunks: &[Hunk],
    line_height: f32,
    theme: Option<&Theme>,
    intraline: bool,
) -> DecorationSet {
    let pal = DiffPalette::from_theme(theme);
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();
    let left_lines_all: Vec<&str> = {
        let mut out = Vec::new();
        let mut start = 0;
        for (i, ch) in left_text.char_indices() {
            if ch == '\n' {
                out.push(&left_text[start..i]);
                start = i + 1;
            }
        }
        if start <= left_text.len() {
            out.push(&left_text[start..]);
        }
        if matches!(out.last(), Some(last) if last.is_empty()) {
            out.pop();
        }
        out
    };

    for hunk in hunks {
        match hunk.kind {
            HunkKind::Context => {}
            HunkKind::Added => {
                line_bg(
                    &mut entries,
                    right_rope,
                    &hunk.right_lines,
                    pal.added_bg,
                    &GutterMarker::DiffAdded,
                );
            }
            HunkKind::Removed => {
                let range = &hunk.left_lines;
                let mut lines = Vec::with_capacity(range.len());
                for li in range.clone() {
                    let text = left_lines_all.get(li).copied().unwrap_or("");
                    lines.push(BlockTextLine {
                        text: SmolStr::from(text),
                        bg: Some(pal.removed_bg),
                        fg: None,
                        gutter_marker: Some(GutterMarker::DiffRemoved),
                        marks: Vec::new(),
                        strikethrough: true,
                    });
                }
                let block = BlockDeco {
                    side: BlockSide::Above,
                    height: range.len() as f32 * line_height,
                    kind: BlockKind::Text { lines },
                };
                push_block_at(&mut entries, right_rope, hunk.right_lines.start, block);
            }
            HunkKind::Modified => {
                // Refine the hunk: line-by-line pair up by similarity. Only
                // truly orphaned left lines go into a removed block; paired
                // lines get char-level intraline marks instead of being shown
                // twice (once removed, once added).
                let left_hunk_owned: Vec<String> = (hunk.left_lines.start..hunk.left_lines.end)
                    .map(|i| left_lines_all.get(i).copied().unwrap_or("").to_string())
                    .collect();
                let right_hunk_owned: Vec<String> = (hunk.right_lines.start..hunk.right_lines.end)
                    .map(|i| {
                        if i < right_rope.len_lines() {
                            right_rope.line_str(i)
                        } else {
                            String::new()
                        }
                    })
                    .collect();
                let left_refs: Vec<&str> = left_hunk_owned.iter().map(std::string::String::as_str).collect();
                let right_refs: Vec<&str> = right_hunk_owned.iter().map(std::string::String::as_str).collect();
                let pairs = refine_modified_hunk(&left_refs, &right_refs, 0.5);

                let removed_block_indices: Vec<u32> = pairs
                    .iter()
                    .filter_map(|p| match p {
                        LinePair::Removed { left_offset } => Some(*left_offset),
                        _ => None,
                    })
                    .collect();
                if !removed_block_indices.is_empty() {
                    let mut lines = Vec::with_capacity(removed_block_indices.len());
                    for idx in &removed_block_indices {
                        let text = left_refs.get(*idx as usize).copied().unwrap_or("");
                        lines.push(BlockTextLine {
                            text: SmolStr::from(text),
                            bg: Some(pal.removed_bg),
                            fg: None,
                            gutter_marker: Some(GutterMarker::DiffRemoved),
                            marks: Vec::new(),
                            strikethrough: true,
                        });
                    }
                    let block = BlockDeco {
                        side: BlockSide::Above,
                        height: removed_block_indices.len() as f32 * line_height,
                        kind: BlockKind::Text { lines },
                    };
                    push_block_at(&mut entries, right_rope, hunk.right_lines.start, block);
                }

                for pair in &pairs {
                    match pair {
                        LinePair::Added { right_offset } => {
                            let line_idx = hunk.right_lines.start + *right_offset as usize;
                            line_bg_one(
                                &mut entries,
                                right_rope,
                                line_idx,
                                pal.added_bg,
                                GutterMarker::DiffAdded,
                            );
                        }
                        LinePair::Modified { left_offset, right_offset } => {
                            let l = left_refs[*left_offset as usize];
                            let r = right_refs[*right_offset as usize];
                            let line_idx = hunk.right_lines.start + *right_offset as usize;
                            line_bg_one(
                                &mut entries,
                                right_rope,
                                line_idx,
                                pal.added_bg,
                                GutterMarker::DiffModified,
                            );
                            let cd = char_diff(l, r);
                            // Removed side: the old line isn't in the buffer, so
                            // show it as a phantom block above its replacement —
                            // otherwise a paired in-line replace renders only the
                            // green inserts and the deleted text is invisible. The
                            // per-character delete/insert marks ride the intraline
                            // toggle; the line itself always shows.
                            let removed_marks: Vec<(std::ops::Range<usize>, Color)> = if intraline
                            {
                                cd.deletes
                                    .iter()
                                    .filter(|d| d.start < d.end)
                                    .map(|d| (d.clone(), pal.word_removed))
                                    .collect()
                            } else {
                                Vec::new()
                            };
                            push_block_at(
                                &mut entries,
                                right_rope,
                                line_idx,
                                BlockDeco {
                                    side: BlockSide::Above,
                                    height: line_height,
                                    kind: BlockKind::Text {
                                        lines: vec![BlockTextLine {
                                            text: SmolStr::from(l),
                                            bg: Some(pal.removed_bg),
                                            fg: None,
                                            gutter_marker: Some(GutterMarker::DiffRemoved),
                                            marks: removed_marks,
                                            strikethrough: true,
                                        }],
                                    },
                                },
                            );
                            // Added side: per-character insert marks (intraline only).
                            if intraline {
                                let right_byte_start = byte_of_line(right_rope, line_idx);
                                for ins in &cd.inserts {
                                    let s = right_byte_start + ins.start;
                                    let e = (right_byte_start + ins.end)
                                        .min(right_rope.len_bytes());
                                    if s < e {
                                        entries.push((
                                            s..e,
                                            Decoration::Mark(MarkStyle {
                                                bg: Some(pal.word_added),
                                                ..MarkStyle::default()
                                            }),
                                        ));
                                    }
                                }
                            }
                        }
                        LinePair::Removed { .. } => {}
                    }
                }
            }
        }
    }
    RangeSet::from_iter(entries)
}

/// Mirror of `unified_decorations_opts` for "suggestion mode": the editable
/// buffer holds the OLD text (diff LEFT side) and we overlay an agent's
/// proposed NEW text (diff RIGHT side) on top, Google-Docs style. Hunks are
/// `diff_lines(buffer_text, proposal_text)`, so `hunk.left_lines` index the
/// buffer and `hunk.right_lines` index the proposal.
///
///   - Added  → the proposed lines aren't in the buffer, so they ride a green
///     phantom `Text` block injected above `hunk.left_lines.start`.
///   - Removed → the lines are real buffer text the agent wants to drop, so
///     mark them with a red line bg plus a struck-through `Mark`.
///   - Modified → both: strike the real old buffer lines AND ghost the new
///     proposal lines as a green block (no per-line pairing refinement). When
///     `intraline` is set, paired lines also get per-character delete marks on
///     the buffer side; the line-level strike always shows.
pub fn proposal_decorations(
    buffer_rope: &Rope,
    proposal_text: &str,
    hunks: &[Hunk],
    line_height: f32,
    theme: Option<&Theme>,
    intraline: bool,
) -> DecorationSet {
    let pal = DiffPalette::from_theme(theme);
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();
    let proposal_lines_all = split_lines(proposal_text);

    for hunk in hunks {
        match hunk.kind {
            HunkKind::Context => {}
            HunkKind::Added => {
                push_proposal_block(
                    &mut entries,
                    buffer_rope,
                    hunk.left_lines.start,
                    &proposal_lines_all,
                    &hunk.right_lines,
                    line_height,
                    &pal,
                );
            }
            HunkKind::Removed => {
                strike_buffer_lines(
                    &mut entries,
                    buffer_rope,
                    &hunk.left_lines,
                    &GutterMarker::DiffRemoved,
                    &pal,
                );
            }
            HunkKind::Modified => {
                strike_buffer_lines(
                    &mut entries,
                    buffer_rope,
                    &hunk.left_lines,
                    &GutterMarker::DiffModified,
                    &pal,
                );
                if intraline {
                    intraline_removed_marks(
                        &mut entries,
                        buffer_rope,
                        &hunk.left_lines,
                        &proposal_lines_all,
                        &hunk.right_lines,
                        &pal,
                    );
                }
                push_proposal_block(
                    &mut entries,
                    buffer_rope,
                    hunk.left_lines.start,
                    &proposal_lines_all,
                    &hunk.right_lines,
                    line_height,
                    &pal,
                );
            }
        }
    }
    RangeSet::from_iter(entries)
}

/// Split text into buffer-style lines (drops the trailing empty line that a
/// final newline produces), matching how `unified_decorations_opts` splits its
/// `left_text` into `left_lines_all`.
fn split_lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            out.push(&text[start..i]);
            start = i + 1;
        }
    }
    if start <= text.len() {
        out.push(&text[start..]);
    }
    if matches!(out.last(), Some(last) if last.is_empty()) {
        out.pop();
    }
    out
}

/// Mark real buffer lines the agent proposes to remove: a red `Line` bg + the
/// given gutter marker, plus a struck-through `Mark` over each line's byte
/// range so the deleted text reads as crossed out.
fn strike_buffer_lines(
    entries: &mut Vec<(std::ops::Range<usize>, Decoration)>,
    buffer_rope: &Rope,
    lines: &std::ops::Range<usize>,
    marker: &GutterMarker,
    pal: &DiffPalette,
) {
    line_bg(entries, buffer_rope, lines, pal.removed_bg, marker);
    for line in lines.clone() {
        if line >= buffer_rope.len_lines() {
            break;
        }
        let start = buffer_rope.line_to_byte(line);
        let raw_end = if line + 1 < buffer_rope.len_lines() {
            buffer_rope.line_to_byte(line + 1)
        } else {
            buffer_rope.len_bytes()
        };
        // Don't strike the trailing newline byte (if any) — only the text.
        let end = if raw_end > start && buffer_rope.line_str(line).ends_with('\n') {
            raw_end - 1
        } else {
            raw_end
        };
        if start < end {
            entries.push((
                start..end,
                Decoration::Mark(MarkStyle {
                    strikethrough: true,
                    bg: Some(pal.word_removed),
                    ..MarkStyle::default()
                }),
            ));
        }
    }
}

/// Inject the proposed (new) lines as a green phantom `Text` block above the
/// buffer line where the insertion lands. The text isn't in the buffer, so it
/// can only be shown as a block.
fn push_proposal_block(
    entries: &mut Vec<(std::ops::Range<usize>, Decoration)>,
    buffer_rope: &Rope,
    buffer_line: usize,
    proposal_lines_all: &[&str],
    right_lines: &std::ops::Range<usize>,
    line_height: f32,
    pal: &DiffPalette,
) {
    let mut lines = Vec::with_capacity(right_lines.len());
    for ri in right_lines.clone() {
        let text = proposal_lines_all.get(ri).copied().unwrap_or("");
        lines.push(BlockTextLine {
            text: SmolStr::from(text),
            bg: Some(pal.added_bg),
            fg: None,
            gutter_marker: Some(GutterMarker::DiffAdded),
            marks: Vec::new(),
            strikethrough: false,
        });
    }
    let block = BlockDeco {
        side: BlockSide::Above,
        height: right_lines.len() as f32 * line_height,
        kind: BlockKind::Text { lines },
    };
    push_block_at(entries, buffer_rope, buffer_line, block);
}

/// For a Modified hunk, char-diff each buffer line against its positional
/// counterpart in the proposal and emit word-removed `Mark`s over the deleted
/// substrings in the buffer. Positional pairing (line N to line N) keeps this
/// simple — no similarity refinement.
fn intraline_removed_marks(
    entries: &mut Vec<(std::ops::Range<usize>, Decoration)>,
    buffer_rope: &Rope,
    left_lines: &std::ops::Range<usize>,
    proposal_lines_all: &[&str],
    right_lines: &std::ops::Range<usize>,
    pal: &DiffPalette,
) {
    for (offset, line) in left_lines.clone().enumerate() {
        if line >= buffer_rope.len_lines() {
            break;
        }
        let old = buffer_rope.line_str(line);
        let old = old.strip_suffix('\n').unwrap_or(&old);
        let new = proposal_lines_all
            .get(right_lines.start + offset)
            .copied()
            .unwrap_or("");
        let line_start = byte_of_line(buffer_rope, line);
        for del in char_diff(old, new).deletes {
            let s = line_start + del.start;
            let e = (line_start + del.end).min(buffer_rope.len_bytes());
            if s < e {
                entries.push((
                    s..e,
                    Decoration::Mark(MarkStyle {
                        strikethrough: true,
                        bg: Some(pal.word_removed),
                        ..MarkStyle::default()
                    }),
                ));
            }
        }
    }
}

fn line_bg_one(
    entries: &mut Vec<(std::ops::Range<usize>, Decoration)>,
    rope: &Rope,
    line: usize,
    bg: Color,
    marker: GutterMarker,
) {
    if line >= rope.len_lines() {
        return;
    }
    let start = rope.line_to_byte(line);
    let end = if line + 1 < rope.len_lines() {
        rope.line_to_byte(line + 1)
    } else {
        rope.len_bytes()
    };
    entries.push((
        start..end,
        Decoration::Line(LineStyle {
            bg: Some(bg),
            gutter_marker: Some(marker),
            ..LineStyle::default()
        }),
    ));
}

fn push_block_after_hunk(
    entries: &mut Vec<(std::ops::Range<usize>, Decoration)>,
    rope: &Rope,
    line_idx: usize,
    height: f32,
    kind: BlockKind,
) {
    let total = rope.len_lines();
    if line_idx >= total {
        if total == 0 {
            entries.push((
                0..0,
                Decoration::Block(BlockDeco { side: BlockSide::Above, height, kind }),
            ));
            return;
        }
        let last = total - 1;
        let anchor = rope.line_to_byte(last);
        entries.push((
            anchor..anchor,
            Decoration::Block(BlockDeco { side: BlockSide::Below, height, kind }),
        ));
        return;
    }
    let anchor = rope.line_to_byte(line_idx);
    entries.push((
        anchor..anchor,
        Decoration::Block(BlockDeco { side: BlockSide::Above, height, kind }),
    ));
}

fn push_block_at(
    entries: &mut Vec<(std::ops::Range<usize>, Decoration)>,
    rope: &Rope,
    line_idx: usize,
    mut block: BlockDeco,
) {
    let total = rope.len_lines();
    if line_idx >= total {
        if total == 0 {
            entries.push((0..0, Decoration::Block(block)));
            return;
        }
        block.side = BlockSide::Below;
        let last = total - 1;
        let anchor = rope.line_to_byte(last);
        entries.push((anchor..anchor, Decoration::Block(block)));
        return;
    }
    let anchor = rope.line_to_byte(line_idx);
    entries.push((anchor..anchor, Decoration::Block(block)));
}

fn line_bg(
    entries: &mut Vec<(std::ops::Range<usize>, Decoration)>,
    rope: &Rope,
    lines: &std::ops::Range<usize>,
    bg: Color,
    marker: &GutterMarker,
) {
    for line in lines.clone() {
        if line >= rope.len_lines() {
            break;
        }
        let start = rope.line_to_byte(line);
        let end = if line + 1 < rope.len_lines() {
            rope.line_to_byte(line + 1)
        } else {
            rope.len_bytes()
        };
        entries.push((
            start..end,
            Decoration::Line(LineStyle {
                bg: Some(bg),
                gutter_marker: Some(marker.clone()),
                ..LineStyle::default()
            }),
        ));
    }
}

fn byte_of_line(rope: &Rope, line: usize) -> usize {
    if line < rope.len_lines() {
        rope.line_to_byte(line)
    } else {
        rope.len_bytes()
    }
}

#[cfg(test)]
mod intraline_tests {
    use super::*;
    use editor_core::diff::lines as diff_lines;
    use editor_core::rope::Rope;

    /// Count Mark decorations in a `DecorationSet` — these are the
    /// per-character intraline highlights emitted on Modified hunks.
    fn count_mark_decos(set: &DecorationSet) -> usize {
        set.iter_all()
            .filter(|(_, d)| matches!(d, Decoration::Mark(_)))
            .count()
    }

    #[test]
    fn intraline_off_drops_per_char_marks() {
        let left = "the quick brown fox\n";
        let right = "the slow brown fox\n";
        let hunks = diff_lines(left, right);
        let rope = Rope::from_str(right);
        let on =
            unified_decorations_opts(&rope, left, &hunks, 18.0, None, true);
        let off =
            unified_decorations_opts(&rope, left, &hunks, 18.0, None, false);
        // With intraline on we expect at least one per-character Mark
        // (the substring that differs). With it off, zero Mark decorations.
        assert!(
            count_mark_decos(&on) >= 1,
            "intraline=true emits Mark decorations",
        );
        assert_eq!(
            count_mark_decos(&off),
            0,
            "intraline=false suppresses per-character Mark decorations",
        );
    }

    #[test]
    fn default_entry_keeps_intraline_on() {
        let left = "alpha\n";
        let right = "alpa\n";
        let hunks = diff_lines(left, right);
        let rope = Rope::from_str(right);
        let default_set = unified_decorations(&rope, left, &hunks, 18.0, None);
        let on_set =
            unified_decorations_opts(&rope, left, &hunks, 18.0, None, true);
        assert_eq!(count_mark_decos(&default_set), count_mark_decos(&on_set));
    }

    /// Concatenated text of every removed-line phantom block
    /// (`BlockSide::Above` + `BlockKind::Text`) in the set.
    fn removed_block_texts(set: &DecorationSet) -> Vec<String> {
        set.iter_all()
            .filter_map(|(_, d)| match d {
                Decoration::Block(BlockDeco {
                    side: BlockSide::Above,
                    kind: BlockKind::Text { lines },
                    ..
                }) => Some(
                    lines
                        .iter()
                        .map(|l| l.text.to_string())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                _ => None,
            })
            .collect()
    }

    /// Regression: a *similar*-line replace pairs as a Modified line
    /// (similarity >= 0.5). The deleted text isn't in the buffer, so it must
    /// surface as a removed-line block above its replacement — otherwise the
    /// unified inline view shows only the green inserts and the removal is
    /// invisible. Holds with the intraline toggle both on and off (the line
    /// always shows; only the per-character marks ride the toggle).
    #[test]
    fn modified_line_renders_removed_text_block() {
        let left = "alpha\nBody text here.\ncharlie\n";
        let right = "alpha\nBody text now.\ncharlie\n";
        let hunks = diff_lines(left, right);
        let rope = Rope::from_str(right);
        for intraline in [true, false] {
            let set = unified_decorations_opts(&rope, left, &hunks, 18.0, None, intraline);
            let removed = removed_block_texts(&set);
            assert!(
                removed.iter().any(|t| t.contains("Body text here.")),
                "intraline={intraline}: removed block missing old line text; got {removed:?}",
            );
            // Removed-line text is struck through (in addition to its red bg).
            let all_struck = set.iter_all().all(|(_, d)| match d {
                Decoration::Block(BlockDeco {
                    side: BlockSide::Above,
                    kind: BlockKind::Text { lines },
                    ..
                }) => lines.iter().all(|l| l.strikethrough),
                _ => true,
            });
            assert!(all_struck, "intraline={intraline}: removed-line block not struck through");
        }
    }
}

#[cfg(test)]
mod proposal_tests {
    use super::*;
    use editor_core::diff::lines as diff_lines;
    use editor_core::rope::Rope;

    /// Count struck-through `Mark` decorations — these mark real buffer lines
    /// the agent proposes to delete.
    fn count_strike_marks(set: &DecorationSet) -> usize {
        set.iter_all()
            .filter(|(_, d)| matches!(d, Decoration::Mark(m) if m.strikethrough))
            .count()
    }

    /// Concatenated text of every added phantom block (`BlockSide::Above` +
    /// `BlockKind::Text`) in the set, paired with whether its lines are struck.
    fn added_blocks(set: &DecorationSet) -> Vec<(String, bool)> {
        set.iter_all()
            .filter_map(|(_, d)| match d {
                Decoration::Block(BlockDeco {
                    side: BlockSide::Above,
                    kind: BlockKind::Text { lines },
                    ..
                }) => Some((
                    lines.iter().map(|l| l.text.to_string()).collect::<Vec<_>>().join("\n"),
                    lines.iter().any(|l| l.strikethrough),
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn agent_insertion_makes_unstruck_block_no_strike_mark() {
        // Proposal adds a line; the buffer (old side) doesn't have it.
        let buffer = "alpha\ncharlie\n";
        let proposal = "alpha\nbravo\ncharlie\n";
        let hunks = diff_lines(buffer, proposal);
        let rope = Rope::from_str(buffer);
        let set = proposal_decorations(&rope, proposal, &hunks, 18.0, None, true);

        let blocks = added_blocks(&set);
        assert!(
            blocks.iter().any(|(t, _)| t.contains("bravo")),
            "inserted line missing from phantom block; got {blocks:?}",
        );
        assert!(
            blocks.iter().all(|(_, struck)| !*struck),
            "inserted phantom block lines must not be struck through; got {blocks:?}",
        );
        assert_eq!(
            count_strike_marks(&set),
            0,
            "a pure insertion must not produce strikethrough Marks",
        );
    }

    #[test]
    fn agent_deletion_strikes_buffer_line_no_added_block() {
        // Proposal drops a line that lives in the buffer.
        let buffer = "alpha\nbravo\ncharlie\n";
        let proposal = "alpha\ncharlie\n";
        let hunks = diff_lines(buffer, proposal);
        let rope = Rope::from_str(buffer);
        let set = proposal_decorations(&rope, proposal, &hunks, 18.0, None, true);

        assert!(
            count_strike_marks(&set) >= 1,
            "a deletion must strike the real buffer line",
        );
        assert!(
            added_blocks(&set).is_empty(),
            "a pure deletion must not inject an added phantom block; got {:?}",
            added_blocks(&set),
        );
    }

    #[test]
    fn identical_input_is_empty() {
        let buffer = "alpha\nbravo\ncharlie\n";
        let hunks = diff_lines(buffer, buffer);
        let rope = Rope::from_str(buffer);
        let set = proposal_decorations(&rope, buffer, &hunks, 18.0, None, true);
        assert_eq!(set.iter_all().count(), 0, "context-only diff emits no decorations");
    }
}
