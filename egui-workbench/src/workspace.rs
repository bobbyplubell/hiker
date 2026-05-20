//! Top-level `Workbench` coordinator + `WorkbenchLayout`.
//!
//! Owns the activity bar, side bars, editor area, panel area, and
//! status bar. Renders them as a layered panel stack each frame.
//! See `DESIGN.md` for the panel order rationale.

use std::hash::Hash;
use std::marker::PhantomData;

use egui::Frame;

use crate::activity_bar::{ActivityBar, show_activity_bar};
use crate::behavior::WorkbenchBehavior;
use crate::editor_area::EditorArea;
use crate::handle::{GroupHandle, TabHandle};
use crate::panel_area::PanelArea;
use crate::side_bar::{SideBar, SideBarRole, SideBarSide, show_side_bar};
use crate::status_bar::StatusBar;
use crate::tab::{DocumentTab, TabState};

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
    Specific(GroupHandle),
}

/// Options for [`Workbench::open_tab`].
#[derive(Clone, Debug)]
pub struct OpenTabOptions {
    /// State (Regular / Preview / Pinned). Default Regular.
    pub state: TabState,
    /// Focus the newly opened tab. Default `true`.
    pub focus: bool,
    /// Target group. Default `Focused`.
    pub group: GroupTarget,
}

impl Default for OpenTabOptions {
    fn default() -> Self {
        Self {
            state: TabState::Regular,
            focus: true,
            group: GroupTarget::Focused,
        }
    }
}

/// The single entry point a host uses to embed a workbench.
pub struct Workbench<Tab: DocumentTab, Mode: Clone + Eq + Hash + 'static> {
    pub activity_bar: ActivityBar<Mode>,
    pub primary_side_bar: SideBar,
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

impl<Tab: DocumentTab, Mode: Clone + Eq + Hash + 'static> Default for Workbench<Tab, Mode> {
    fn default() -> Self {
        Self {
            activity_bar: ActivityBar::default(),
            primary_side_bar: SideBar::new(SideBarSide::Left),
            secondary_side_bar: SideBar {
                side: SideBarSide::Right,
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

impl<Tab: DocumentTab, Mode: Clone + Eq + Hash + 'static> Workbench<Tab, Mode> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a tab with the given options. Returns the stable handle.
    ///
    /// Preview replacement: when `opts.state == TabState::Preview`, the
    /// target editor group's existing Preview tab (if any) is closed
    /// first. This enforces the "single ephemeral preview per group"
    /// semantics — the same one previously-typed file inhabits the
    /// preview slot until you explicitly promote it.
    pub fn open_tab(&mut self, tab: Tab, opts: OpenTabOptions) -> TabHandle {
        // Preview replacement.
        if opts.state == TabState::Preview
            && let Some(group) = self.editor_area.focused_group
            && let Some(existing) = self.editor_area.preview_handle_in_group(group)
        {
            self.editor_area.remove_tab(existing);
            self.editor_area.entries.remove(&existing);
        }
        let handle = TabHandle(self.next_handle);
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
    pub fn open_panel_tab(&mut self, tab: Tab, opts: OpenTabOptions) -> TabHandle {
        let handle = TabHandle(self.next_handle);
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
    pub fn pin_tab(&mut self, handle: TabHandle, pinned: bool) {
        let target = if pinned { TabState::Pinned } else { TabState::Regular };
        if self.editor_area.entries.contains_key(&handle) {
            self.editor_area.set_state(handle, target);
        } else if self.panel_area.inner.entries.contains_key(&handle) {
            self.panel_area.inner.set_state(handle, target);
        }
        self.dirty = true;
    }

    /// Toggle a tab between Pinned and Regular.
    pub fn toggle_pin(&mut self, handle: TabHandle) {
        let current = self
            .editor_area
            .state(handle)
            .or_else(|| self.panel_area.state(handle));
        let next = !matches!(current, Some(TabState::Pinned));
        self.pin_tab(handle, next);
    }

    /// Promote a Preview tab to Regular.
    pub fn promote_preview(&mut self, handle: TabHandle) {
        if let Some(state) = self.editor_area.state(handle)
            && state == TabState::Preview
        {
            self.editor_area.set_state(handle, TabState::Regular);
            self.dirty = true;
        } else if let Some(state) = self.panel_area.state(handle)
            && state == TabState::Preview
        {
            self.panel_area.inner.set_state(handle, TabState::Regular);
            self.dirty = true;
        }
    }

    fn promote_preview_with(
        &mut self,
        handle: TabHandle,
        behavior: &mut impl WorkbenchBehavior<Tab, Mode>,
    ) {
        let was_preview = self.editor_area.state(handle) == Some(TabState::Preview);
        self.promote_preview(handle);
        if was_preview
            && let Some(tab) = self.editor_area.entries.get(&handle).map(|e| &e.tab)
        {
            behavior.on_preview_promoted(tab);
        }
    }

    /// Close every tab in `handle`'s group except `handle` itself and
    /// any pinned tabs.
    pub fn close_others(&mut self, except: TabHandle) {
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
            if self.editor_area.state(h) == Some(TabState::Pinned) {
                continue;
            }
            self.editor_area.remove_tab(h);
            self.editor_area.entries.remove(&h);
        }
        self.dirty = true;
    }

    /// Close every tab in `handle`'s group that appears strictly to the
    /// right of `handle`. Skips pinned tabs.
    pub fn close_to_right(&mut self, after: TabHandle) {
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
            if self.editor_area.state(h) == Some(TabState::Pinned) {
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
            if self.editor_area.state(h) == Some(TabState::Pinned) {
                continue;
            }
            self.editor_area.remove_tab(h);
            self.editor_area.entries.remove(&h);
        }
        self.dirty = true;
    }

    /// Close the tab with the given handle. Returns `true` if a tab was
    /// removed. Bypasses [`WorkbenchBehavior::on_tab_close`].
    pub fn close_tab(&mut self, handle: TabHandle) -> bool {
        let removed = self.editor_area.remove_tab(handle);
        if removed {
            self.dirty = true;
        }
        removed
    }

    /// Iterate over open tabs in the editor area.
    pub fn iter_tabs(&self) -> impl Iterator<Item = (TabHandle, &Tab)> {
        self.editor_area.iter_tabs()
    }

    /// Currently focused editor group, if any.
    pub fn focused_group(&self) -> Option<GroupHandle> {
        self.editor_area.focused_group()
    }

    /// Programmatically split the focused group.
    ///
    /// In v0.1 users obtain splits via the existing drag-to-edge gesture
    /// supplied by `egui_tiles`. The programmatic command form is
    /// deferred to v0.2 — see CHANGELOG.md.
    pub fn split_active_group(&mut self, _dir: SplitDir) {
        // Deferred to v0.2 (see CHANGELOG). Drag-to-edge already works.
    }

    /// Toggle whether the primary side bar is visible.
    pub fn toggle_primary_side_bar(&mut self) {
        self.primary_side_bar.toggle();
    }

    /// Toggle whether the secondary side bar is visible.
    pub fn toggle_secondary_side_bar(&mut self) {
        self.secondary_side_bar.toggle();
    }

    /// Toggle whether the panel area is visible.
    pub fn toggle_panel_area(&mut self) {
        self.panel_area.toggle();
    }

    /// Set which edge the primary side bar lives on.
    pub fn set_side_bar_side(&mut self, side: SideBarSide) {
        self.primary_side_bar.side = side;
        self.secondary_side_bar.side = match side {
            SideBarSide::Left => SideBarSide::Right,
            SideBarSide::Right => SideBarSide::Left,
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
    /// order. Used by [`Self::focus_group`] and friends.
    fn group_traversal(&self) -> Vec<GroupHandle> {
        crate::internal::tree_adapter::groups_in_order(&self.editor_area.tree)
            .into_iter()
            .map(GroupHandle)
            .collect()
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
    /// the host's [`WorkbenchBehavior::on_tab_close`] veto.
    /// Suggested chord: `Ctrl+W`.
    pub fn close_active(&mut self, behavior: &mut impl WorkbenchBehavior<Tab, Mode>) {
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
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Clear the "layout changed" flag. Hosts call this after they've
    /// persisted the latest layout to storage.
    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Render the full workbench. Calls into the host `behavior` at
    /// each customisation point.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        behavior: &mut impl WorkbenchBehavior<Tab, Mode>,
    ) {
        let theme = behavior.theme(&ctx.style());

        // 1) Activity bar — fixed narrow strip on the leading edge.
        if self.activity_bar.is_visible() {
            let activity_panel = match self.activity_bar.side {
                SideBarSide::Left => egui::SidePanel::left("egui_workbench::activity_bar"),
                SideBarSide::Right => egui::SidePanel::right("egui_workbench::activity_bar"),
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
                    let resp = show_activity_bar::<Tab, _, _>(
                        &mut self.activity_bar,
                        ui,
                        &theme,
                        behavior,
                    );
                    if let Some(mode) = resp.clicked {
                        // Click semantics: same activity already active
                        // → toggle side bar visibility; otherwise →
                        // switch to that mode (and ensure side bar
                        // visible).
                        if self.activity_bar.active.as_ref() == Some(&mode) {
                            self.primary_side_bar.toggle();
                        } else {
                            self.activity_bar.active = Some(mode);
                            self.primary_side_bar.visible = true;
                        }
                    }
                });
        }

        // 2) Primary side bar — activity-driven.
        show_side_bar::<Tab, _, _>(
            &mut self.primary_side_bar,
            ctx,
            "egui_workbench::primary_side_bar",
            &theme,
            behavior,
            self.activity_bar.active.as_ref(),
            SideBarRole::Primary,
        );

        // 3) Secondary side bar — fixed host content, independent of
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

        // 4) Status bar — bottom strip; bottom-most panel so it sits
        //    below the panel area.
        if self.status_bar.visible {
            egui::TopBottomPanel::bottom("egui_workbench::status_bar")
                .resizable(false)
                .exact_height(22.0)
                .show(ctx, |ui| {
                    behavior.status_bar_ui(ui);
                });
        }

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
            let panel_resp = egui::TopBottomPanel::bottom("egui_workbench::panel_area")
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

    fn show_panel_area(
        &mut self,
        ui: &mut egui::Ui,
        behavior: &mut impl WorkbenchBehavior<Tab, Mode>,
        theme: &crate::WorkbenchTheme,
    ) {
        // Top-right controls: maximize toggle + close.
        let mut toggle_max = false;
        let mut close_panel = false;
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("x").on_hover_text("Close panel").clicked() {
                    close_panel = true;
                }
                let label = if self.panel_area.maximized { "v" } else { "^" };
                let hint = if self.panel_area.maximized {
                    "Restore panel size"
                } else {
                    "Maximize panel"
                };
                if ui.small_button(label).on_hover_text(hint).clicked() {
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
        behavior: &mut impl WorkbenchBehavior<Tab, Mode>,
        theme: &crate::WorkbenchTheme,
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
        behavior: &mut impl WorkbenchBehavior<Tab, Mode>,
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

