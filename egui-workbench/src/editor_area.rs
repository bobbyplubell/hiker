//! Editor area — tabbed editor groups with splits. See `DESIGN.md`.
//!
//! Holds an `egui_tiles::Tree<TabId>` (groups + splits) plus a
//! payload map keyed by handle. A crate-private Behavior impl is built
//! per-frame and bridges egui_tiles back to the [`Host`]
//! supplied by the host.

use std::collections::HashMap;
use std::hash::Hash;
use std::marker::PhantomData;

use egui::{
    Align2, Color32, Rect, Response, Sense, Stroke, StrokeKind, TextStyle, Vec2, Visuals, vec2,
};
use egui_tiles::{
    Behavior as TilesBehavior, Container, EditAction, SimplificationOptions, TabState as TilesTabState,
    Tabs, Tile, TileId, Tiles, Tree, UiResponse,
};

use crate::behavior::Host;
use crate::workspace::{GroupId, TabId};
use crate::internal::tree_adapter;
use crate::tab::{Document, TabEntry, State, UiContext};
use crate::theme::Palette;

/// The central editor region. Owns the tab payload map and an
/// `egui_tiles::Tree<TabId>` describing the groups + splits.
pub struct EditorArea<Tab: Document> {
    pub(crate) tree: Tree<TabId>,
    pub(crate) entries: HashMap<TabId, TabEntry<Tab>>,
    /// Most recently focused group (the one user actions like
    /// "close active tab" target). May be `None` when the tree is empty.
    pub(crate) focused_group: Option<TileId>,
    /// Last-seen active tab handle per group. `tab_ui` consults this to
    /// scroll a freshly-activated tab into view exactly once (on the
    /// activation-change frame), so the horizontal tab-strip `ScrollArea`
    /// reveals a new/just-selected tab without fighting manual scrolling
    /// on subsequent frames.
    pub(crate) last_active_per_group: HashMap<TileId, TabId>,
    _marker: PhantomData<Tab>,
}

impl<Tab: Document> Default for EditorArea<Tab> {
    fn default() -> Self {
        Self {
            tree: Tree::empty(egui::Id::new("egui_workbench::editor_tree")),
            entries: HashMap::new(),
            focused_group: None,
            last_active_per_group: HashMap::new(),
            _marker: PhantomData,
        }
    }
}

impl<Tab: Document> EditorArea<Tab> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct an empty editor area whose underlying `egui_tiles::Tree`
    /// uses the given egui `Id` as its persistence key. Used by
    /// [`crate::PanelArea`] so the two trees don't collide in egui's
    /// data store.
    pub fn with_tree_id(id: egui::Id) -> Self {
        Self {
            tree: Tree::empty(id),
            entries: HashMap::new(),
            focused_group: None,
            last_active_per_group: HashMap::new(),
            _marker: PhantomData,
        }
    }

    /// Number of currently open tabs (across all groups).
    pub fn tab_count(&self) -> usize {
        self.entries.len()
    }

    /// Iterate `(handle, entry)` pairs — used by persistence.
    pub(crate) fn iter_entries(
        &self,
    ) -> impl Iterator<Item = (TabId, &TabEntry<Tab>)> {
        self.entries.iter().map(|(h, e)| (*h, e))
    }

    /// Clone the underlying tree — used by persistence.
    pub(crate) fn tree_clone(&self) -> Tree<TabId> {
        self.tree.clone()
    }

    /// For each `Tabs` container, return the first child `TabId`,
    /// in tree-iteration order. Public for tests and host-side
    /// inspection of pinned-first enforcement.
    pub fn leading_handle_per_group(&self) -> Vec<TabId> {
        let mut out = Vec::new();
        for (_id, tile) in self.tree.tiles.iter() {
            if let Tile::Container(Container::Tabs(tabs)) = tile
                && let Some(first_id) = tabs.children.first()
                && let Some(Tile::Pane(h)) = self.tree.tiles.get(*first_id)
            {
                out.push(*h);
            }
        }
        out
    }

    /// Replace the tree + entries wholesale. Used by persistence.
    pub(crate) fn replace_tree(
        &mut self,
        tree: Tree<TabId>,
        entries: HashMap<TabId, TabEntry<Tab>>,
    ) {
        self.tree = tree;
        self.entries = entries;
        self.focused_group = tree_adapter::first_tabs_container(&self.tree);
        // New tree → previously-seen active handles no longer apply; clear
        // so a restored active tab scrolls into view on its first frame.
        self.last_active_per_group.clear();
    }

    /// Iterate over open tabs.
    pub fn iter_tabs(&self) -> impl Iterator<Item = (TabId, &Tab)> {
        self.entries.iter().map(|(h, e)| (*h, &e.tab))
    }

    /// Look up a tab payload by handle.
    pub fn get(&self, handle: TabId) -> Option<&Tab> {
        self.entries.get(&handle).map(|e| &e.tab)
    }

    /// Mutable access to a tab payload.
    pub fn get_mut(&mut self, handle: TabId) -> Option<&mut Tab> {
        self.entries.get_mut(&handle).map(|e| &mut e.tab)
    }

    pub fn state(&self, handle: TabId) -> Option<State> {
        self.entries.get(&handle).map(|e| e.state)
    }

    pub(crate) fn set_state(&mut self, handle: TabId, state: State) {
        if let Some(entry) = self.entries.get_mut(&handle) {
            entry.state = state;
        }
    }

    /// Insert a tab into the tree, returning the `TileId` of the new pane.
    /// Caller supplies the handle (the workbench allocates them).
    pub(crate) fn insert_tab(
        &mut self,
        handle: TabId,
        tab: Tab,
        state: State,
        focus: bool,
    ) -> TileId {
        self.entries
            .insert(handle, TabEntry::new(tab, state, handle));

        let pane_id = self.tree.tiles.insert_pane(handle);

        // Find or create the destination Tabs container, then attach
        // the pane to it.
        let target_group = if let Some(group) = self.focused_group
            && self.is_tabs_container(group)
        {
            group
        } else if let Some(group) = tree_adapter::first_tabs_container(&self.tree) {
            group
        } else {
            // Empty tree: this pane becomes the only child of a fresh
            // root Tabs container.
            let new_root = self.tree.tiles.insert_tab_tile(vec![pane_id]);
            self.tree.root = Some(new_root);
            self.focused_group = Some(new_root);
            return pane_id;
        };

        if let Some(Tile::Container(Container::Tabs(tabs))) =
            self.tree.tiles.get_mut(target_group)
        {
            if !tabs.children.contains(&pane_id) {
                tabs.children.push(pane_id);
            }
            if focus {
                tabs.set_active(pane_id);
            }
        }
        self.focused_group = Some(target_group);
        pane_id
    }

    fn is_tabs_container(&self, id: TileId) -> bool {
        matches!(
            self.tree.tiles.get(id),
            Some(Tile::Container(Container::Tabs(_)))
        )
    }

    /// Remove a tab. Returns `true` if a tab was removed.
    pub(crate) fn remove_tab(&mut self, handle: TabId) -> bool {
        let Some(pane_id) = tree_adapter::find_pane_of(&self.tree, handle) else {
            self.entries.remove(&handle);
            return false;
        };
        self.tree.tiles.remove(pane_id);
        self.entries.remove(&handle);
        true
    }

    /// Return the handle of the (single) Preview tab inside `group`, if
    /// any. Used by [`crate::Workbench::open_tab`] to enforce the
    /// "one Preview tab per group" invariant.
    pub(crate) fn preview_handle_in_group(&self, group: TileId) -> Option<TabId> {
        let handles = tree_adapter::handles_in_group(&self.tree, group);
        handles
            .into_iter()
            .find(|h| self.state(*h) == Some(State::Preview))
    }

    /// Mark the given group as focused for future operations.
    pub const fn set_focused_group(&mut self, group: GroupId) {
        self.focused_group = Some(group.0);
    }

    pub fn focused_group(&self) -> Option<GroupId> {
        self.focused_group.map(GroupId)
    }

    /// Make `handle` the active tab inside its enclosing `Tabs`
    /// container and mark that container as the focused group. Hosts
    /// call this when navigation logic outside the workbench (e.g.
    /// browser-style back/forward) needs to swing the visible pane
    /// over to a tab the user didn't click. Returns `true` when the
    /// active tab actually changed; `false` if the handle is unknown
    /// or already active.
    pub fn set_active(&mut self, handle: TabId) -> bool {
        let Some(pane_id) = tree_adapter::find_pane_of(&self.tree, handle) else {
            return false;
        };
        let Some(group_id) = tree_adapter::find_group_of(&self.tree, handle) else {
            return false;
        };
        let mut changed = false;
        if let Some(Tile::Container(Container::Tabs(tabs))) =
            self.tree.tiles.get_mut(group_id)
        {
            if tabs.active != Some(pane_id) {
                tabs.set_active(pane_id);
                changed = true;
            }
        }
        self.focused_group = Some(group_id);
        changed
    }

    /// Locate the tab payload whose host id (as exposed by the host
    /// tab type) matches a predicate. Used by [`Workbench::set_active`]
    /// when the host knows its own id but not the workbench's
    /// `TabId`.
    pub fn handle_for<F: Fn(&Tab) -> bool>(&self, pred: F) -> Option<TabId> {
        self.entries
            .iter()
            .find(|(_, e)| pred(&e.tab))
            .map(|(h, _)| *h)
    }

    /// Drive one frame of the tabbed area: swap the tree out, run
    /// `egui_tiles` against an [`EditorBehavior`], drain the pending-state
    /// vectors that `tab_ui` populated, apply tab activations, drop
    /// payload entries for closed tabs, and run the pinned-first
    /// invariant pass. Returns a [`DriveOutcome`] describing the
    /// context-menu / focus actions the caller still needs to apply.
    ///
    /// Shared by [`crate::workspace::Workbench::show_editor_area`] and
    /// `show_panel_area` so the two tabbed surfaces don't drift.
    pub(crate) fn drive_ui<Mode, B>(
        &mut self,
        ui: &mut egui::Ui,
        behavior: &mut B,
        theme: &Palette,
        placeholder_id: egui::Id,
    ) -> DriveOutcome
    where
        Mode: Clone + Eq + Hash + 'static,
        B: Host<Tab, Mode> + ?Sized,
    {
        let placeholder = Tree::empty(placeholder_id);
        let mut tree = std::mem::replace(&mut self.tree, placeholder);
        let focused_group = self.focused_group;
        // Build a `pane TileId → parent Tabs-container TileId` map for
        // the whole tree so `EditorBehavior::pane_ui` can resolve a
        // pane's owning group without a per-frame tree walk.
        let mut pane_to_group: std::collections::HashMap<TileId, TileId> =
            std::collections::HashMap::new();
        for (tile_id, tile) in tree.tiles.iter() {
            if let Tile::Container(Container::Tabs(tabs)) = tile {
                for child in &tabs.children {
                    pane_to_group.insert(*child, *tile_id);
                }
            }
        }
        let mut adapter = EditorBehavior::<Tab, Mode, _> {
            entries: &mut self.entries,
            behavior,
            theme,
            dirty: false,
            pending_closes: Vec::new(),
            pending_close_others: None,
            pending_close_to_right: None,
            pending_close_all: false,
            pending_pin_toggles: Vec::new(),
            pending_promote: Vec::new(),
            pending_tab_activations: Vec::new(),
            focused_group,
            pane_to_group,
            pending_focus: None,
            last_active_per_group: &mut self.last_active_per_group,
            _mode: PhantomData,
        };
        tree.ui(&mut adapter, ui);

        // egui_tiles' tabs container makes the empty area of the tab
        // strip a drag handle for the whole container — drag the strip
        // background to drag the parent Tabs tile into a split. We
        // don't want that affordance: it triggers on near-misses when
        // the user just wants to click an empty bit of the strip, and
        // recombining split groups via that path lands in unintuitive
        // drop targets. Individual tabs are still draggable for reorder
        // and split through their own per-tab interaction. Cancel any
        // drag whose id resolves to a container tile.
        let dragged_id = ui.ctx().dragged_id();
        if let Some(dragged) = dragged_id {
            let tree_id = tree.id();
            for (tile_id, tile) in tree.tiles.iter() {
                if matches!(tile, Tile::Container(_))
                    && tile_id.egui_id(tree_id) == dragged
                {
                    ui.ctx().stop_dragging();
                    break;
                }
            }
        }

        let outcome = DriveOutcome {
            dirty: adapter.dirty,
            pending_close_others: adapter.pending_close_others.take(),
            pending_close_to_right: adapter.pending_close_to_right.take(),
            pending_close_all: adapter.pending_close_all,
            pending_pin_toggles: std::mem::take(&mut adapter.pending_pin_toggles),
            pending_promote: std::mem::take(&mut adapter.pending_promote),
        };
        let pending_closes = std::mem::take(&mut adapter.pending_closes);
        let activations = std::mem::take(&mut adapter.pending_tab_activations);
        let pending_focus = adapter.pending_focus.take();
        drop(adapter);

        if let Some(group) = pending_focus {
            self.focused_group = Some(group);
        }

        // Apply "all tabs" dropdown activations from the frame.
        for (group, child) in activations {
            if let Some(Tile::Container(Container::Tabs(tabs))) = tree.tiles.get_mut(group) {
                tabs.set_active(child);
            }
        }

        self.tree = tree;
        for handle in pending_closes {
            self.entries.remove(&handle);
        }

        // Post-frame: enforce pinned-first in each Tabs container.
        // egui_tiles permits pinned tabs to land after Regular ones; the
        // workbench forbids that ordering. Two passes so we don't hold
        // a `&mut Tiles` while looking at panes, and to keep this O(n)
        // without per-container scratch allocation.
        let entries = &self.entries;
        let state_for = |h: TabId| entries.get(&h).map(|e| e.state).unwrap_or_default();
        let container_ids: Vec<_> = self
            .tree
            .tiles
            .iter()
            .filter_map(|(id, tile)| match tile {
                Tile::Container(Container::Tabs(_)) => Some(*id),
                _ => None,
            })
            .collect();
        for cid in container_ids {
            let snapshot: Vec<(TileId, bool)> = {
                let Some(Tile::Container(Container::Tabs(tabs))) = self.tree.tiles.get(cid) else {
                    continue;
                };
                tabs.children
                    .iter()
                    .map(|child_id| {
                        let pinned = match self.tree.tiles.get(*child_id) {
                            Some(Tile::Pane(h)) => state_for(*h) == State::Pinned,
                            _ => false,
                        };
                        (*child_id, pinned)
                    })
                    .collect()
            };
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
            let mut reordered: Vec<TileId> =
                snapshot.iter().filter(|(_, p)| *p).map(|(id, _)| *id).collect();
            reordered.extend(snapshot.iter().filter(|(_, p)| !*p).map(|(id, _)| *id));
            if let Some(Tile::Container(Container::Tabs(tabs))) = self.tree.tiles.get_mut(cid) {
                tabs.children = reordered;
            }
        }

        // If the focused group was pruned by simplification, fall back to
        // the first remaining Tabs container.
        if let Some(group) = self.focused_group
            && self.tree.tiles.get(group).is_none()
        {
            self.focused_group = tree_adapter::first_tabs_container(&self.tree);
        }

        outcome
    }
}

/// Outcome of [`EditorArea::drive_ui`]. Carries the deferred user actions
/// that the workspace still needs to apply with cross-area awareness
/// (e.g., toggling a pin updates the workbench `dirty` flag).
pub(crate) struct DriveOutcome {
    /// `true` if `egui_tiles` reported any edit (drag, resize, activate).
    pub dirty: bool,
    pub pending_close_others: Option<TabId>,
    pub pending_close_to_right: Option<TabId>,
    pub pending_close_all: bool,
    pub pending_pin_toggles: Vec<TabId>,
    pub pending_promote: Vec<TabId>,
}

/// Whether the active tab should be scrolled into the tab strip's view
/// this frame. True only when `this_tab` is the active tab in its group
/// (`is_active`) and the group's previously-seen active handle (`prev`)
/// differs from it — i.e. activation just changed (new tab opened or a
/// different tab selected). Returning `false` once `prev == Some(this_tab)`
/// is what keeps the auto-scroll from firing every frame and overriding
/// the user's manual horizontal scrolling.
pub(crate) fn should_scroll_active_into_view(
    is_active: bool,
    prev: Option<TabId>,
    this_tab: TabId,
) -> bool {
    is_active && prev != Some(this_tab)
}

/// Per-frame `egui_tiles::Behavior` adapter. Holds borrows of the
/// payload map (so `pane_ui` can look up the tab) and the host
/// behavior. Constructed and dropped within a single `Tree::ui` call.
pub(crate) struct EditorBehavior<'a, Tab, Mode, B>
where
    Tab: Document,
    Mode: Clone + Eq + Hash + 'static,
    B: Host<Tab, Mode> + ?Sized,
{
    pub entries: &'a mut HashMap<TabId, TabEntry<Tab>>,
    pub behavior: &'a mut B,
    pub theme: &'a Palette,
    /// Set to `true` if any edit happened (drag, resize, tab select).
    /// The workbench uses this to mark its layout cache dirty.
    pub dirty: bool,
    /// Tabs the user asked to close this frame. Drained by the workbench.
    pub pending_closes: Vec<TabId>,
    /// Tabs to close (others / right-of) requested via context menu this
    /// frame. Drained by the workbench, which applies the close-others
    /// semantics (skip pinned).
    pub pending_close_others: Option<TabId>,
    pub pending_close_to_right: Option<TabId>,
    pub pending_close_all: bool,
    /// Tabs whose pinned state was toggled via context menu this frame.
    pub pending_pin_toggles: Vec<TabId>,
    /// Tabs whose Preview state should be promoted to Regular this frame
    /// (e.g. because the user explicitly clicked "Keep open").
    pub pending_promote: Vec<TabId>,
    /// `(group, child)` pairs to activate this frame — sourced from the
    /// "all tabs" dropdown rendered via `top_bar_right_ui`.
    pub pending_tab_activations: Vec<(TileId, TileId)>,
    /// Focused group computed from the current frame, if known.
    pub focused_group: Option<TileId>,
    /// Pane → owning Tabs container, precomputed before `tree.ui()` so
    /// `pane_ui` can resolve its parent group cheaply.
    pub pane_to_group: HashMap<TileId, TileId>,
    /// Persistent (cross-frame) last-seen active tab per group. `tab_ui`
    /// reads + updates this to scroll a just-activated tab into view only
    /// on the frame its activation changed — see
    /// [`EditorArea::last_active_per_group`].
    pub last_active_per_group: &'a mut HashMap<TileId, TabId>,
    /// Group the user clicked on this frame, if any — the workbench
    /// promotes this to `focused_group` post-frame so per-group commands
    /// (close-active, focus-next, etc.) target what the user just touched.
    pub pending_focus: Option<TileId>,
    pub _mode: PhantomData<Mode>,
}

impl<'a, Tab, Mode, B> TilesBehavior<TabId> for EditorBehavior<'a, Tab, Mode, B>
where
    Tab: Document,
    Mode: Clone + Eq + Hash + 'static,
    B: Host<Tab, Mode> + ?Sized,
{
    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        tile_id: TileId,
        handle: &mut TabId,
    ) -> UiResponse {
        let Some(entry) = self.entries.get_mut(handle) else {
            ui.weak("missing tab");
            return UiResponse::None;
        };
        let parent = self.pane_to_group.get(&tile_id).copied();
        let group = GroupId(parent.unwrap_or(tile_id));
        let focused = match (parent, self.focused_group) {
            (Some(p), Some(f)) => p == f,
            _ => false,
        };
        let ctx = UiContext {
            handle: *handle,
            group,
            focused,
            state: entry.state,
            _marker: PhantomData,
        };
        // Inset the pane content by `pane_content_inset` so it sits
        // visually inside the pane boundary rather than flush against
        // the tab strip / pane edges (also pulls content out from under
        // the `paint_on_top_of_tile` focused-group stroke). Tabs that
        // own their own edge-to-edge surface (`wants_pane_content_inset
        // -> false`, e.g. a markdown editor that paints its own bg)
        // skip the inset so the host bg doesn't show as a contrasting
        // strip around the editor's content area.
        let inset = if entry.tab.wants_pane_content_inset() {
            self.theme.pane_content_inset.max(self.theme.focused_group_border_width)
        } else {
            0.0
        };
        let inset_rect = ui.max_rect().shrink(inset);
        let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(inset_rect));
        self.behavior.pane_ui(&mut inner, &mut entry.tab, ctx);
        ui.allocate_rect(inner.min_rect(), egui::Sense::hover());
        UiResponse::None
    }

    fn tab_title_for_pane(&mut self, handle: &TabId) -> egui::WidgetText {
        let Some(entry) = self.entries.get(handle) else {
            return "(missing)".into();
        };
        let raw = entry.tab.title().text().to_string();
        let mut rich = egui::RichText::new(raw);
        if entry.state == State::Preview {
            rich = rich.italics();
        }
        rich.into()
    }

    fn is_tab_closable(&self, tiles: &Tiles<TabId>, tile_id: TileId) -> bool {
        // Closability is determined per-tab via Document::closable.
        match tiles.get(tile_id) {
            Some(Tile::Pane(h)) => self
                .entries
                .get(h)
                .map(|e| e.tab.closable())
                .unwrap_or(true),
            _ => true,
        }
    }

    fn tab_ui(
        &mut self,
        tiles: &mut Tiles<TabId>,
        ui: &mut egui::Ui,
        id: egui::Id,
        tile_id: TileId,
        state: &TilesTabState,
    ) -> Response {
        // Resolve the handle + flags before borrowing the painter.
        let (handle, tab_state, is_dirty, tooltip) = match tiles.get(tile_id) {
            Some(Tile::Pane(h)) => {
                let entry = self.entries.get(h);
                let s = entry.map(|e| e.state).unwrap_or_default();
                let d = entry.map(|e| e.tab.is_dirty()).unwrap_or(false);
                let t = entry.and_then(|e| e.tab.tooltip());
                (*h, s, d, t)
            }
            _ => {
                return ui.allocate_response(Vec2::ZERO, Sense::hover());
            }
        };

        // Pull glyph / dirty placement state.
        let pinned = tab_state == State::Pinned;
        let preview = tab_state == State::Preview;

        let text = self.tab_title_for_tile(tiles, tile_id);
        let font_id = TextStyle::Button.resolve(ui.style());
        let galley = text.into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, font_id.clone());

        let x_margin = self.tab_title_spacing(ui.visuals());
        let close_btn_size = Vec2::splat(self.close_button_outer_size());
        let close_btn_left_padding = 4.0;
        let pin_glyph_width = if pinned { 10.0 } else { 0.0 };
        // We reserve room for either dirty dot OR close button, not both
        // (dirty dot replaces close until hover). egui_tiles' close gating
        // is governed by `state.closable`.
        let right_slot_width = if state.closable {
            close_btn_left_padding + close_btn_size.x
        } else {
            0.0
        };

        let button_width =
            galley.size().x + 2.0 * x_margin + pin_glyph_width + right_slot_width;
        let (_, tab_rect) = ui.allocate_space(vec2(button_width, ui.available_height()));

        let tab_response = ui
            .interact(tab_rect, id, Sense::click_and_drag())
            .on_hover_cursor(self.tab_hover_cursor_icon());

        if ui.is_rect_visible(tab_rect) && !state.is_being_dragged {
            let bg_color = self.tab_bg_color(ui.visuals(), tiles, tile_id, state);
            let stroke = self.tab_outline_stroke(ui.visuals(), tiles, tile_id, state);
            ui.painter().rect(
                tab_rect.shrink(0.5),
                0.0,
                bg_color,
                stroke,
                StrokeKind::Inside,
            );
            if state.active {
                ui.painter().hline(
                    tab_rect.x_range(),
                    tab_rect.bottom(),
                    Stroke::new(stroke.width + 1.0, bg_color),
                );
            }

            let text_color = self.tab_text_color(ui.visuals(), tiles, tile_id, state);

            // Pin glyph: a small leading vertical bar so we avoid emoji
            // tofu. Painted, not text, to keep its size predictable.
            let inner = tab_rect.shrink2(vec2(x_margin, 0.0));
            let mut text_left = inner.left();
            if pinned {
                let cy = inner.center().y;
                let x = inner.left() + 2.5;
                ui.painter().rect_filled(
                    Rect::from_min_size(
                        egui::pos2(x, cy - 4.0),
                        vec2(2.5, 8.0),
                    ),
                    1.0,
                    text_color,
                );
                text_left += pin_glyph_width;
            }

            let text_position = Align2::LEFT_CENTER
                .align_size_within_rect(
                    galley.size(),
                    Rect::from_min_max(
                        egui::pos2(text_left, inner.top()),
                        inner.right_bottom(),
                    ),
                )
                .min;
            ui.painter().galley(text_position, galley, text_color);

            // Right-side: dirty dot replaces close X until hover.
            if state.closable {
                let slot = Align2::RIGHT_CENTER
                    .align_size_within_rect(close_btn_size, inner);
                let show_dirty_dot = is_dirty && !tab_response.hovered();
                if show_dirty_dot {
                    ui.painter()
                        .circle_filled(slot.center(), 3.5, text_color);
                } else {
                    let close_btn_id = ui.auto_id_with(("workbench_tab_close", tile_id));
                    let close_resp = ui
                        .interact(slot, close_btn_id, Sense::click_and_drag())
                        .on_hover_cursor(egui::CursorIcon::Default);
                    let visuals = ui.style().interact(&close_resp);
                    let rect = slot
                        .shrink(self.close_button_inner_margin())
                        .expand(visuals.expansion);
                    let stroke = visuals.fg_stroke;
                    ui.painter().line_segment([rect.left_top(), rect.right_bottom()], stroke);
                    ui.painter().line_segment([rect.right_top(), rect.left_bottom()], stroke);
                    if close_resp.clicked() && self.on_tab_close(tiles, tile_id) {
                        tiles.remove(tile_id);
                    }
                }
            }
        }

        // Tooltip from Document::tooltip().
        let tab_response = if let Some(tip) = tooltip {
            tab_response.on_hover_text(tip)
        } else {
            tab_response
        };

        // Middle-click closes (subject to host veto).
        if tab_response.clicked_by(egui::PointerButton::Middle)
            && self.on_tab_close(tiles, tile_id)
        {
            tiles.remove(tile_id);
        }

        // Track focused group: any interaction on a tab promotes its
        // owning group to the focused one for the frame.
        if (tab_response.clicked() || tab_response.drag_started())
            && let Some(parent) = tiles.parent_of(tile_id)
        {
            self.pending_focus = Some(parent);
        }

        // Preview promotion on double-click.
        if preview && tab_response.double_clicked() {
            self.pending_promote.push(handle);
        }

        // Context menu. Queue actions onto pending_* vecs and let the
        // workbench apply them after egui_tiles returns control.
        let mut close_self = false;
        let mut close_others = false;
        let mut close_to_right = false;
        let mut close_all = false;
        let mut toggle_pin = false;
        let mut promote = false;
        let host_extra_tab = self.entries.get(&handle).map(|e| e.tab.clone());
        let _ = tab_response.context_menu(|ui| {
            if ui.button("Close").clicked() {
                close_self = true;
                ui.close();
            }
            if ui.button("Close Others").clicked() {
                close_others = true;
                ui.close();
            }
            if ui.button("Close to the Right").clicked() {
                close_to_right = true;
                ui.close();
            }
            if ui.button("Close All").clicked() {
                close_all = true;
                ui.close();
            }
            ui.separator();
            let pin_label = if tab_state == State::Pinned { "Unpin" } else { "Pin" };
            if ui.button(pin_label).clicked() {
                toggle_pin = true;
                ui.close();
            }
            if preview && ui.button("Keep Open").clicked() {
                promote = true;
                ui.close();
            }
            if let Some(tab_ref) = host_extra_tab.as_ref() {
                ui.separator();
                self.behavior.tab_context_menu(ui, tab_ref);
            }
        });
        if close_self && self.on_tab_close(tiles, tile_id) {
            tiles.remove(tile_id);
        }
        if close_others {
            self.pending_close_others = Some(handle);
        }
        if close_to_right {
            self.pending_close_to_right = Some(handle);
        }
        if close_all {
            self.pending_close_all = true;
        }
        if toggle_pin {
            self.pending_pin_toggles.push(handle);
        }
        if promote {
            self.pending_promote.push(handle);
        }

        // Scroll a just-activated tab into view. egui_tiles lays out tabs
        // inside a horizontal `ScrollArea`; a newly-opened/selected tab can
        // land off-screen to the right. Calling `scroll_to_me` only on the
        // frame the group's active handle changed reveals it once without
        // overriding the user's manual horizontal scroll on later frames.
        if let Some(group) = self.pane_to_group.get(&tile_id).copied() {
            let prev = self.last_active_per_group.get(&group).copied();
            if should_scroll_active_into_view(state.active, prev, handle) {
                tab_response.scroll_to_me(None);
            }
            if state.active {
                self.last_active_per_group.insert(group, handle);
            }
        }

        self.on_tab_button(tiles, tile_id, tab_response)
    }

    fn on_tab_close(&mut self, tiles: &mut Tiles<TabId>, tile_id: TileId) -> bool {
        let handle = match tiles.get(tile_id) {
            Some(Tile::Pane(handle)) => *handle,
            _ => return true,
        };
        let allow = self
            .entries
            .get(&handle)
            .map(|e| self.behavior.on_tab_close(&e.tab))
            .unwrap_or(true);
        if allow {
            self.pending_closes.push(handle);
        }
        allow
    }

    fn simplification_options(&self) -> SimplificationOptions {
        SimplificationOptions {
            prune_empty_tabs: true,
            prune_empty_containers: true,
            prune_single_child_tabs: false,
            prune_single_child_containers: false,
            all_panes_must_have_tabs: true,
            join_nested_linear_containers: true,
        }
    }

    fn on_edit(&mut self, _edit_action: EditAction) {
        self.dirty = true;
    }

    fn paint_on_top_of_tile(
        &self,
        painter: &egui::Painter,
        _style: &egui::Style,
        tile_id: TileId,
        rect: Rect,
    ) {
        if self.focused_group == Some(tile_id) {
            let stroke =
                Stroke::new(self.theme.focused_group_border_width, self.theme.focused_group_border);
            painter.rect(
                rect.shrink(stroke.width / 2.0),
                0.0,
                Color32::TRANSPARENT,
                stroke,
                StrokeKind::Inside,
            );
        }
    }

    /// Theme-accented stroke around the active drop preview.
    fn drag_preview_stroke(&self, _visuals: &Visuals) -> Stroke {
        Stroke::new(2.0, self.theme.accent)
    }

    /// Translucent accent fill for the active drop zone.
    fn drag_preview_color(&self, _visuals: &Visuals) -> Color32 {
        let a = self.theme.accent;
        Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), 64)
    }

    /// Workbench-styled drop preview. Refines the default by:
    /// - Drawing the parent-group outline with the theme accent.
    /// - For thin previews (tab-strip insert), drawing a 2px insert bar
    ///   rather than a translucent rect.
    /// - For body drops, filling the target zone with a translucent
    ///   accent overlay and outlining it with a 2px stroke.
    fn paint_drag_preview(
        &self,
        visuals: &Visuals,
        painter: &egui::Painter,
        parent_rect: Option<Rect>,
        preview_rect: Rect,
    ) {
        let stroke = self.drag_preview_stroke(visuals);
        let fill = self.drag_preview_color(visuals);

        if let Some(parent) = parent_rect {
            // Faint outline on the parent container so users see the
            // group they're about to drop into.
            let parent_stroke =
                Stroke::new(1.0, self.theme.accent.gamma_multiply(0.5));
            painter.rect_stroke(parent, 1.0, parent_stroke, StrokeKind::Inside);
        }

        // Heuristic: a sliver-shaped preview rect means "insert between
        // tabs" (tab strip). Render as a solid accent bar.
        let is_insert_bar = preview_rect.width() <= 6.0 || preview_rect.height() <= 6.0;
        if is_insert_bar {
            painter.rect_filled(preview_rect, 1.0, self.theme.accent);
        } else {
            painter.rect(preview_rect, 2.0, fill, stroke, StrokeKind::Inside);
        }
    }

    /// "All tabs" dropdown on the right of every tab strip. Clicking
    /// activates the chosen tab.
    fn top_bar_right_ui(
        &mut self,
        tiles: &Tiles<TabId>,
        ui: &mut egui::Ui,
        tile_id: TileId,
        tabs: &Tabs,
        scroll_offset: &mut f32,
    ) {
        // Wheel scrolls the tab strip horizontally (without changing the
        // active tab). egui_tiles' inner horizontal ScrollArea would
        // otherwise ignore vertical wheel input. We consume the scroll
        // delta here — before the ScrollArea runs — and bake it into the
        // offset egui_tiles feeds to that ScrollArea.
        let tab_bar_rect = ui.max_rect();
        if ui.rect_contains_pointer(tab_bar_rect) {
            let delta = ui.input(|i| i.smooth_scroll_delta);
            let combined = delta.x + delta.y;
            if combined != 0.0 {
                *scroll_offset -= combined;
                ui.input_mut(|i| {
                    i.smooth_scroll_delta = egui::Vec2::ZERO;
                });
            }
        }

        let popup_id = ui.id().with(("workbench_all_tabs", tile_id));
        let button = ui
            .add(egui::Button::image(crate::workspace::chevron_down()).small())
            .on_hover_text("All tabs");
        let mut activate: Option<TileId> = None;
        egui::Popup::menu(&button)
            .id(popup_id)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClick)
            .show(|ui| {
                ui.set_min_width(160.0);
                for child_id in &tabs.children {
                    let title = self.tab_title_for_tile(tiles, *child_id);
                    let active = tabs.active == Some(*child_id);
                    let mut rich = egui::RichText::new(title.text().to_string());
                    if active {
                        rich = rich.strong();
                    }
                    if ui.selectable_label(active, rich).clicked() {
                        activate = Some(*child_id);
                    }
                }
            });
        if let Some(child) = activate {
            self.pending_tab_activations.push((tile_id, child));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{should_scroll_active_into_view, TabId};

    #[test]
    fn inactive_tab_never_scrolls() {
        // A non-active tab never scrolls itself into view, regardless of
        // what the group's previously-seen active handle was.
        assert!(!should_scroll_active_into_view(false, None, TabId(1)));
        assert!(!should_scroll_active_into_view(false, Some(TabId(2)), TabId(1)));
        assert!(!should_scroll_active_into_view(false, Some(TabId(1)), TabId(1)));
    }

    #[test]
    fn newly_active_tab_scrolls_once() {
        // First time we see this tab as active for its group (no prior, or
        // a different prior) → scroll it in.
        assert!(should_scroll_active_into_view(true, None, TabId(1)));
        assert!(should_scroll_active_into_view(true, Some(TabId(2)), TabId(1)));
    }

    #[test]
    fn active_tab_does_not_rescroll_on_steady_frames() {
        // Once the group's last-seen active handle equals this tab, the
        // activation hasn't changed — stay put so manual scroll wins.
        assert!(!should_scroll_active_into_view(true, Some(TabId(1)), TabId(1)));
    }
}

