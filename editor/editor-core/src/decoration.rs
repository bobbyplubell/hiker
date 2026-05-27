//! Decorations: styling and replacement applied to ranges of the document.
//!
//! Decorations are *not* part of the document. They're produced by extensions
//! (markdown live preview, syntax highlighting, diff hunks, occurrence
//! highlight) and consumed by the view layer at paint time.

use std::fmt;
use std::sync::Arc;

use smol_str::SmolStr;

use crate::rangeset::{HeightAffecting, RangeSet};

/// Severity of a [`Diagnostic`], ordered from most to least critical.
///
/// Drives the decoration provider's underline color and gutter marker for a
/// diagnostic; see [`GutterMarker::Diagnostic`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

/// A structured lint/error report attached to a byte range of the document.
///
/// Diagnostics are produced by hosts (LSP, tree-sitter queries, custom
/// linters) and rendered by the diagnostic decoration provider as
/// wavy-underlined marks plus per-line gutter markers. See SPEC §9.7 and
/// IMPLEMENTATION §16.5.1.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Diagnostic {
    /// Byte range in the document this diagnostic applies to.
    pub range: std::ops::Range<usize>,
    pub severity: Severity,
    pub message: SmolStr,
    /// Producer identifier, e.g. "rustc", "clippy", "tree-sitter".
    pub source: SmolStr,
    /// Optional machine-readable code, e.g. "E0308".
    pub code: Option<SmolStr>,
}

/// Data-side trait for inline widgets embedded in a line of text.
///
/// Widgets are introduced as decorations via [`Decoration::InlineWidget`]. The
/// painter measures the widget at the current font size and reserves an
/// equivalently-sized region in the line layout. Painting of arbitrary
/// widgets is deferred — the v1 egui adapter renders a placeholder rect.
pub trait InlineWidget: Send + Sync {
    /// Pixel size when laid out at the given font size.
    fn measure(&self, font_size: f32) -> (f32, f32);
    /// True if clicks land on the widget; default false = passthrough to cursor.
    fn handles_click(&self) -> bool {
        false
    }
    /// Stable identity for diffing; defaults to 0.
    fn widget_id(&self) -> u64 {
        0
    }
    /// When `Some`, the inline-widget painter renders the returned text
    /// with the supplied colors instead of the bordered "widget"
    /// placeholder. Used by hosts that want a small textual insertion
    /// at a byte position (patch-review intraline `new_str` rendering,
    /// inline diagnostics, etc.) without introducing a new decoration
    /// variant. Default `None` keeps the placeholder behavior for
    /// non-textual widgets.
    fn display(&self) -> Option<InlineWidgetDisplay> {
        None
    }
}

#[derive(Clone, Debug)]
pub struct InlineWidgetDisplay {
    pub text: SmolStr,
    pub bg: Option<Color>,
    pub fg: Option<Color>,
    pub strikethrough: bool,
}

/// Data-side trait for block widgets injected in the vertical gap above /
/// below a line. See [`InlineWidget`] for v1 rendering limitations.
pub trait BlockWidget: Send + Sync {
    /// Returns the laid-out height (pixels) for the given font size and
    /// available width.
    fn measure(&self, font_size: f32, width: f32) -> f32;
    fn handles_click(&self) -> bool {
        false
    }
    fn widget_id(&self) -> u64 {
        0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MarkStyle {
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub underline: bool,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    /// 1.0 == base font size. Used for heading scaling.
    pub font_scale: Option<f32>,
    pub monospace: bool,
    /// When true, cursor motion treats the marked range as a single
    /// indivisible unit: motion commands that would land inside the range
    /// snap to the appropriate boundary in the direction of motion.
    /// `Decoration::Replace` is implicitly atomic regardless of this field.
    pub atomic: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LineStyle {
    pub bg: Option<Color>,
    /// 1.0 == base line height. > 1.0 makes the line taller.
    pub height_scale: Option<f32>,
    /// Left indent in pixels (host-resolved).
    pub indent: Option<f32>,
    pub gutter_marker: Option<GutterMarker>,
    /// When true, the line is hidden entirely (height = 0, no text, no gutter,
    /// no selection, no cursor rendering). Used by folds.
    pub hide: bool,
    /// Show a clickable fold chevron in the gutter on this line.
    pub fold_chevron: Option<FoldChevron>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FoldChevron {
    pub id: u64,
    pub collapsed: bool,
}

#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum GutterMarker {
    DiffAdded,
    DiffRemoved,
    DiffModified,
    Bookmark,
    Diagnostic(Severity),
    Custom(SmolStr),
}

#[derive(Clone)]
pub enum Decoration {
    /// Inline styling applied to a byte range.
    Mark(MarkStyle),
    /// Whole-line styling. The range should cover one line (rope's
    /// `line_to_byte` boundaries).
    Line(LineStyle),
    /// Hide the underlying text and (optionally) render `display` instead.
    Replace { display: Option<SmolStr> },
    /// Inject vertical space above or below the line at `range.start`. The
    /// underlying model is *not* modified — line numbers stay correct, cursor
    /// positions still map to source. Equivalent to CodeMirror's "block
    /// widget" and VSCode's "view zone".
    Block(BlockDeco),
    /// Trait-object inline widget. The painter reserves a region sized via
    /// `widget.measure(font_size)`. v1 limitation: the egui adapter renders a
    /// styled placeholder rect with a small "widget" label rather than calling
    /// into the widget for painting.
    InlineWidget {
        widget: Arc<dyn InlineWidget>,
        /// If true, the inline widget is treated as a single indivisible unit
        /// for cursor motion (equivalent to `MarkStyle { atomic: true }`).
        atomic: bool,
    },
    /// Trait-object block widget. v1 limitation: rendered as a colored
    /// placeholder rect in the block zone; the trait's painting hook is
    /// deferred to a future revision.
    BlockWidget {
        side: BlockSide,
        widget: Arc<dyn BlockWidget>,
    },
}

impl fmt::Debug for Decoration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Decoration::Mark(s) => f.debug_tuple("Mark").field(s).finish(),
            Decoration::Line(s) => f.debug_tuple("Line").field(s).finish(),
            Decoration::Replace { display } => {
                f.debug_struct("Replace").field("display", display).finish()
            }
            Decoration::Block(b) => f.debug_tuple("Block").field(b).finish(),
            Decoration::InlineWidget { atomic, widget } => f
                .debug_struct("InlineWidget")
                .field("atomic", atomic)
                .field("widget_id", &widget.widget_id())
                .finish(),
            Decoration::BlockWidget { side, widget } => f
                .debug_struct("BlockWidget")
                .field("side", side)
                .field("widget_id", &widget.widget_id())
                .finish(),
        }
    }
}

// TODO(serde): `BlockDeco`/`BlockKind` and `Decoration` itself are not
// serializable because they may transitively reference `Arc<dyn Widget>`
// trait objects. Revisit when a stable widget identity scheme exists.
#[derive(Clone, Debug)]
pub struct BlockDeco {
    pub side: BlockSide,
    pub height: f32,
    pub kind: BlockKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BlockSide {
    Above,
    Below,
}

#[derive(Clone, Debug)]
pub enum BlockKind {
    /// Diagonal-stripe hatched fill — used for spacer alignment in
    /// side-by-side diffs.
    Hatched(Color),
    /// Solid fill — used for plain gutters / placeholders.
    Solid(Color),
    /// Rendered text — used by the unified inline diff to show removed
    /// lines stacked above their replacement.
    Text {
        lines: Vec<BlockTextLine>,
    },
    /// Clickable bar that toggles a fold. The host receives a
    /// `ClickAction::ToggleFold(id)` and is responsible for updating its fold
    /// state and re-emitting decorations on the next frame.
    Expander {
        id: u64,
        label: SmolStr,
        /// Whether the body is currently collapsed; used to choose the
        /// chevron glyph and label tense.
        collapsed: bool,
    },
    /// Horizontal row with a label on the left and one or more clickable
    /// buttons on the right. Used by hosts that want a single-line "do
    /// something" affordance attached to a block range (patch-review
    /// per-hunk Accept/Reject, unanchored-pin rows). Each enabled button
    /// registers a `ClickAction::WidgetClick(button.id)` zone.
    ActionRow {
        label: SmolStr,
        /// Small leading glyph rendered before the label (e.g. "?" for an
        /// unanchored hunk). Empty / None when not needed.
        glyph: Option<SmolStr>,
        tone: ActionTone,
        buttons: Vec<ActionButton>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ActionTone {
    Normal,
    Warning,
    Conflicted,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ActionButton {
    pub id: u64,
    pub label: SmolStr,
    pub style: ActionButtonStyle,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ActionButtonStyle {
    Primary,
    Danger,
    Neutral,
}

#[derive(Clone, Debug)]
pub struct BlockTextLine {
    pub text: SmolStr,
    pub bg: Option<Color>,
    pub fg: Option<Color>,
    pub gutter_marker: Option<GutterMarker>,
    /// Intraline byte ranges (within `text`) to emphasize with a bg color.
    pub marks: Vec<(std::ops::Range<usize>, Color)>,
    /// Draw a horizontal strike line through the whole text row. Set on
    /// removed-diff lines so deleted content reads as struck-through in
    /// addition to its red background.
    pub strikethrough: bool,
}

impl HeightAffecting for Decoration {
    /// A decoration affects line height when it hides a line, scales its
    /// height, or injects a vertical block above/below it. Mark / Replace /
    /// InlineWidget styling stays within the existing line box and does not.
    /// This is the single source of truth the heightmap driver relies on; it
    /// must stay in sync with the variants matched in the egui widget's
    /// `apply_line_height_decorations`.
    fn affects_height(&self) -> bool {
        matches!(
            self,
            Decoration::Line(LineStyle { hide: true, .. })
                | Decoration::Line(LineStyle { height_scale: Some(_), .. })
                | Decoration::Block(_)
                | Decoration::BlockWidget { .. }
        )
    }
}

pub type Set = RangeSet<Decoration>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn height_affecting_variants() {
        assert!(Decoration::Line(LineStyle { hide: true, ..Default::default() }).affects_height());
        assert!(Decoration::Line(LineStyle {
            height_scale: Some(2.0),
            ..Default::default()
        })
        .affects_height());
        assert!(
            Decoration::Block(BlockDeco {
                side: BlockSide::Above,
                height: 10.0,
                kind: BlockKind::Solid(Color::TRANSPARENT),
            })
            .affects_height()
        );
    }

    #[test]
    fn paint_only_variants_do_not_affect_height() {
        assert!(!Decoration::Mark(MarkStyle::default()).affects_height());
        assert!(!Decoration::Replace { display: None }.affects_height());
        // A Line with only a bg / indent (no hide, no height_scale) is
        // paint-only — it stays within the existing line box.
        assert!(!Decoration::Line(LineStyle {
            bg: Some(Color::TRANSPARENT),
            ..Default::default()
        })
        .affects_height());
    }

    #[test]
    fn set_marks_affects_height_at_construction() {
        let height_set: Set = RangeSet::from_iter([(
            0..1,
            Decoration::Line(LineStyle { height_scale: Some(1.5), ..Default::default() }),
        )]);
        assert!(height_set.affects_height());

        let paint_set: Set =
            RangeSet::from_iter([(0..1, Decoration::Mark(MarkStyle { bold: true, ..Default::default() }))]);
        assert!(!paint_set.affects_height());

        // Mixed set: one height-affecting entry flips the whole set.
        let mixed: Set = RangeSet::from_iter([
            (0..1, Decoration::Mark(MarkStyle::default())),
            (2..3, Decoration::Line(LineStyle { hide: true, ..Default::default() })),
        ]);
        assert!(mixed.affects_height());
    }
}
