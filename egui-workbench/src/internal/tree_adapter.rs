//! Helpers that bridge `egui_tiles::Tree` to workbench concepts.
//!
//! The editor tree and panel tree store [`crate::TabHandle`] payloads
//! (not the tabs themselves). Payload lookup is a single hashmap probe
//! against `EditorArea::entries`.

use egui_tiles::{Tile, TileId, Tree};

use crate::TabHandle;

/// Walk the tree and find the [`TileId`] of the [`Tabs`](egui_tiles::Container::Tabs)
/// container that currently holds `handle` as one of its children.
pub(crate) fn find_group_of<P>(tree: &Tree<P>, handle: TabHandle) -> Option<TileId>
where
    P: PartialEq<TabHandle>,
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
pub(crate) fn find_pane_of<P>(tree: &Tree<P>, handle: TabHandle) -> Option<TileId>
where
    P: PartialEq<TabHandle>,
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

/// Collect all `TabHandle`s that live in the given Tabs container.
pub(crate) fn handles_in_group(tree: &Tree<TabHandle>, group: TileId) -> Vec<TabHandle> {
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

/// Iterate every `TabHandle` referenced anywhere in the tree.
pub(crate) fn all_handles(tree: &Tree<TabHandle>) -> Vec<TabHandle> {
    tree.tiles
        .iter()
        .filter_map(|(_, tile)| match tile {
            Tile::Pane(h) => Some(*h),
            _ => None,
        })
        .collect()
}

/// Build a `pane TileId → parent Tabs-container TileId` map for the
/// whole tree. Used by [`crate::editor_area::EditorBehavior::pane_ui`]
/// to resolve a pane's owning group without a per-frame tree walk.
pub(crate) fn pane_to_group_map<P>(
    tree: &Tree<P>,
) -> std::collections::HashMap<TileId, TileId> {
    let mut map = std::collections::HashMap::new();
    for (tile_id, tile) in tree.tiles.iter() {
        if let Tile::Container(egui_tiles::Container::Tabs(tabs)) = tile {
            for child in &tabs.children {
                map.insert(*child, *tile_id);
            }
        }
    }
    map
}

/// Editor-group `TileId`s in depth-first traversal order rooted at
/// `tree.root`. Containers are visited in the order their children
/// appear in the parent (so left-to-right for `Horizontal`,
/// top-to-bottom for `Vertical`). Used by group-focus navigation so
/// `focus_group(N)` is stable across frames.
pub(crate) fn groups_in_order<P>(tree: &Tree<P>) -> Vec<TileId> {
    let mut out = Vec::new();
    if let Some(root) = tree.root {
        walk(tree, root, &mut out);
    }
    return out;

    fn walk<P>(tree: &Tree<P>, id: TileId, out: &mut Vec<TileId>) {
        match tree.tiles.get(id) {
            Some(Tile::Container(egui_tiles::Container::Tabs(_))) => out.push(id),
            Some(Tile::Container(egui_tiles::Container::Linear(lin))) => {
                for child in &lin.children {
                    walk(tree, *child, out);
                }
            }
            Some(Tile::Container(egui_tiles::Container::Grid(grid))) => {
                for child in grid.children() {
                    walk(tree, *child, out);
                }
            }
            _ => {}
        }
    }
}

/// Resolve the active tab handle inside the given Tabs container.
pub(crate) fn active_handle_in_group<P>(tree: &Tree<P>, group: TileId) -> Option<TabHandle>
where
    P: Copy + Into<TabHandle>,
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
