//! Decoration provider for selection-occurrence highlights. Viewport-scoped.

use editor_core::{Color, Decoration, DecorationSet, EditorState, MarkStyle, RangeSet};

use crate::multicursor;

const HIGHLIGHT_BG: Color = Color::rgba(255, 235, 130, 70);

pub fn occurrence_decorations(
    state: &EditorState,
    viewport: std::ops::Range<usize>,
) -> DecorationSet {
    let occurrences = multicursor::selection_occurrences(state, viewport);
    let entries = occurrences.into_iter().map(|r| {
        (
            r,
            Decoration::Mark(MarkStyle {
                bg: Some(HIGHLIGHT_BG),
                ..MarkStyle::default()
            }),
        )
    });
    RangeSet::from_iter(entries)
}
