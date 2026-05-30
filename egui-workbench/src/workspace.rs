//! Top-level `Workbench` coordinator + `WorkbenchLayout`.
//!
//! Owns the activity bar, side bars, editor area, panel area, and
//! status bar. Renders them as a layered panel stack each frame.
//! See `DESIGN.md` for the panel order rationale.

use std::hash::Hash;
use std::marker::PhantomData;

use egui::Frame;
use egui_tiles::TileId;

use crate::activity_bar::ActivityBar;
use crate::behavior::Host;
use crate::editor_area::EditorArea;
use crate::panel_area::PanelArea;
use crate::side_bar::{SideBar, SideBarRole, Side, show_side_bar};
use crate::tab::{Document, State};

/// Stable identifier for a tab payload inside a [`Workbench`].
///
/// Allocated monotonically by [`Workbench::next_handle`]; only reused after
/// the referenced tab has been removed from both the editor tree and the
/// panel tree. Survives across tree reorderings, splits, and moves between
/// groups.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TabId(pub u64);

impl TabId {
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Identifier for an editor group: an `egui_tiles::TileId` referring to a
/// `Tabs` container in either the editor tree or the panel tree.
///
/// Note: unlike [`TabId`], this is not guaranteed stable across full layout
/// reloads (deserializing a layout allocates fresh `TileId`s).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GroupId(pub TileId);

/// Status bar — thin horizontal strip with appendable cells. See `DESIGN.md`.
/// Phase E will populate the cell rendering; Phase C gives hosts a place to
/// draw via `Host::status_bar_ui`.
pub struct StatusBar {
    pub visible: bool,
}

impl Default for StatusBar {
    fn default() -> Self {
        Self { visible: true }
    }
}

/// Build a 14×14 image from a static SVG byte blob for built-in workbench
/// chrome (the panel maximise/minimise toggle, etc.). `uri` must be a stable,
/// per-asset `bytes://…` key so egui caches the decoded texture across frames.
///
/// `egui_extras::install_image_loaders` must be called by the host before any
/// of these render — the workbench example sets that up once at startup.
pub(crate) fn chrome_icon(uri: &'static str, bytes: &'static [u8]) -> egui::Image<'static> {
    egui::Image::new(egui::ImageSource::Bytes {
        uri: uri.into(),
        bytes: egui::load::Bytes::Static(bytes),
    })
    .fit_to_exact_size(egui::vec2(14.0, 14.0))
}

/// The "collapse / restore panel size" chevron used by the panel-area toggle
/// and the editor-area "all tabs" dropdown trigger.
pub(crate) fn chevron_down() -> egui::Image<'static> {
    static BYTES: &[u8] = include_bytes!("../assets/chevron_down.svg");
    chrome_icon("bytes://egui_workbench-icon-chevron_down.svg", BYTES)
}

/// Which direction a new editor-group split runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDir {
    Left,
    Right,
    Up,
    Down,
}

/// Where to place a newly-opened tab.
#[derive(Clone, Copy, Debug, Default)]
pub enum GroupTarget {
    /// Open in the currently-focused group (default).
    #[default]
    Focused,
    /// Create a new group by splitting the focused one.
    NewSplit(SplitDir),
    /// Open in the named group.
    Specific(GroupId),
}

/// Options for [`Workbench::open_tab`].
#[derive(Clone, Debug)]
pub struct OpenTabOptions {
    /// State (Regular / Preview / Pinned). Default Regular.
    pub state: State,
    /// Focus the newly opened tab. Default `true`.
    pub focus: bool,
    /// Target group. Default `Focused`.
    pub group: GroupTarget,
}

impl Default for OpenTabOptions {
    fn default() -> Self {
        Self {
            state: State::Regular,
            focus: true,
            group: GroupTarget::Focused,
        }
    }
}

/// The single entry point a host uses to embed a workbench.
pub struct Workbench<Tab: Document, Mode: Clone + Eq + Hash + 'static> {
    pub activity_bar: ActivityBar<Mode>,
    pub primary_side_bar: SideBar,
    /// Primary side-panel region as a VSCode-style accordion of
    /// collapsible feature sections. The activity bar switches/focuses
    /// sections here; headers drag to reorder. [feature-multi-region-sidebar]
    pub primary_panels: crate::side_panel_stack::SidePanelStack<Mode>,
    pub secondary_side_bar: SideBar,
    pub editor_area: EditorArea<Tab>,
    pub panel_area: PanelArea<Tab>,
    pub status_bar: StatusBar,
    pub(crate) next_handle: u64,
    /// Set to true any time the layout structure changes — useful for
    /// hosts that persist layout on-change.
    pub(crate) dirty: bool,
    _mode: PhantomData<Mode>,
}

impl<Tab: Document, Mode: Clone + Eq + Hash + 'static> Default for Workbench<Tab, Mode> {
    fn default() -> Self {
        Self {
            activity_bar: ActivityBar::default(),
            primary_side_bar: SideBar::new(Side::Left),
            primary_panels: crate::side_panel_stack::SidePanelStack::new(),
            secondary_side_bar: SideBar {
                side: Side::Right,
                visible: false,
                ..SideBar::default()
            },
            editor_area: EditorArea::new(),
            panel_area: PanelArea::new(),
            status_bar: StatusBar::default(),
            next_handle: 1,
            dirty: false,
            _mode: PhantomData,
        }
    }
}

impl<Tab: Document, Mode: Clone + Eq + Hash + 'static> Workbench<Tab, Mode> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a tab with the given options. Returns the stable handle.
    ///
    /// Preview replacement: when `opts.state == State::Preview`, the
    /// target editor group's existing Preview tab (if any) is closed
    /// first. This enforces the "single ephemeral preview per group"
    /// semantics — the same one previously-typed file inhabits the
    /// preview slot until you explicitly promote it.
    pub fn open_tab(&mut self, tab: Tab, opts: &OpenTabOptions) -> TabId {
        // Preview replacement.
        if opts.state == State::Preview
            && let Some(group) = self.editor_area.focused_group
            && let Some(existing) = self.editor_area.preview_handle_in_group(group)
        {
            self.editor_area.remove_tab(existing);
            self.editor_area.entries.remove(&existing);
        }
        let handle = TabId(self.next_handle);
        self.next_handle = self.next_handle.saturating_add(1);
        // Phase C ignores `group == NewSplit / Specific` — both fall back
        // to "open in focused group". Phase D wires the split path.
        let _ = opts.group;
        self.editor_area
            .insert_tab(handle, tab, opts.state, opts.focus);
        self.dirty = true;
        handle
    }

    /// Open a tab in the bottom panel area. Same semantics as
    /// [`Self::open_tab`] but targets the panel tree.
    pub fn open_panel_tab(&mut self, tab: Tab, opts: &OpenTabOptions) -> TabId {
        let handle = TabId(self.next_handle);
        self.next_handle = self.next_handle.saturating_add(1);
        let _ = opts.group;
        self.panel_area.inner.insert_tab(handle, tab, opts.state, opts.focus);
        // Panel becomes visible when something is added (auto-show
        // counterpart to the empty-panel auto-hide).
        self.panel_area.visible = true;
        self.dirty = true;
        handle
    }

    /// Flip a tab's pinned/regular state.
    pub fn pin_tab(&mut self, handle: TabId, pinned: bool) {
        let target = if pinned { State::Pinned } else { State::Regular };
        if self.editor_area.entries.contains_key(&handle) {
            self.editor_area.set_state(handle, target);
        } else if self.panel_area.inner.entries.contains_key(&handle) {
            self.panel_area.inner.set_state(handle, target);
        }
        self.dirty = true;
    }

    /// Toggle a tab between Pinned and Regular.
    pub fn toggle_pin(&mut self, handle: TabId) {
        let current = self
            .editor_area
            .state(handle)
            .or_else(|| self.panel_area.state(handle));
        let next = !matches!(current, Some(State::Pinned));
        self.pin_tab(handle, next);
    }

    /// Promote a Preview tab to Regular.
    pub fn promote_preview(&mut self, handle: TabId) {
        if let Some(state) = self.editor_area.state(handle)
            && state == State::Preview
        {
            self.editor_area.set_state(handle, State::Regular);
            self.dirty = true;
        } else if let Some(state) = self.panel_area.state(handle)
            && state == State::Preview
        {
            self.panel_area.inner.set_state(handle, State::Regular);
            self.dirty = true;
        }
    }

    fn promote_preview_with(
        &mut self,
        handle: TabId,
        behavior: &mut impl Host<Tab, Mode>,
    ) {
        let was_preview = self.editor_area.state(handle) == Some(State::Preview);
        self.promote_preview(handle);
        if was_preview
            && let Some(tab) = self.editor_area.entries.get(&handle).map(|e| &e.tab)
        {
            behavior.on_preview_promoted(tab);
        }
    }

    /// Close every tab in `handle`'s group except `handle` itself and
    /// any pinned tabs.
    pub fn close_others(&mut self, except: TabId) {
        let Some(group) = crate::internal::tree_adapter::find_group_of(
            &self.editor_area.tree,
            except,
        ) else {
            return;
        };
        let handles = crate::internal::tree_adapter::handles_in_group(&self.editor_area.tree, group);
        for h in handles {
            if h == except {
                continue;
            }
            if self.editor_area.state(h) == Some(State::Pinned) {
                continue;
            }
            self.editor_area.remove_tab(h);
            self.editor_area.entries.remove(&h);
        }
        self.dirty = true;
    }

    /// Close every tab in `handle`'s group that appears strictly to the
    /// right of `handle`. Skips pinned tabs.
    pub fn close_to_right(&mut self, after: TabId) {
        let Some(group) = crate::internal::tree_adapter::find_group_of(
            &self.editor_area.tree,
            after,
        ) else {
            return;
        };
        let handles = crate::internal::tree_adapter::handles_in_group(&self.editor_area.tree, group);
        let mut past = false;
        for h in handles {
            if !past {
                if h == after {
                    past = true;
                }
                continue;
            }
            if self.editor_area.state(h) == Some(State::Pinned) {
                continue;
            }
            self.editor_area.remove_tab(h);
            self.editor_area.entries.remove(&h);
        }
        self.dirty = true;
    }

    /// Close every tab in every editor group. Skips pinned tabs.
    pub fn close_all(&mut self) {
        let handles = crate::internal::tree_adapter::all_handles(&self.editor_area.tree);
        for h in handles {
            if self.editor_area.state(h) == Some(State::Pinned) {
                continue;
            }
            self.editor_area.remove_tab(h);
            self.editor_area.entries.remove(&h);
        }
        self.dirty = true;
    }

    /// Close the tab with the given handle. Returns `true` if a tab was
    /// removed. Bypasses [`Host::on_tab_close`].
    pub fn close_tab(&mut self, handle: TabId) -> bool {
        let removed = self.editor_area.remove_tab(handle);
        if removed {
            self.dirty = true;
        }
        removed
    }

    /// Iterate over open tabs in the editor area.
    pub fn iter_tabs(&self) -> impl Iterator<Item = (TabId, &Tab)> {
        self.editor_area.iter_tabs()
    }

    /// Currently focused editor group, if any.
    pub fn focused_group(&self) -> Option<GroupId> {
        self.editor_area.focused_group()
    }

    /// Handle of the active tab inside the focused editor group, if any.
    /// Hosts use this to detect "user clicked a tab in the strip" by
    /// snapshotting before [`Self::ui`] and comparing after.
    pub fn active_handle(&self) -> Option<TabId> {
        let group = self.editor_area.focused_group()?;
        crate::internal::tree_adapter::active_handle_in_group(
            &self.editor_area.tree,
            group.0,
        )
    }

    /// Programmatically activate `handle` in its enclosing tab group.
    /// Hosts call this when navigation logic outside the workbench
    /// (browser-style back/forward, command palette "jump to tab",
    /// etc.) needs the active pane to follow without simulating a
    /// click. Returns `true` if the active selection actually changed.
    pub fn set_active(&mut self, handle: TabId) -> bool {
        let changed = self.editor_area.set_active(handle);
        if changed {
            self.dirty = true;
        }
        changed
    }

    /// Programmatically split the focused group.
    ///
    /// In v0.1 users obtain splits via the existing drag-to-edge gesture
    /// supplied by `egui_tiles`. The programmatic command form is
    /// deferred to v0.2 — see CHANGELOG.md.
    pub const fn split_active_group(&mut self, _dir: SplitDir) {
        // Deferred to v0.2 (see CHANGELOG). Drag-to-edge already works.
    }

    /// Toggle whether the primary side bar is visible.
    pub const fn toggle_primary_side_bar(&mut self) {
        self.primary_side_bar.toggle();
    }

    /// Toggle whether the secondary side bar is visible.
    pub const fn toggle_secondary_side_bar(&mut self) {
        self.secondary_side_bar.toggle();
    }

    /// Toggle whether the panel area is visible.
    pub const fn toggle_panel_area(&mut self) {
        self.panel_area.toggle();
    }

    /// Set which edge the primary side bar lives on.
    pub const fn set_side_bar_side(&mut self, side: Side) {
        self.primary_side_bar.side = side;
        self.secondary_side_bar.side = match side {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        };
        self.activity_bar.side = side;
    }

    // === Keyboard navigation hooks ===
    //
    // These methods only manipulate workbench state — they do not bind
    // any keys. Hosts wire them to their own keybinding system. The
    // suggested default chords match the common IDE convention
    // (`Ctrl+1` for the first group, etc.).

    /// Editor-group tiles in left-to-right, top-to-bottom traversal
    /// order. Used by [`Self::focus_group`] and friends. Containers
    /// are visited in the order their children appear in the parent
    /// (so left-to-right for `Horizontal`, top-to-bottom for
    /// `Vertical`), giving stable group indices across frames.
    fn group_traversal(&self) -> Vec<GroupId> {
        fn walk<P>(
            tree: &egui_tiles::Tree<P>,
            id: egui_tiles::TileId,
            out: &mut Vec<egui_tiles::TileId>,
        ) {
            match tree.tiles.get(id) {
                Some(egui_tiles::Tile::Container(egui_tiles::Container::Tabs(_))) => out.push(id),
                Some(egui_tiles::Tile::Container(egui_tiles::Container::Linear(lin))) => {
                    for child in &lin.children {
                        walk(tree, *child, out);
                    }
                }
                Some(egui_tiles::Tile::Container(egui_tiles::Container::Grid(grid))) => {
                    for child in grid.children() {
                        walk(tree, *child, out);
                    }
                }
                _ => {}
            }
        }
        let tree = &self.editor_area.tree;
        let mut out = Vec::new();
        if let Some(root) = tree.root {
            walk(tree, root, &mut out);
        }
        out.into_iter().map(GroupId).collect()
    }

    /// Focus the Nth editor group in left-to-right, top-to-bottom
    /// traversal order. Suggested chord: `Ctrl+1` .. `Ctrl+9`.
    pub fn focus_group(&mut self, idx: usize) {
        let groups = self.group_traversal();
        if let Some(g) = groups.get(idx) {
            self.editor_area.set_focused_group(*g);
        }
    }

    /// Focus the next editor group, wrapping at the end.
    /// Suggested chord: `Ctrl+K Ctrl+RightArrow`.
    pub fn focus_next_group(&mut self) {
        let groups = self.group_traversal();
        if groups.is_empty() {
            return;
        }
        let current = self
            .editor_area
            .focused_group()
            .and_then(|cur| groups.iter().position(|g| g.0 == cur.0))
            .unwrap_or(0);
        let next = (current + 1) % groups.len();
        self.editor_area.set_focused_group(groups[next]);
    }

    /// Focus the previous editor group, wrapping at the start.
    /// Suggested chord: `Ctrl+K Ctrl+LeftArrow`.
    pub fn focus_prev_group(&mut self) {
        let groups = self.group_traversal();
        if groups.is_empty() {
            return;
        }
        let current = self
            .editor_area
            .focused_group()
            .and_then(|cur| groups.iter().position(|g| g.0 == cur.0))
            .unwrap_or(0);
        let prev = (current + groups.len() - 1) % groups.len();
        self.editor_area.set_focused_group(groups[prev]);
    }

    /// Advance the active tab within the focused group.
    /// Suggested chord: `Ctrl+Tab`.
    pub fn next_tab_in_group(&mut self) {
        self.cycle_active_in_focused_group(1);
    }

    /// Step back to the previous tab in the focused group.
    /// Suggested chord: `Ctrl+Shift+Tab`.
    pub fn prev_tab_in_group(&mut self) {
        self.cycle_active_in_focused_group(-1);
    }

    fn cycle_active_in_focused_group(&mut self, delta: i32) {
        let Some(group) = self.editor_area.focused_group else { return };
        if let Some(egui_tiles::Tile::Container(egui_tiles::Container::Tabs(tabs))) =
            self.editor_area.tree.tiles.get_mut(group)
        {
            let n = tabs.children.len();
            if n == 0 {
                return;
            }
            let cur_pos = tabs
                .active
                .and_then(|a| tabs.children.iter().position(|c| *c == a))
                .unwrap_or(0) as i32;
            let new_pos = ((cur_pos + delta).rem_euclid(n as i32)) as usize;
            let new_active = tabs.children[new_pos];
            tabs.set_active(new_active);
            self.dirty = true;
        }
    }

    /// Close the currently active tab in the focused group, honouring
    /// the host's [`Host::on_tab_close`] veto.
    /// Suggested chord: `Ctrl+W`.
    pub fn close_active(&mut self, behavior: &mut impl Host<Tab, Mode>) {
        let Some(group) = self.editor_area.focused_group else { return };
        let Some(handle) = crate::internal::tree_adapter::active_handle_in_group(
            &self.editor_area.tree,
            group,
        ) else {
            return;
        };
        let allow = self
            .editor_area
            .entries
            .get(&handle)
            .map(|e| behavior.on_tab_close(&e.tab))
            .unwrap_or(true);
        if allow {
            self.close_tab(handle);
        }
    }

    /// Has the layout changed since the last `clear_dirty` call?
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clear the "layout changed" flag. Hosts call this after they've
    /// persisted the latest layout to storage.
    pub const fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Render the full workbench. Calls into the host `behavior` at
    /// each customisation point.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        behavior: &mut impl Host<Tab, Mode>,
    ) {
        let theme = behavior.theme(&ctx.style());

        // 1) Status bar — declared FIRST so it claims the full bottom
        //    strip across the whole viewport before any SidePanel can
        //    grab its column. egui panels claim space in declaration
        //    order: a `SidePanel::left` declared first takes the entire
        //    height from top toolbar to the window bottom, leaving the
        //    subsequent `TopBottomPanel::bottom` boxed in between the
        //    side bars (only as wide as the central pane). Putting the
        //    status bar at the top of this fn makes the side bars stop
        //    at `status_bar.top` instead — the bar then spans
        //    activity-bar → secondary-side-bar full width.
        if self.status_bar.visible {
            egui::TopBottomPanel::bottom("egui_workbench::status_bar")
                .resizable(false)
                .exact_height(22.0)
                .show(ctx, |ui| {
                    behavior.status_bar_ui(ui);
                });
        }

        // 2) Activity bar — fixed narrow strip on the leading edge.
        if self.activity_bar.is_visible() {
            let activity_panel = match self.activity_bar.side {
                Side::Left => egui::SidePanel::left("egui_workbench::activity_bar"),
                Side::Right => egui::SidePanel::right("egui_workbench::activity_bar"),
            };
            activity_panel
                .resizable(false)
                .exact_width(theme.activity_bar_width)
                // Zero inner margin so the icons sit flush against the
                // panel edges. The default `Frame::side_top_panel`
                // inner margin (~4-6 px) otherwise pushes the items
                // inward and exaggerates the visual gap between the
                // bar and the side bar next to it.
                .frame(
                    Frame::side_top_panel(&ctx.style())
                        .fill(theme.activity_bar_bg)
                        .inner_margin(0),
                )
                .show(ctx, |ui| {
                    let resp = self.activity_bar.show::<Tab, _>(ui, &theme, behavior);
                    if let Some(mode) = resp.clicked {
                        // VSCode switch semantics: clicking the focused
                        // section's icon hides the side bar; clicking any
                        // other icon switches that section into focus
                        // (replacing the focused section in place, never
                        // adding a split). The activity-bar highlight
                        // tracks the focused section.
                        let was_focused = self.primary_panels.focused.as_ref() == Some(&mode);
                        if was_focused && self.primary_side_bar.visible {
                            self.primary_side_bar.visible = false;
                        } else {
                            self.primary_panels.switch(mode);
                            self.primary_side_bar.visible = true;
                        }
                        self.activity_bar.active = self.primary_panels.focused.clone();
                    }
                    if let Some(mode) = resp.dropped_out {
                        // Dragged an activity icon into the window → add it
                        // as a new accordion section (VSCode "drag a view
                        // into the sidebar"). [feature-multi-region-sidebar]
                        self.primary_panels.add_section(mode);
                        self.activity_bar.active = self.primary_panels.focused.clone();
                        self.primary_side_bar.visible = true;
                    }
                });
        }

        // 3) Primary side bar — activity-driven, backed by the accordion
        //    `side_panel_stack`: one or more collapsible feature sections,
        //    each header a drag handle for reordering. A lone section
        //    looks identical to the old single side bar.
        self.show_primary_side_bar(ctx, &theme, behavior);

        // 4) Secondary side bar — fixed host content, independent of
        //    the active activity.
        show_side_bar::<Tab, _, _>(
            &mut self.secondary_side_bar,
            ctx,
            "egui_workbench::secondary_side_bar",
            &theme,
            behavior,
            None,
            SideBarRole::Secondary,
        );

        // 5) Panel area — bottom-docked tabbed surface.
        //    Auto-hide when empty (SPEC §14.2).
        if self.panel_area.inner.entries.is_empty() {
            self.panel_area.maximized = false;
        }
        let panel_visible = self.panel_area.visible && !self.panel_area.inner.entries.is_empty();
        // Maximised panel hides the editor area: render panel as
        // CentralPanel and skip the editor central panel below.
        if panel_visible && self.panel_area.maximized {
            egui::CentralPanel::default()
                .frame(Frame::central_panel(&ctx.style()).inner_margin(0))
                .show(ctx, |ui| {
                    self.show_panel_area(ui, behavior, &theme);
                });
            return;
        }

        if panel_visible {
            let panel_id = egui::Id::new("egui_workbench::panel_area");
            let panel_resp = egui::TopBottomPanel::bottom(panel_id)
                .resizable(true)
                .default_height(self.panel_area.height)
                .min_height(80.0)
                .show(ctx, |ui| {
                    self.show_panel_area(ui, behavior, &theme);
                });
            let new_height = panel_resp.response.rect.height();
            if (new_height - self.panel_area.height).abs() > 0.5 {
                self.panel_area.height = new_height.max(80.0);
                self.dirty = true;
            }
        }

        // 6) Central panel — the editor area.
        egui::CentralPanel::default()
            .frame(Frame::central_panel(&ctx.style()).inner_margin(0))
            .show(ctx, |ui| {
                self.show_editor_area(ui, behavior, &theme);
            });
    }

    /// Switch the primary side region to show `mode` as a focused
    /// section and ensure the side bar is visible. Used by hosts for the
    /// initial panel + single-panel programmatic switches.
    /// [feature-multi-region-sidebar]
    pub fn open_primary_panel(&mut self, mode: Mode) {
        self.primary_panels.switch(mode);
        self.activity_bar.active = self.primary_panels.focused.clone();
        self.primary_side_bar.visible = true;
    }

    /// Add `mode` as an additional accordion section below the focused
    /// one (the multi-panel path), making the side bar visible.
    pub fn add_primary_panel(&mut self, mode: Mode) {
        self.primary_panels.add_section(mode);
        self.activity_bar.active = self.primary_panels.focused.clone();
        self.primary_side_bar.visible = true;
    }

    /// Replace the open primary-panel set wholesale (layout restore).
    pub fn set_primary_panels(&mut self, modes: &[Mode]) {
        self.primary_panels.set_open(modes);
        self.activity_bar.active = self.primary_panels.focused.clone();
        self.primary_side_bar.visible = !self.primary_panels.is_empty();
    }

    /// Render the primary side bar: the accordion of feature sections
    /// inside a resizable `SidePanel`. A header click syncs the
    /// activity-bar highlight to the focused section.
    fn show_primary_side_bar(
        &mut self,
        ctx: &egui::Context,
        theme: &crate::theme::Palette,
        behavior: &mut impl Host<Tab, Mode>,
    ) {
        let bar = &mut self.primary_side_bar;
        if !bar.visible || self.primary_panels.is_empty() {
            return;
        }
        let frame = Frame::side_top_panel(&ctx.style()).fill(theme.side_bar_bg);
        let panel = match bar.side {
            crate::side_bar::Side::Left => egui::SidePanel::left("egui_workbench::primary_side_bar"),
            crate::side_bar::Side::Right => {
                egui::SidePanel::right("egui_workbench::primary_side_bar")
            }
        };
        let clamped = bar.width.clamp(bar.min_width, bar.max_width);
        let response = panel
            .frame(frame)
            .resizable(true)
            .default_width(clamped)
            .min_width(bar.min_width)
            .max_width(bar.max_width)
            .show(ctx, |ui| {
                if let Some(clicked) = self.primary_panels.ui::<Tab, _>(ui, theme, behavior) {
                    self.activity_bar.active = Some(clicked);
                }
            });
        let actual = response.response.rect.width();
        let new_width = actual.clamp(bar.min_width, bar.max_width);
        if (new_width - bar.width).abs() > 0.5 {
            bar.width = new_width;
        }
    }

    fn show_panel_area(
        &mut self,
        ui: &mut egui::Ui,
        behavior: &mut impl Host<Tab, Mode>,
        theme: &crate::theme::Palette,
    ) {
        // Top-right controls: maximize toggle + close.
        let mut toggle_max = false;
        let mut close_panel = false;
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("x").on_hover_text("Close panel").clicked() {
                    close_panel = true;
                }
                let (icon, hint) = if self.panel_area.maximized {
                    (chevron_down(), "Restore panel size")
                } else {
                    // `chevron_up` has a single call site, so it stays inline
                    // here rather than as a named helper (which would trip
                    // `clippy::single_call_fn`).
                    static UP_BYTES: &[u8] = include_bytes!("../assets/chevron_up.svg");
                    let up = chrome_icon(
                        "bytes://egui_workbench-icon-chevron_up.svg",
                        UP_BYTES,
                    );
                    (up, "Maximize panel")
                };
                if ui
                    .add(egui::Button::image(icon).small())
                    .on_hover_text(hint)
                    .clicked()
                {
                    toggle_max = true;
                }
            });
        });
        if toggle_max {
            self.panel_area.maximized = !self.panel_area.maximized;
            self.dirty = true;
        }
        if close_panel {
            self.panel_area.visible = false;
            self.panel_area.maximized = false;
            self.dirty = true;
            return;
        }

        if self.panel_area.inner.entries.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.weak("Panel is empty");
            });
            return;
        }

        let outcome = self.panel_area.inner.drive_ui(
            ui,
            behavior,
            theme,
            egui::Id::new("egui_workbench::panel_tree_placeholder"),
        );
        if outcome.dirty {
            self.dirty = true;
        }
        self.apply_drive_outcome(outcome, behavior, /* panel */ true);
    }

    fn show_editor_area(
        &mut self,
        ui: &mut egui::Ui,
        behavior: &mut impl Host<Tab, Mode>,
        theme: &crate::theme::Palette,
    ) {
        // Empty state: no tabs across no groups.
        if self.editor_area.entries.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.weak("No editor open");
            });
            return;
        }

        let outcome = self.editor_area.drive_ui(
            ui,
            behavior,
            theme,
            egui::Id::new("egui_workbench::editor_tree_placeholder"),
        );
        if outcome.dirty {
            self.dirty = true;
        }
        self.apply_drive_outcome(outcome, behavior, /* panel */ false);
    }

    /// Apply context-menu actions deferred from the tabbed-area frame.
    /// `panel == true` routes pins/promotions to the panel area; the
    /// editor-only "close others/right/all" actions are no-ops there.
    fn apply_drive_outcome(
        &mut self,
        outcome: crate::editor_area::DriveOutcome,
        behavior: &mut impl Host<Tab, Mode>,
        panel: bool,
    ) {
        for handle in outcome.pending_pin_toggles {
            self.toggle_pin(handle);
        }
        for handle in outcome.pending_promote {
            self.promote_preview_with(handle, behavior);
        }
        if panel {
            // Panel area does not surface close-others / close-to-right
            // / close-all from its context menu in v0.1 — every panel
            // tab is reachable individually. If we ever extend the
            // context menu there, route through the same paths the
            // editor uses (close_others / close_to_right / close_all).
            return;
        }
        if let Some(except) = outcome.pending_close_others {
            self.close_others(except);
        }
        if let Some(after) = outcome.pending_close_to_right {
            self.close_to_right(after);
        }
        if outcome.pending_close_all {
            self.close_all();
        }
    }
}

