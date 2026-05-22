//! YAML frontmatter folding. Detects `---\n…\n---` at the very start of the
//! document, emits a fold chevron on the first `---` line, and (when collapsed)
//! hides the body lines.

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::FoldChevron;

use editor_core::decoration::LineStyle;

use editor_core::rangeset::RangeSet;

use editor_core::theme::Theme;
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
    // Inline frontmatter detection: opening `---` must be the very first
    // line; close at the next `---` line.
    let detected: Option<(usize, usize)> = {
        let mut lines = text.split('\n');
        match lines.next() {
            Some(first) if first.trim_end_matches('\r') == "---" => {
                let mut found = None;
                for (idx, line) in lines.enumerate() {
                    if line.trim_end_matches('\r') == "---" {
                        found = Some((0_usize, idx + 1));
                        break;
                    }
                }
                found
            }
            _ => None,
        }
    };
    let Some((open_line, close_line)) = detected else {
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

