//! Tooltip primitive (SPEC §9.5, IMPLEMENTATION §16.5.2).
//!
//! A `Tooltip` is a declarative description of a floating overlay anchored to
//! either a buffer byte offset or a widget-local pixel coordinate. The host
//! populates `ViewState::tooltips` each frame; the renderer (e.g.
//! `editor-egui`) walks the list and paints each entry above the editor.
//!
//! v1 supports plain text and (un-rendered) markdown content. The richer
//! "callback that draws into a sub-painter" form described in SPEC §9.5 is
//! deferred — see `TooltipContent` for the simple variants we ship now.
//!
//! Dismiss-on-blur and hover lifecycle are the host's responsibility for v1;
//! the widget just paints whatever is currently in the list.

use smol_str::SmolStr;

/// Where a tooltip should be anchored.
#[derive(Clone, Debug, PartialEq)]
pub enum TooltipAnchor {
    /// Anchor to a byte offset in the buffer. The renderer maps the byte to a
    /// screen position using the line layout / height map.
    BufferPos { byte: u32 },
    /// Anchor to a pixel coordinate, in widget-local space (origin at the
    /// widget's top-left, before scroll).
    Coords { x: f32, y: f32 },
}

/// Where the tooltip should sit relative to its anchor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TooltipPlacement {
    Above,
    Below,
    /// Prefer Below, flip to Above if the tooltip would clip the bottom of
    /// the widget rect.
    Smart,
}

/// Tooltip content. v1 only supports text; `Markdown` is currently rendered
/// the same as `Text` (no inline markdown rendering inside tooltips yet —
/// hosts that need rich content can pre-render to text). A render-callback
/// variant for arbitrary egui UI will be added later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TooltipContent {
    Text(SmolStr),
    Markdown(SmolStr),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tooltip {
    /// Stable identifier supplied by the host; used by the renderer to derive
    /// a unique egui `Area` id so multiple tooltips don't collide.
    pub id: u64,
    pub anchor: TooltipAnchor,
    pub placement: TooltipPlacement,
    pub content: TooltipContent,
}
