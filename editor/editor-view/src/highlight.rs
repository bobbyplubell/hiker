//! Decoration provider for selection-occurrence highlights. Viewport-scoped.

use editor_core::decoration::Color;

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::MarkStyle;

use editor_core::rangeset::RangeSet;
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
