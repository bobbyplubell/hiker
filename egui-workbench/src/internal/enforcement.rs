//! Post-frame invariant enforcement.
//!
//! egui_tiles permits structural states that the workbench wants to
//! forbid (e.g., pinned tabs after Regular tabs, panel tabs ending up
//! in the editor tree). The functions here run after `dock.ui()` to
//! restore invariants.

use egui_tiles::{Container, Tile, Tree};

use crate::TabHandle;
use crate::tab::TabState;

/// Walks every `Tabs` container in the tree and reorders its `children`
/// vec so pinned-state handles come first, preserving relative order
/// within each band (pinned vs not).
///
/// `state_for` resolves a `TabHandle` to its `TabState`. Handles not
/// found (stale tree entries during a tear-down frame) are treated as
/// `Regular` to keep the pass total.
pub(crate) fn enforce_pinned_first<F>(tree: &mut Tree<TabHandle>, state_for: F)
where
    F: Fn(TabHandle) -> TabState,
{
    // Two passes so we don't hold a `&mut Tiles` while looking at panes
    // (and to keep this O(n) without allocating per-container scratch).
    let container_ids: Vec<_> = tree
        .tiles
        .iter()
        .filter_map(|(id, tile)| match tile {
            Tile::Container(Container::Tabs(_)) => Some(*id),
            _ => None,
        })
        .collect();

    for cid in container_ids {
        // Snapshot child handles + states.
        let snapshot: Vec<(egui_tiles::TileId, bool)> = {
            let Some(Tile::Container(Container::Tabs(tabs))) = tree.tiles.get(cid) else {
                continue;
            };
            tabs.children
                .iter()
                .map(|child_id| {
                    let pinned = match tree.tiles.get(*child_id) {
                        Some(Tile::Pane(h)) => state_for(*h) == TabState::Pinned,
                        _ => false,
                    };
                    (*child_id, pinned)
                })
                .collect()
        };

        // Are they already pinned-first?
        let mut seen_unpinned = false;
        let mut needs_sort = false;
        for (_, pinned) in &snapshot {
            if *pinned && seen_unpinned {
                needs_sort = true;
                break;
            }
            if !*pinned {
                seen_unpinned = true;
            }
        }
        if !needs_sort {
            continue;
        }

        // Stable partition: pinned first, then rest.
        let mut reordered: Vec<egui_tiles::TileId> =
            snapshot.iter().filter(|(_, p)| *p).map(|(id, _)| *id).collect();
        reordered.extend(snapshot.iter().filter(|(_, p)| !*p).map(|(id, _)| *id));

        if let Some(Tile::Container(Container::Tabs(tabs))) = tree.tiles.get_mut(cid) {
            tabs.children = reordered;
        }
    }
}
