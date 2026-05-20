//! Tile-layout construction + on-disk persistence (egui_tiles).
//!
//! Holds:
//!   - `default_dock()` — factory layout (Files/Clusters/Trails on the
//!     left, an empty center for buffer tabs, Search/Related/Backlinks/
//!     Chat on the right).
//!   - `LayoutFile` — versioned serializable wrapper (v1 was the old
//!     egui_dock format; v2 is the egui_tiles format). v1 files are
//!     dropped on load and regenerated from defaults.
//!   - Side-tile detection heuristics for tracking which Tabs container
//!     plays the role of left / center / right after deserialisation.
//!   - Load-priority chain: vault override -> user default -> factory.
//!
//! Buffer tabs (DockTab::Tab) live in the center tile; panels
//! (DockTab::Panel) live in their default-side tile. The reconciler in
//! `tabs.rs` enforces this invariant on every frame.
//!
//! The tracked side-tile ids live on `Session` next to the tree itself;
//! `default_dock` returns them alongside the tree so the caller can wire
//! them up.

use std::path::{Path, PathBuf};

use egui_tiles::{Container, Tile, TileId, Tree};
use serde::{Deserialize, Serialize};

use crate::panels_registry::{
    PANEL_BACKLINKS, PANEL_CHAT, PANEL_CLUSTERS, PANEL_FILES, PANEL_RELATED,
    PANEL_SEARCH, PANEL_TRAILS, PanelRegistry, PanelSide,
};
use crate::tab::DockTab;

pub const LAYOUT_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
pub struct LayoutFile {
    pub version: u32,
    pub dock: Tree<DockTab>,
    pub center_tile: TileId,
    pub left_tile: TileId,
    pub right_tile: TileId,
}

#[derive(Serialize)]
struct LayoutFileRef<'a> {
    version: u32,
    dock: &'a Tree<DockTab>,
    center_tile: TileId,
    left_tile: TileId,
    right_tile: TileId,
}

/// Forwards-compat dispatch on `version`. v1 was egui_dock; we drop it
/// and let the caller fall back to factory defaults. v2 is identity.
fn migrate(value: serde_json::Value) -> Option<LayoutFile> {
    let version = value
        .get("version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    match version {
        2 => serde_json::from_value::<LayoutFile>(value).ok(),
        1 => {
            tracing::info!(
                "layout: v1 (egui_dock) schema detected, regenerating from defaults"
            );
            None
        }
        other => {
            tracing::warn!(
                version = other,
                "layout: unknown schema version; ignoring file",
            );
            None
        }
    }
}

/// Bundle returned by `default_dock` (and produced after every load) so
/// callers can store the side-tile ids on Session along with the tree.
pub struct DockBundle {
    pub tree: Tree<DockTab>,
    pub center_tile: TileId,
    pub left_tile: TileId,
    pub right_tile: TileId,
}

/// Build the factory layout. Three Tabs containers (left/center/right)
/// inside a horizontal Linear, with shares 0.18 / 0.61 / 0.21.
pub fn default_dock() -> DockBundle {
    let mut tiles = egui_tiles::Tiles::default();

    // Left tabs: Files/Clusters/Trails.
    let left_children = vec![
        tiles.insert_pane(DockTab::panel(PANEL_FILES)),
        tiles.insert_pane(DockTab::panel(PANEL_CLUSTERS)),
        tiles.insert_pane(DockTab::panel(PANEL_TRAILS)),
    ];
    let left_tile = tiles.insert_tab_tile(left_children);

    // Center: empty Tabs container — buffer tabs land here.
    let center_tile = tiles.insert_tab_tile(Vec::new());

    // Right tabs: Search/Related/Backlinks/Chat.
    let right_children = vec![
        tiles.insert_pane(DockTab::panel(PANEL_SEARCH)),
        tiles.insert_pane(DockTab::panel(PANEL_RELATED)),
        tiles.insert_pane(DockTab::panel(PANEL_BACKLINKS)),
        tiles.insert_pane(DockTab::panel(PANEL_CHAT)),
    ];
    let right_tile = tiles.insert_tab_tile(right_children);

    let root_children = vec![left_tile, center_tile, right_tile];
    let root_id = tiles.insert_horizontal_tile(root_children.clone());

    // Apply target shares (left 18%, center 61%, right 21%). Shares are
    // re-normalised by the linear layout each frame; what matters is the
    // ratios.
    if let Some(Tile::Container(Container::Linear(linear))) = tiles.get_mut(root_id) {
        linear.shares.set_share(left_tile, 0.18);
        linear.shares.set_share(center_tile, 0.61);
        linear.shares.set_share(right_tile, 0.21);
    }

    let tree = Tree::new("hiker-dock", root_id, tiles);
    DockBundle {
        tree,
        center_tile,
        left_tile,
        right_tile,
    }
}

/// Walk a deserialised tree and pick the Tabs container that should
/// play "center": the first one that holds zero `DockTab::Panel(_)`
/// children. Falls back to any Tabs container if no panel-free one
/// exists.
fn detect_center_tile(tree: &Tree<DockTab>) -> Option<TileId> {
    let mut first_tabs: Option<TileId> = None;
    for (id, tile) in tree.tiles.iter() {
        let Tile::Container(Container::Tabs(tabs)) = tile else { continue };
        if first_tabs.is_none() {
            first_tabs = Some(*id);
        }
        let has_panel = tabs.children.iter().any(|c| {
            matches!(
                tree.tiles.get(*c),
                Some(Tile::Pane(DockTab::Panel(_))),
            )
        });
        if !has_panel {
            return Some(*id);
        }
    }
    first_tabs
}

/// Pick a Tabs container that hosts a panel whose `default_side` matches
/// `side`. If multiple match, return the first found. Falls back to
/// `None` so the caller can decide what to do.
fn detect_side_tile(tree: &Tree<DockTab>, side: PanelSide) -> Option<TileId> {
    let reg = PanelRegistry::all();
    for (id, tile) in tree.tiles.iter() {
        let Tile::Container(Container::Tabs(tabs)) = tile else { continue };
        let matches = tabs.children.iter().any(|c| match tree.tiles.get(*c) {
            Some(Tile::Pane(DockTab::Panel(pid))) => reg
                .by_id(pid)
                .is_some_and(|p| p.default_side == side),
            _ => false,
        });
        if matches {
            return Some(*id);
        }
    }
    None
}

// ---- Paths --------------------------------------------------------------

pub fn vault_layout_path(vault_root: &Path) -> PathBuf {
    vault_root.join(".hiker/layout.json")
}

pub fn user_config_dir() -> PathBuf {
    directories::ProjectDirs::from("", "", "hiker")
        .map(|p| p.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".hiker-config"))
}

pub fn user_default_layout_path() -> PathBuf {
    user_config_dir().join("default-layout.json")
}

pub fn user_profiles_dir() -> PathBuf {
    user_config_dir().join("layouts")
}

// ---- Load / save --------------------------------------------------------

fn read_layout_at(path: &Path) -> Option<LayoutFile> {
    let bytes = std::fs::read(path).ok()?;
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(error = %err, path = %path.display(), "layout: parse failed");
            return None;
        }
    };
    migrate(value)
}

/// Resolve the layout to use on bootstrap. Tries the vault override
/// first, then the user-default, then falls back to the factory layout.
pub fn load_for_vault(vault_root: &Path) -> DockBundle {
    if let Some(lf) = read_layout_at(&vault_layout_path(vault_root)) {
        return finalize_loaded(lf);
    }
    if let Some(lf) = read_layout_at(&user_default_layout_path()) {
        return finalize_loaded(lf);
    }
    default_dock()
}

/// Run the post-deserialise fixups: make sure every registered panel is
/// present somewhere (adding missing ones to their default side), drop
/// unknown panels, and re-confirm the center/left/right tile ids.
fn finalize_loaded(lf: LayoutFile) -> DockBundle {
    let LayoutFile { dock, center_tile, left_tile, right_tile, .. } = lf;
    let mut bundle = DockBundle { tree: dock, center_tile, left_tile, right_tile };
    // Re-validate the side-tile ids against the loaded tree; if any are
    // stale (e.g. user manually edited the file) fall back to heuristic.
    if bundle.tree.tiles.get(bundle.center_tile).is_none() {
        bundle.center_tile = detect_center_tile(&bundle.tree)
            .unwrap_or(bundle.center_tile);
    }
    if bundle.tree.tiles.get(bundle.left_tile).is_none() {
        bundle.left_tile = detect_side_tile(&bundle.tree, PanelSide::Left)
            .unwrap_or(bundle.center_tile);
    }
    if bundle.tree.tiles.get(bundle.right_tile).is_none() {
        bundle.right_tile = detect_side_tile(&bundle.tree, PanelSide::Right)
            .unwrap_or(bundle.center_tile);
    }
    ensure_all_panels_present(&mut bundle);
    bundle
}

/// Insert any registered panel that's missing from the tree at its
/// default side. Drops `Panel` entries whose id no longer matches a
/// registered panel (downgrade case).
fn ensure_all_panels_present(bundle: &mut DockBundle) {
    let reg = PanelRegistry::all();
    // 1. Drop unknown panels (panes only).
    let stale: Vec<TileId> = bundle
        .tree
        .tiles
        .iter()
        .filter_map(|(id, tile)| match tile {
            Tile::Pane(DockTab::Panel(pid)) if reg.by_id(pid).is_none() => Some(*id),
            _ => None,
        })
        .collect();
    for id in stale {
        bundle.tree.remove_recursively(id);
    }
    // 2. Find which panels are present.
    let present: std::collections::HashSet<String> = bundle
        .tree
        .tiles
        .iter()
        .filter_map(|(_, tile)| match tile {
            Tile::Pane(DockTab::Panel(pid)) => Some(pid.clone()),
            _ => None,
        })
        .collect();
    for p in reg.list() {
        if present.contains(p.id) {
            continue;
        }
        let target = match p.default_side {
            PanelSide::Left => bundle.left_tile,
            PanelSide::Right => bundle.right_tile,
            PanelSide::Center => bundle.center_tile,
        };
        let pane_id = bundle
            .tree
            .tiles
            .insert_pane(DockTab::panel(p.id));
        if let Some(Tile::Container(container)) = bundle.tree.tiles.get_mut(target) {
            container.add_child(pane_id);
        } else {
            // Target tile is gone; drop the pane on the center as last resort.
            if let Some(Tile::Container(container)) =
                bundle.tree.tiles.get_mut(bundle.center_tile)
            {
                container.add_child(pane_id);
            }
        }
    }
}

// ---- Serialisation helpers ---------------------------------------------

fn serialize(bundle: &DockBundle) -> serde_json::Result<String> {
    let lf = LayoutFileRef {
        version: LAYOUT_VERSION,
        dock: &bundle.tree,
        center_tile: bundle.center_tile,
        left_tile: bundle.left_tile,
        right_tile: bundle.right_tile,
    };
    serde_json::to_string_pretty(&lf)
}

/// Write the dock to `<vault>/.hiker/layout.json`. Best-effort.
pub fn save_for_vault(
    vault_root: &Path,
    bundle: &DockBundle,
) -> std::io::Result<()> {
    let path = vault_layout_path(vault_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serialize(bundle)
        .map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

/// Copy the per-vault layout to the user's default location.
pub fn save_user_default(bundle: &DockBundle) -> std::io::Result<()> {
    let path = user_default_layout_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serialize(bundle)
        .map_err(std::io::Error::other)?;
    std::fs::write(path, body)
}

/// Save the current dock as a named profile under
/// `~/.config/hiker/layouts/<name>.json`.
pub fn save_profile(
    name: &str,
    bundle: &DockBundle,
) -> std::io::Result<PathBuf> {
    let dir = user_profiles_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.json"));
    let body = serialize(bundle)
        .map_err(std::io::Error::other)?;
    std::fs::write(&path, body)?;
    Ok(path)
}

pub fn load_profile(name: &str) -> Option<DockBundle> {
    let path = user_profiles_dir().join(format!("{name}.json"));
    read_layout_at(&path).map(finalize_loaded)
}

#[allow(dead_code)]
pub fn list_profiles() -> Vec<String> {
    let dir = user_profiles_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            out.push(stem.to_string());
        }
    }
    out.sort();
    out
}

// ---- Helpers for tabs.rs / actions.rs ----------------------------------

/// Find the `TileId` of the pane carrying `DockTab::Panel(panel_id)`,
/// if present.
pub fn find_panel_tile(tree: &Tree<DockTab>, panel_id: &str) -> Option<TileId> {
    for (id, tile) in tree.tiles.iter() {
        if let Tile::Pane(DockTab::Panel(pid)) = tile
            && pid == panel_id
        {
            return Some(*id);
        }
    }
    None
}

/// Find the `TileId` of the pane carrying `DockTab::Tab(tab_id)`, if
/// present.
#[allow(dead_code)]
pub fn find_tab_tile(tree: &Tree<DockTab>, tab_id: crate::tab::TabId) -> Option<TileId> {
    for (id, tile) in tree.tiles.iter() {
        if let Tile::Pane(DockTab::Tab(t)) = tile
            && *t == tab_id
        {
            return Some(*id);
        }
    }
    None
}

/// Move every `DockTab::Tab(_)` pane currently NOT under `center_tile`
/// into it. The center tile must be a Tabs container; if it isn't
/// (e.g. user nuked it), no-op (the next reconcile will rebuild).
pub fn enforce_buffer_tabs_in_center(
    tree: &mut Tree<DockTab>,
    center_tile: TileId,
) {
    let Some(Tile::Container(Container::Tabs(_))) = tree.tiles.get(center_tile)
    else {
        return;
    };
    // Collect strays first to avoid mutating while iterating.
    let strays: Vec<TileId> = tree
        .tiles
        .iter()
        .filter_map(|(id, tile)| match tile {
            Tile::Pane(DockTab::Tab(_)) => {
                let parent = tree.tiles.parent_of(*id);
                if parent != Some(center_tile) {
                    Some(*id)
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();
    for id in strays {
        tree.move_tile_to_container(id, center_tile, usize::MAX, false);
    }
}
