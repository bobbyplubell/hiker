//! editor-view: input events, viewport state, motion, command dispatch.
//! Backend-neutral. Renderer-agnostic. Hosts wire platform events into
//! [`InputEvent`] and platform paint calls receive coordinates / layout
//! information from [`ViewState`].

pub mod event;
pub mod view;
pub mod motion;
pub mod multicursor;
pub mod command;
pub mod ime;
pub mod occurrence;
pub mod autopair;
pub mod brackets;
pub mod completion;
pub mod diagnostics;
pub mod highlights;
pub mod special_chars;
pub mod tooltip;
pub mod wrap;
pub mod search;
pub mod snippet;
pub mod panel;

pub use brackets::{bracket_match_decorations, BracketPair, DEFAULT_BRACKETS};
pub use search::{
    replace_all, replace_current, run_search, search_decorations, SearchFlags, SearchState,
};
pub use completion::{CompletionItem, CompletionKind, CompletionSource, CompletionState};
pub use snippet::{
    map_through as snippet_map_through, mirror_sync as snippet_mirror_sync,
    selection_for_stop as snippet_selection_for_stop, ParseError as SnippetParseError, Snippet,
    SnippetState,
};
pub use diagnostics::diagnostic_decorations;
pub use highlights::{active_line_decorations, trailing_whitespace_decorations};
pub use special_chars::{special_chars_decorations, SpecialCharsFlags};
pub use event::{ImeEvent, InputEvent, Key, KeyEvent, Modifiers, MouseButton, MouseEvent, NamedKey};
pub use ime::ImeState;
pub use panel::{Panel, PanelKind, PanelPlacement, PanelStack};
pub use tooltip::{Tooltip, TooltipAnchor, TooltipContent, TooltipPlacement};
pub use view::{
    ClickAction, ClickRect, ClickZone, DecorationLayers, DragState, HeightMap, IndentProvider,
    LineGeometry, ViewState,
};
pub use wrap::{compute_wraps, WrapMap, WrappedLine};

/// Convert a byte-range viewport to a line range `[start, end)`. The returned
/// end line is exclusive and clamped to the document's line count. Used by
/// paint-only decoration providers that walk lines so they can scope their
/// work to the visible region.
pub fn viewport_lines(
    doc: &editor_core::Rope,
    viewport: &std::ops::Range<usize>,
) -> std::ops::Range<usize> {
    let total = doc.len_lines();
    if total == 0 {
        return 0..0;
    }
    let doc_len = doc.len_bytes();
    let start = doc.byte_to_line(viewport.start.min(doc_len));
    let end_byte = viewport
        .end
        .min(doc_len)
        .saturating_sub(1)
        .max(viewport.start);
    let end = doc.byte_to_line(end_byte).saturating_add(1);
    start..end.min(total)
}
