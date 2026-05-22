//! editor-view: input events, viewport state, motion, command dispatch.
//! Backend-neutral. Renderer-agnostic. Hosts wire platform events into
//! [`InputEvent`] and platform paint calls receive coordinates / layout
//! information from [`ViewState`].

pub mod events;
pub mod viewport;
pub mod motion;
pub mod multicursor;
pub mod command;
pub mod highlight;
pub mod pairs;
pub mod brackets;
pub mod autocomplete;
pub mod diagnostics;
pub mod highlights;
pub mod whitespace;
pub mod popup;
pub mod wrapping;
pub mod find;
pub mod snippets;
pub mod panels;


/// Convert a byte-range viewport to a line range `[start, end)`. The returned
/// end line is exclusive and clamped to the document's line count. Used by
/// paint-only decoration providers that walk lines so they can scope their
/// work to the visible region.
pub fn viewport_lines(
    doc: &editor_core::rope::Rope,
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
