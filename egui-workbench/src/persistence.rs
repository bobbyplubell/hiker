//! Layout persistence — versioned JSON snapshot of the workbench state.
//!
//! The on-disk schema is intentionally workbench-owned: the trees serialise
//! as a recursive [`TileDto`] enum, never as `egui_tiles`' internal
//! `Tree<P>` representation. This decouples the v1 layout format from
//! upstream serde changes in `egui_tiles` and keeps the JSON
//! self-describing enough to inspect or hand-edit.
//!
//! Versioning policy: the schema bumps independently from the crate
//! version. v1 is the initial format; future versions add fields and
//! ship a migration arm in [`migrate`]. Migrations are append-only —
//! they never delete data.

use std::collections::HashMap;
use std::hash::Hash;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::workspace::TabId;
use crate::side_bar::Side;
use crate::tab::{Document, TabEntry, State};
use crate::workspace::Workbench;

/// Trait marker for tabs that can be persisted via [`WorkbenchLayout`].
///
/// Blanket implementation: anything that is `Document + Serialize +
/// DeserializeOwned` automatically qualifies. Hosts opt in by adding
/// the `serde` derives to their tab type.
pub trait PersistableTab: Document + Serialize + DeserializeOwned {}
impl<T: Document + Serialize + DeserializeOwned> PersistableTab for T {}

/// Snapshot of a tab's persisted state. The payload is the host's tab
/// serialised to JSON so the schema stays type-erased.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TabEntryDto {
    pub handle: u64,
    pub state: State,
    pub payload: serde_json::Value,
}

/// Serialisable form of a single tile in an editor- or panel-area tree.
/// Mirrors `egui_tiles::Tile` but encodes children recursively (no
/// stable `TileId`s in the on-disk form).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum TileDto {
    /// A leaf — references a [`TabEntryDto`] by handle.
    Pane { handle: u64 },
    /// A tabbed container. `active` is the *index* of the active child
    /// within `children` (rather than a `TileId`), so the schema stays
    /// independent of how `egui_tiles` numbers tiles internally.
    Tabs {
        children: Vec<TileDto>,
        active: Option<usize>,
    },
    /// A linear (horizontal or vertical) split. `shares` is the
    /// per-child share; an empty vec means "use defaults".
    Linear {
        dir: LinearDirDto,
        children: Vec<TileDto>,
        #[serde(default)]
        shares: Vec<f32>,
    },
    /// A grid container.
    Grid {
        layout: GridLayoutDto,
        children: Vec<TileDto>,
        #[serde(default)]
        col_shares: Vec<f32>,
        #[serde(default)]
        row_shares: Vec<f32>,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum LinearDirDto {
    Horizontal,
    Vertical,
}

impl From<egui_tiles::LinearDir> for LinearDirDto {
    fn from(d: egui_tiles::LinearDir) -> Self {
        match d {
            egui_tiles::LinearDir::Horizontal => LinearDirDto::Horizontal,
            egui_tiles::LinearDir::Vertical => LinearDirDto::Vertical,
        }
    }
}

impl From<LinearDirDto> for egui_tiles::LinearDir {
    fn from(d: LinearDirDto) -> Self {
        match d {
            LinearDirDto::Horizontal => egui_tiles::LinearDir::Horizontal,
            LinearDirDto::Vertical => egui_tiles::LinearDir::Vertical,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum GridLayoutDto {
    Auto,
    Columns(usize),
}

impl From<egui_tiles::GridLayout> for GridLayoutDto {
    fn from(g: egui_tiles::GridLayout) -> Self {
        match g {
            egui_tiles::GridLayout::Auto => GridLayoutDto::Auto,
            egui_tiles::GridLayout::Columns(n) => GridLayoutDto::Columns(n),
        }
    }
}

impl From<GridLayoutDto> for egui_tiles::GridLayout {
    fn from(g: GridLayoutDto) -> Self {
        match g {
            GridLayoutDto::Auto => egui_tiles::GridLayout::Auto,
            GridLayoutDto::Columns(n) => egui_tiles::GridLayout::Columns(n),
        }
    }
}

/// Serialisable form of a tabbed-area tree (editor or panel). An empty
/// tree has `root: None`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TreeDto {
    pub root: Option<TileDto>,
}

/// Versioned, serialisable snapshot of the full workbench layout.
///
/// Round-trippable through JSON. Db use the workbench-owned
/// [`TreeDto`]; tab payloads are stored as `serde_json::Value` so the
/// schema does not depend on the host's tab type at compile time.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkbenchLayout {
    pub version: u32,
    pub primary_side: Side,
    pub side_bar_visible: bool,
    pub side_bar_width: f32,
    pub secondary_side_bar_visible: bool,
    pub secondary_side_bar_width: f32,
    pub active_activity: Option<serde_json::Value>,
    /// Hidden activity modes, each as the host `Mode` serialised to JSON
    /// (the schema stays independent of the host's type). Defaulted so
    /// layout files written before this field load cleanly.
    #[serde(default)]
    pub hidden_activities: Vec<serde_json::Value>,
    /// User-preferred activity order, same type-erased encoding as
    /// `hidden_activities`.
    #[serde(default)]
    pub activity_order: Vec<serde_json::Value>,
    pub panel_area_visible: bool,
    pub panel_area_maximized: bool,
    pub panel_area_height: f32,
    pub status_bar_visible: bool,
    pub editor_tree: TreeDto,
    pub panel_tree: TreeDto,
    pub tab_entries: Vec<TabEntryDto>,
    pub next_handle: u64,
}

impl Default for WorkbenchLayout {
    fn default() -> Self {
        Self {
            version: 1,
            primary_side: Side::Left,
            side_bar_visible: true,
            side_bar_width: 260.0,
            secondary_side_bar_visible: false,
            secondary_side_bar_width: 260.0,
            active_activity: None,
            hidden_activities: Vec::new(),
            activity_order: Vec::new(),
            panel_area_visible: false,
            panel_area_maximized: false,
            panel_area_height: 240.0,
            status_bar_visible: true,
            editor_tree: TreeDto::default(),
            panel_tree: TreeDto::default(),
            tab_entries: Vec::new(),
            next_handle: 1,
        }
    }
}

#[derive(Debug)]
pub enum LayoutError {
    UnknownSchemaVersion(u32),
    PayloadDeserialise {
        handle: u64,
        source: serde_json::Error,
    },
    ModeDeserialise(serde_json::Error),
}

impl std::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutError::UnknownSchemaVersion(v) => {
                write!(f, "unknown workbench layout schema version: {v}")
            }
            LayoutError::PayloadDeserialise { handle, source } => {
                write!(f, "failed to deserialise tab {handle} payload: {source}")
            }
            LayoutError::ModeDeserialise(e) => {
                write!(f, "failed to deserialise active activity mode: {e}")
            }
        }
    }
}

impl std::error::Error for LayoutError {}

/// Schema migration entry point. Returns `None` for unknown versions
/// (logged as a warning) rather than panicking, so a stale on-disk
/// layout cannot crash the host.
pub fn migrate(value: serde_json::Value) -> Option<WorkbenchLayout> {
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as u32;
    match version {
        1 => serde_json::from_value(value).ok(),
        other => {
            tracing::warn!(version = other, "workbench: unknown layout schema; ignoring");
            None
        }
    }
}

impl<Tab, Mode> Workbench<Tab, Mode>
where
    Tab: PersistableTab,
    Mode: Clone + Eq + Hash + Serialize + DeserializeOwned + 'static,
{
    /// Capture the current layout as a serialisable snapshot.
    pub fn layout(&self) -> WorkbenchLayout {
        let mut tab_entries: Vec<TabEntryDto> = Vec::new();
        for (handle, entry) in self.editor_area.iter_entries() {
            tab_entries.push(entry_to_dto(handle, entry));
        }
        for (handle, entry) in self.panel_area_entries() {
            tab_entries.push(entry_to_dto(handle, entry));
        }
        WorkbenchLayout {
            version: 1,
            primary_side: self.primary_side_bar.side,
            side_bar_visible: self.primary_side_bar.visible,
            side_bar_width: self.primary_side_bar.width,
            secondary_side_bar_visible: self.secondary_side_bar.visible,
            secondary_side_bar_width: self.secondary_side_bar.width,
            active_activity: self
                .activity_bar
                .active()
                .and_then(|m| serde_json::to_value(m).ok()),
            hidden_activities: self
                .activity_bar
                .hidden()
                .iter()
                .filter_map(|m| serde_json::to_value(m).ok())
                .collect(),
            activity_order: self
                .activity_bar
                .order()
                .iter()
                .filter_map(|m| serde_json::to_value(m).ok())
                .collect(),
            panel_area_visible: self.panel_area.visible,
            panel_area_maximized: self.panel_area.maximized,
            panel_area_height: self.panel_area.height,
            status_bar_visible: self.status_bar.visible,
            editor_tree: tree_to_dto(&self.editor_area.tree_clone()),
            panel_tree: tree_to_dto(&self.panel_area_tree_clone()),
            tab_entries,
            next_handle: self.next_handle,
        }
    }

    /// Owning variant of [`Self::layout`].
    pub fn into_layout(self) -> WorkbenchLayout {
        self.layout()
    }

    /// Replace the workbench's state with the contents of `layout`.
    pub fn apply_layout(&mut self, layout: WorkbenchLayout) -> Result<(), LayoutError> {
        if layout.version != 1 {
            return Err(LayoutError::UnknownSchemaVersion(layout.version));
        }

        // Decode the host's Mode (if any).
        let active_mode = match layout.active_activity {
            Some(v) => Some(
                serde_json::from_value::<Mode>(v).map_err(LayoutError::ModeDeserialise)?,
            ),
            None => None,
        };

        // Hidden set + order are best-effort: a mode the host no longer
        // exposes (variant removed since the layout was written) is
        // dropped rather than failing the whole restore.
        let hidden: Vec<Mode> = layout
            .hidden_activities
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect();
        let order: Vec<Mode> = layout
            .activity_order
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect();

        // Decode tab payloads.
        let mut decoded: HashMap<TabId, TabEntry<Tab>> = HashMap::new();
        for dto in layout.tab_entries {
            let tab: Tab = serde_json::from_value(dto.payload).map_err(|e| {
                LayoutError::PayloadDeserialise {
                    handle: dto.handle,
                    source: e,
                }
            })?;
            let handle = TabId(dto.handle);
            decoded.insert(handle, TabEntry::new(tab, dto.state, handle));
        }

        // Build the two trees from their DTOs.
        let editor_tree = tree_from_dto(
            &layout.editor_tree,
            egui::Id::new("egui_workbench::editor_tree"),
        );
        let panel_tree = tree_from_dto(
            &layout.panel_tree,
            egui::Id::new("egui_workbench::panel_tree"),
        );

        // Partition decoded entries by which tree references each handle.
        let editor_handles = crate::internal::tree_adapter::all_handles(&editor_tree);
        let panel_handles = crate::internal::tree_adapter::all_handles(&panel_tree);
        let mut editor_entries = HashMap::new();
        let mut panel_entries = HashMap::new();
        for h in editor_handles {
            if let Some(entry) = decoded.remove(&h) {
                editor_entries.insert(h, entry);
            }
        }
        for h in panel_handles {
            if let Some(entry) = decoded.remove(&h) {
                panel_entries.insert(h, entry);
            }
        }

        // Apply.
        self.activity_bar.set_active(active_mode);
        self.activity_bar.set_hidden(hidden);
        self.activity_bar.set_order(order);
        self.primary_side_bar.side = layout.primary_side;
        self.primary_side_bar.visible = layout.side_bar_visible;
        self.primary_side_bar.width = layout.side_bar_width;
        self.secondary_side_bar.side = match layout.primary_side {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        };
        self.secondary_side_bar.visible = layout.secondary_side_bar_visible;
        self.secondary_side_bar.width = layout.secondary_side_bar_width;
        self.activity_bar.set_side(layout.primary_side);

        self.editor_area.replace_tree(editor_tree, editor_entries);
        self.panel_area_replace(panel_tree, panel_entries);

        self.panel_area.visible = layout.panel_area_visible;
        self.panel_area.maximized = layout.panel_area_maximized;
        self.panel_area.height = layout.panel_area_height;
        self.status_bar.visible = layout.status_bar_visible;
        self.next_handle = layout.next_handle.max(1);
        self.dirty = false;
        Ok(())
    }
}

fn entry_to_dto<Tab: PersistableTab>(handle: TabId, entry: &TabEntry<Tab>) -> TabEntryDto {
    TabEntryDto {
        handle: handle.0,
        state: entry.state,
        payload: serde_json::to_value(&entry.tab)
            .unwrap_or(serde_json::Value::Null),
    }
}

// --- Tree ↔ TileDto bridge -------------------------------------------------

/// Serialise an `egui_tiles::Tree<TabId>` into the workbench's
/// portable [`TreeDto`] form.
pub(crate) fn tree_to_dto(tree: &egui_tiles::Tree<TabId>) -> TreeDto {
    TreeDto {
        root: tree.root.and_then(|id| tile_to_dto(tree, id)),
    }
}

fn tile_to_dto(tree: &egui_tiles::Tree<TabId>, id: egui_tiles::TileId) -> Option<TileDto> {
    match tree.tiles.get(id)? {
        egui_tiles::Tile::Pane(handle) => Some(TileDto::Pane { handle: handle.0 }),
        egui_tiles::Tile::Container(egui_tiles::Container::Tabs(tabs)) => {
            let children: Vec<TileDto> = tabs
                .children
                .iter()
                .filter_map(|cid| tile_to_dto(tree, *cid))
                .collect();
            let active = tabs
                .active
                .and_then(|aid| tabs.children.iter().position(|c| *c == aid));
            Some(TileDto::Tabs { children, active })
        }
        egui_tiles::Tile::Container(egui_tiles::Container::Linear(lin)) => {
            let children: Vec<TileDto> = lin
                .children
                .iter()
                .filter_map(|cid| tile_to_dto(tree, *cid))
                .collect();
            let shares: Vec<f32> = lin
                .children
                .iter()
                .map(|cid| lin.shares.iter().find(|(t, _)| *t == cid).map(|(_, s)| *s).unwrap_or(1.0))
                .collect();
            Some(TileDto::Linear {
                dir: lin.dir.into(),
                children,
                shares,
            })
        }
        egui_tiles::Tile::Container(egui_tiles::Container::Grid(grid)) => {
            let children: Vec<TileDto> = grid
                .children()
                .filter_map(|cid| tile_to_dto(tree, *cid))
                .collect();
            Some(TileDto::Grid {
                layout: grid.layout.into(),
                children,
                col_shares: grid.col_shares.clone(),
                row_shares: grid.row_shares.clone(),
            })
        }
    }
}

/// Rebuild an `egui_tiles::Tree<TabId>` from its persisted form.
/// `id` is the egui persistence key for the new tree.
pub(crate) fn tree_from_dto(
    dto: &TreeDto,
    id: egui::Id,
) -> egui_tiles::Tree<TabId> {
    let mut tree = egui_tiles::Tree::empty(id);
    let root = dto.root.as_ref().map(|t| insert_tile(&mut tree, t));
    tree.root = root;
    tree
}

fn insert_tile(
    tree: &mut egui_tiles::Tree<TabId>,
    dto: &TileDto,
) -> egui_tiles::TileId {
    match dto {
        TileDto::Pane { handle } => tree.tiles.insert_pane(TabId(*handle)),
        TileDto::Tabs { children, active } => {
            let child_ids: Vec<_> = children.iter().map(|c| insert_tile(tree, c)).collect();
            let tabs_id = tree.tiles.insert_tab_tile(child_ids.clone());
            if let Some(idx) = *active
                && let Some(active_id) = child_ids.get(idx)
                && let Some(egui_tiles::Tile::Container(egui_tiles::Container::Tabs(tabs))) =
                    tree.tiles.get_mut(tabs_id)
            {
                tabs.set_active(*active_id);
            }
            tabs_id
        }
        TileDto::Linear { dir, children, shares } => {
            let child_ids: Vec<_> = children.iter().map(|c| insert_tile(tree, c)).collect();
            let container = egui_tiles::Linear::new((*dir).into(), child_ids.clone());
            let lin_id = tree.tiles.insert_container(container);
            if !shares.is_empty()
                && let Some(egui_tiles::Tile::Container(egui_tiles::Container::Linear(lin))) =
                    tree.tiles.get_mut(lin_id)
            {
                for (child, share) in child_ids.iter().zip(shares.iter()) {
                    lin.shares.set_share(*child, *share);
                }
            }
            lin_id
        }
        TileDto::Grid {
            layout,
            children,
            col_shares,
            row_shares,
        } => {
            let child_ids: Vec<_> = children.iter().map(|c| insert_tile(tree, c)).collect();
            let mut container = egui_tiles::Grid::new(child_ids);
            container.layout = (*layout).into();
            container.col_shares = col_shares.clone();
            container.row_shares = row_shares.clone();
            tree.tiles.insert_container(container)
        }
    }
}

// Internal accessors so `persistence` doesn't need crate-public fields
// on the area types.
impl<Tab: Document, Mode: Clone + Eq + Hash + 'static> Workbench<Tab, Mode> {
    pub(crate) fn panel_area_entries(
        &self,
    ) -> impl Iterator<Item = (TabId, &TabEntry<Tab>)> {
        self.panel_area.inner.iter_entries()
    }
    pub(crate) fn panel_area_tree_clone(&self) -> egui_tiles::Tree<TabId> {
        self.panel_area.inner.tree_clone()
    }
    pub(crate) fn panel_area_replace(
        &mut self,
        tree: egui_tiles::Tree<TabId>,
        entries: HashMap<TabId, TabEntry<Tab>>,
    ) {
        self.panel_area.inner.replace_tree(tree, entries);
    }
}
