//! Helpers that bridge `egui_tiles::Tree` to workbench concepts.
//!
//! The editor tree and panel tree store [`crate::TabId`] payloads
//! (not the tabs themselves). Payload lookup is a single hashmap probe
//! against `EditorArea::entries`.

use egui_tiles::{Tile, TileId, Tree};

use crate::workspace::TabId;

/// Walk the tree and find the [`TileId`] of the [`Tabs`](egui_tiles::Container::Tabs)
/// container that currently holds `handle` as one of its children.
pub(crate) fn find_group_of<P>(tree: &Tree<P>, handle: TabId) -> Option<TileId>
where
    P: PartialEq<TabId>,
{
    for (tile_id, tile) in tree.tiles.iter() {
        if let Tile::Pane(pane) = tile
            && pane == &handle
        {
            return tree.tiles.parent_of(*tile_id);
        }
    }
    None
}

/// Find the [`TileId`] of the pane carrying `handle`.
pub(crate) fn find_pane_of<P>(tree: &Tree<P>, handle: TabId) -> Option<TileId>
where
    P: PartialEq<TabId>,
{
    for (tile_id, tile) in tree.tiles.iter() {
        if let Tile::Pane(pane) = tile
            && pane == &handle
        {
            return Some(*tile_id);
        }
    }
    None
}

/// First [`Tabs`](egui_tiles::Container::Tabs) container we encounter
/// during traversal. Used as the fallback "active group" when no group
/// has been explicitly focused yet.
pub(crate) fn first_tabs_container<P>(tree: &Tree<P>) -> Option<TileId> {
    for (tile_id, tile) in tree.tiles.iter() {
        if let Tile::Container(container) = tile
            && matches!(container, egui_tiles::Container::Tabs(_))
        {
            return Some(*tile_id);
        }
    }
    None
}

/// Collect all `TabId`s that live in the given Tabs container.
pub(crate) fn handles_in_group(tree: &Tree<TabId>, group: TileId) -> Vec<TabId> {
    let Some(Tile::Container(egui_tiles::Container::Tabs(tabs))) = tree.tiles.get(group) else {
        return Vec::new();
    };
    tabs.children
        .iter()
        .filter_map(|child| match tree.tiles.get(*child) {
            Some(Tile::Pane(h)) => Some(*h),
            _ => None,
        })
        .collect()
}

/// Iterate every `TabId` referenced anywhere in the tree.
pub(crate) fn all_handles(tree: &Tree<TabId>) -> Vec<TabId> {
    tree.tiles
        .iter()
        .filter_map(|(_, tile)| match tile {
            Tile::Pane(h) => Some(*h),
            _ => None,
        })
        .collect()
}

/// Resolve the active tab handle inside the given Tabs container.
pub(crate) fn active_handle_in_group<P>(tree: &Tree<P>, group: TileId) -> Option<TabId>
where
    P: Copy + Into<TabId>,
{
    let Some(Tile::Container(egui_tiles::Container::Tabs(tabs))) = tree.tiles.get(group) else {
        return None;
    };
    let active = tabs.active?;
    let Tile::Pane(pane) = tree.tiles.get(active)? else {
        return None;
    };
    Some((*pane).into())
}
