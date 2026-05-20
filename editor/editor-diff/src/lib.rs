//! Diff view helpers: turn a `Vec<Hunk>` (from `editor_core::diff::diff_lines`)
//! into `DecorationSet`s ready to feed into a view's `decorations` layer.
//!
//! The widget itself doesn't know about diff — it just renders Block, Mark,
//! and Line decorations. This crate is the "consumer" that produces them.

mod view;

pub use view::{alignment_decorations, unified_decorations, unified_decorations_opts};
