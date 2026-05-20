//! YAML frontmatter folding. Detects `---\n…\n---` at the very start of the
//! document, emits a fold chevron on the first `---` line, and (when collapsed)
//! hides the body lines.

use editor_core::{
    Decoration, DecorationSet, EditorState, FoldChevron, LineStyle, RangeSet, Theme,
};

use crate::folds::FoldState;

/// Stable id for the frontmatter fold. High bit set so it can't collide with
/// the hash-based heading/list fold ids in practice.
pub const FRONTMATTER_FOLD_ID: u64 = 0xF20E_0001;

pub fn frontmatter_fold(
    state: &EditorState,
    fold_state: &FoldState,
    _theme: Option<&Theme>,
) -> DecorationSet {
    let text = state.doc.to_string();
    let Some((open_line, close_line)) = detect_frontmatter(&text) else {
        return RangeSet::from_iter(std::iter::empty());
    };

    let total_lines = state.doc.len_lines();
    let doc_len = state.doc.len_bytes();
    let line_byte_end = |line: usize| -> usize {
        if line + 1 < total_lines {
            state.doc.line_to_byte(line + 1)
        } else {
            doc_len
        }
    };

    let collapsed = fold_state.contains(&FRONTMATTER_FOLD_ID);
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();

    let head_start = state.doc.line_to_byte(open_line);
    let head_end = line_byte_end(open_line);
    entries.push((
        head_start..head_end,
        Decoration::Line(LineStyle {
            fold_chevron: Some(FoldChevron {
                id: FRONTMATTER_FOLD_ID,
                collapsed,
            }),
            ..LineStyle::default()
        }),
    ));

    if collapsed {
        for line in (open_line + 1)..=close_line {
            if line >= total_lines {
                break;
            }
            let s = state.doc.line_to_byte(line);
            let e = line_byte_end(line);
            entries.push((
                s..e,
                Decoration::Line(LineStyle {
                    hide: true,
                    ..LineStyle::default()
                }),
            ));
        }
    }

    RangeSet::from_iter(entries)
}

/// Returns `(open_line, close_line)` if the doc starts with a `---` frontmatter
/// block. The opening `---` must be the very first line.
fn detect_frontmatter(text: &str) -> Option<(usize, usize)> {
    let mut lines = text.split('\n');
    let first = lines.next()?;
    if first.trim_end_matches('\r') != "---" {
        return None;
    }
    for (idx, line) in lines.enumerate() {
        let stripped = line.trim_end_matches('\r');
        if stripped == "---" {
            return Some((0, idx + 1));
        }
    }
    None
}
