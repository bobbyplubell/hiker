//! editor-core: state, rope, transactions, selection. No rendering deps.
//!
//! Serialization is opt-in via the `serde` feature.

pub mod compartment;
pub mod sumtree;
pub mod rope;
pub mod change;
pub mod anchor;
pub mod selection;
pub mod rangeset;
pub mod decoration;
pub mod diagnostic;
pub mod transaction;
pub mod history;
pub mod state;
pub mod diff;
pub mod theme;
pub mod facet;

pub use anchor::{Anchor, Bias};
pub use facet::{Facet, FacetStore, FieldStore, StateField, ViewPlugin};
pub use change::{ChangeSet, Op};
pub use compartment::{Compartment, CompartmentId, CompartmentStore};
pub use decoration::{
    BlockDeco, BlockKind, BlockSide, BlockTextLine, BlockWidget, Color, Decoration, DecorationSet,
    FoldChevron, GutterMarker, InlineWidget, LineStyle, MarkStyle,
};
pub use diagnostic::{Diagnostic, Severity};
pub use history::History;
pub use rangeset::RangeSet;
pub use rope::Rope;
pub use selection::{SelRange, Selection};
pub use state::{ChangeFilter, EditorState, TransactionExtender, TransactionFilter, TransactionListener};
#[cfg(feature = "serde")]
pub use state::SavedState;
pub use theme::{
    dark_default, light_default, DiagnosticColors, DiffColors, MarkdownColors, Theme,
    ThemePalette,
};
pub use transaction::{Annotations, EditType, StateEffect, Transaction};
