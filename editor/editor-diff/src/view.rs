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
                            // Skip the per-character insert marks when the
                            // intraline toggle is off — line-level
                            // emphasis is enough.
                            if intraline {
                                let cd = char_diff(l, r);
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
                            } else {
                                let _ = l;
                                let _ = r;
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
}
