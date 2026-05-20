//! Stable handle types for tabs and editor groups.
//!
//! `TabHandle` is a monotonic `u64` allocated by `Workbench::next_handle`.
//! It survives across tree reorderings, splits, and moves between groups.
//!
//! `GroupHandle` wraps an `egui_tiles::TileId` referring to a `Tabs`
//! container in either the editor tree or the panel tree.

use egui_tiles::TileId;

/// Stable identifier for a tab payload inside a [`crate::Workbench`].
///
/// Allocated monotonically; only reused after the referenced tab has been
/// removed from both the editor tree and the panel tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TabHandle(pub u64);

impl TabHandle {
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// Identifier for an editor group (a `Tabs` container in the underlying tree).
///
/// Note: unlike [`TabHandle`], this is not guaranteed stable across full
/// layout reloads (deserializing a layout allocates fresh `TileId`s).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GroupHandle(pub TileId);
