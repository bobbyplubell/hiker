//! Hiker ↔ `egui_workbench` bridge.
//!
//! Owns the host-side `Workbench` integration: the per-frame `Host`
//! adapter (activity modes are activity ids, sourced from the registry),
//! and the sync helper that
//! keeps `AppState::session.tabs` (the canonical list of open buffers
//! and pages) in lock-step with the workbench's editor area.
//!
//! Tab payloads inside the workbench are intentionally a thin
//! view-model ([`HikerWbTab`]). The real tab data lives in
//! `Session::tabs` / `Session::buffers`; the view-model carries just
//! enough cached state (label, dirty flag) for the workbench's
//! tab-strip rendering to be self-sufficient. We refresh it each frame
//! before calling `Workbench::ui`.

use std::sync::Arc;

use eframe::egui;
use egui_workbench::activity_bar::Item;
use egui_workbench::tab::Document;
use egui_workbench::workspace::OpenTabOptions;
use egui_workbench::tab::State;
use egui_workbench::tab::UiContext;
use egui_workbench::behavior::Host;
use egui_workbench::theme::Palette;
use crate::panels;
use crate::clusters;
use crate::state::AppState;
use crate::tab::{ChildSlot, TabId, TabKind};

/// View-model for an editor tab inside the workbench. Carries a
/// `TabId` pointer back into `Session::tabs` plus enough cached state
/// (label, dirty) that the workbench's tab strip can render without
/// consulting `AppState`.
#[derive(Clone)]
pub struct HikerWbTab {
    pub id: TabId,
    pub cached_label: String,
    pub cached_dirty: bool,
    /// True when the tab body draws its own edge-to-edge surface (the
    /// markdown editor + its bottom status strip) and shouldn't get the
    /// workbench's standard pane-content inset. Cached at sync time so
    /// `Document::wants_pane_content_inset` can answer without
    /// reaching back into `AppState`.
    pub edge_to_edge: bool,
}

impl Document for HikerWbTab {
    fn title(&self) -> egui::WidgetText {
        self.cached_label.clone().into()
    }
    fn is_dirty(&self) -> bool {
        self.cached_dirty
    }
    fn wants_pane_content_inset(&self) -> bool {
        !self.edge_to_edge
    }
}

// Fresh-workbench construction is inlined at `bootstrap::open_vault`'s
// `AppState` construction site; it was the only caller.

/// Sync `app.session.tabs` into the workbench's editor area. Opens
/// workbench tabs for newly-added `Session::tabs` entries, closes
/// workbench tabs whose backing `TabId` is gone, and refreshes the
/// cached label / dirty marker for survivors.
impl AppState {

pub fn sync_workbench_tabs(&mut self) {
    let app = self;
    // Snapshot the desired set first (immutable borrow of `session`).
    struct Want {
        id: TabId,
        label: String,
        dirty: bool,
        state: State,
        is_active: bool,
        edge_to_edge: bool,
    }
    let active_id = app.session.active_tab;
    let preview_id = app.session.preview_tab;
    let mut want: Vec<Want> = app
        .session
        .tabs
        .iter()
        .map(|t| {
            let dirty = t
                .buffer_path()
                .and_then(|p| app.session.buffers.get(p))
                .map(super::buffer::Buffer::is_dirty)
                .unwrap_or(false);
            let state = if Some(t.id) == preview_id || !t.sticky {
                State::Preview
            } else {
                State::Regular
            };
            Want {
                id: t.id,
                label: t.label(),
                dirty,
                state,
                is_active: Some(t.id) == active_id,
                // Tabs that paint their own full-bleed surface (the markdown
                // editor + status strip; the canvas with its header + board)
                // skip the workbench pane inset so the host bg doesn't frame
                // them with a contrasting border.
                edge_to_edge: matches!(
                    t.kind,
                    crate::tab::TabKind::Editor { .. }
                        | crate::tab::TabKind::Canvas { .. }
                        | crate::tab::TabKind::ChartBuilder { .. }
                ),
            }
        })
        .collect();

    // Index of TabId → workbench TabId for survivors.
    let mut existing: std::collections::HashMap<TabId, egui_workbench::workspace::TabId> =
        std::collections::HashMap::new();
    for (handle, tab) in app.workbench.iter_tabs() {
        existing.insert(tab.id, handle);
    }

    let want_ids: std::collections::HashSet<TabId> =
        want.iter().map(|w| w.id).collect();

    // Close workbench tabs that no longer have a Session::tabs entry.
    let stale: Vec<_> = existing
        .iter()
        .filter(|(id, _)| !want_ids.contains(id))
        .map(|(_, h)| *h)
        .collect();
    for h in stale {
        app.workbench.close_tab(h);
    }

    // Open / refresh tabs we want present.
    for w in want {
        match existing.get(&w.id).copied() {
            Some(handle) => {
                if let Some(t) = app.workbench.editor_area.get_mut(handle) {
                    t.cached_label = w.label;
                    t.cached_dirty = w.dirty;
                    t.edge_to_edge = w.edge_to_edge;
                }
            }
            None => {
                let tab = HikerWbTab {
                    id: w.id,
                    cached_label: w.label,
                    cached_dirty: w.dirty,
                    edge_to_edge: w.edge_to_edge,
                };
                app.workbench.open_tab(
                    tab,
                    &OpenTabOptions {
                        state: w.state,
                        focus: w.is_active,
                        ..OpenTabOptions::default()
                    },
                );
            }
        }
    }

    // Activation pass: drive the workbench's visible active tab from
    // `app.session.active_tab`. Without this, browser-style back/
    // forward (`editor_pane::nav_go`) updates `active_tab` but the
    // workbench's tab strip stays on whatever the user last clicked —
    // i.e. the visible pane doesn't actually navigate. The reverse
    // direction (workbench click → session.active_tab) is handled by
    // the focus-driven sync in `HikerWbBehavior::pane_ui` callers via
    // the existing tab-strip event plumbing.
    if let Some(active) = active_id
        && let Some(handle) = app.workbench.editor_area.handle_for(|t| t.id == active)
    {
        app.workbench.set_active(handle);
    }
}

/// Hiker's `TabId` of the active tab inside the given editor `group`, if any.
/// Resolves the workbench's active-tab *handle* in that group back to the
/// `HikerWbTab` payload's `TabId` — the FOLLOW seam a linked viz tab reads
/// each frame to learn "what note is active over there". status: tab-linking
pub fn active_tab_in_group(
    &self,
    group: egui_workbench::workspace::GroupId,
) -> Option<TabId> {
    let handle = self.workbench.active_tab_in_group(group)?;
    self.workbench.editor_area.get(handle).map(|t| t.id)
}

/// The editor group currently holding hiker tab `id`, if it is mirrored into
/// the workbench. Resolves `id` to its workbench handle first, then asks the
/// workbench which group that handle lives in — the basis for a
/// tab-targeted DRIVE link. status: tab-linking
pub fn group_of_tab(&self, id: TabId) -> Option<egui_workbench::workspace::GroupId> {
    let handle = self.workbench.editor_area.handle_for(|t| t.id == id)?;
    self.workbench.group_of(handle)
}
}

/// Per-frame `Host` adapter. Lives only for the duration
/// of one `Workbench::ui` call.
pub struct HikerWbBehavior<'a> {
    pub app: &'a mut AppState,
    pub rt: &'a Arc<tokio::runtime::Runtime>,
}

impl<'a> Host<HikerWbTab, String> for HikerWbBehavior<'a> {
    fn pane_ui(
        &mut self,
        ui: &mut egui::Ui,
        tab: &mut HikerWbTab,
        _ctx: UiContext<'_>,
    ) {
        let _g = crate::profiling::FrameProf::guard("wb:pane_ui");
        let Some(kind) = self.app.tab_by_id(tab.id).map(|t| t.kind.clone()) else {
            ui.centered_and_justified(|ui| {
                ui.label("(missing tab)");
            });
            return;
        };
        self.render_tab_body(ui, tab.id, ChildSlot::Primary, &kind);
    }

    fn on_preview_promoted(&mut self, tab: &HikerWbTab) {
        // egui-workbench promoted this tab from Preview → Regular
        // (double-click on the tab in the strip, or a "Keep open" menu
        // action). Mirror the promotion into hiker's per-tab `sticky`
        // flag and clear the session-level preview slot so the next
        // non-sticky `open_file` allocates a fresh preview tab instead
        // of swapping the just-pinned tab's contents.
        let id = tab.id;
        if let Some(t) = self.app.tab_by_id_mut(id) {
            t.sticky = true;
        }
        if self.app.session.preview_tab == Some(id) {
            self.app.session.preview_tab = None;
        }
    }

    fn on_tab_close(&mut self, tab: &HikerWbTab) -> bool {
        // Defer to hiker's dirty-close guard. Returning `false` makes
        // egui_tiles keep the pane until the next sync removes it; the
        // guard either calls `editor_pane::close_tab` (synchronous
        // remove from `Session::tabs`) or surfaces the dirty-close
        // modal (close happens later on user confirmation).
        crate::editor_pane::close_tab_with_dirty_guard(self.app, tab.id);
        false
    }

    fn activity_items(&self) -> Vec<Item<String>> {
        // Activity bar = the registry's activity-bar activities, in
        // registry order. `on_activity_bar()` excludes secondary-dock-only
        // activities (chat). Each item's mode is the activity id; its
        // icon/label come straight from the activity.
        // [feature-consumer-activity-bar]
        self.app
            .activities
            .iter()
            .filter(|f| f.on_activity_bar())
            .map(|f| Item {
                label: f.label().to_string(),
                icon: Some(f.icon()),
                mode: f.id().to_string(),
                badge: None,
            })
            .collect()
    }

    fn side_bar_title(&self, mode: &String) -> egui::WidgetText {
        // A top-level activity id resolves to its label directly. A multi-view
        // container sub-view arrives as a slashed wire id (`"context/appears-in"`)
        // that isn't itself an activity, so title-case the view key for the
        // section header instead of showing the raw id. [feature-multi-region-sidebar]
        if let Some(f) = self.app.activities.by_id(mode) {
            return f.label().to_string().into();
        }
        let (_, view_key) = crate::activity::split_view_id(mode);
        titleize(view_key).into()
    }

    fn side_bar_ui(&mut self, ui: &mut egui::Ui, mode: &String) {
        let _g = crate::profiling::FrameProf::guard("wb:side_bar");
        // `mode` is a `ViewId`; resolve its activity, then its view.
        // Every sidebar mode is now a registered Activity with at least
        // one `View`: render through the narrow `activity::SurfaceCtx`, then
        // drain its deferred effects with full `&mut AppState`. The old
        // `panels_registry` fallback was retired once Files (the last
        // hardcoded panel) migrated. [feature-consumer-sidebar]
        let (activity_id, view_key) = crate::activity::split_view_id(mode);
        let activity = self.app.activities.by_id(activity_id).cloned();
        let view = activity
            .as_ref()
            .and_then(|a| a.views().into_iter().find(|v| v.id() == view_key));
        match view {
            Some(view) => {
                // Coerce `&mut AppState` to `&mut dyn AppCtx` and let the
                // view open its own narrow `SurfaceCtx` (via `surface_ctx()`)
                // inside `render`. Then drain any deferred effects with
                // full `&mut AppState`. [feature-consumer-sidebar]
                //
                // Invariant: every surface-invocation site drains the queue
                // synchronously, so it must be empty going in. A non-empty
                // queue here means an earlier surface pushed effects without
                // draining — they'd otherwise fire now against this surface's
                // frame. The queue is a shared `AppState` field, so this
                // guards the one weak spot of decoupling push from drain.
                debug_assert!(
                    self.app.pending_effects.is_empty(),
                    "pending_effects leaked from an earlier surface — every \
                     surface-invocation site must drain it"
                );
                view.render(ui, self.app as &mut dyn crate::activity::AppCtx);
                for eff in std::mem::take(&mut self.app.pending_effects) {
                    eff(self.app);
                }
            }
            None => {
                ui.weak(format!("(panel '{mode}' has no view)"));
            }
        }
    }


    fn container_views(&self, container: &String) -> Vec<String> {
        // Resolve the container to its activity and return its ordered
        // wire view-ids (slashed for multi-view containers). Unknown ids
        // fall back to the bare container id. [feature-multi-region-sidebar]
        self.app.activities.by_id(container).map_or_else(
            || vec![container.clone()],
            |a| {
                let views: Vec<String> = a.views().iter().map(|v| a.view_id(*v)).collect();
                if views.is_empty() { vec![container.clone()] } else { views }
            },
        )
    }

    fn container_location(&self, container: &String) -> egui_workbench::side_bar::Location {
        self.app
            .activities
            .by_id(container)
            .map_or(egui_workbench::side_bar::Location::LeftBar, |a| {
                a.default_location()
            })
    }

    fn side_bar_action_buttons(&mut self, ui: &mut egui::Ui, mode: &String) {
        if mode == "files" {
            // `+` split-button: primary mints a note; the caret dropdown picks
            // any document type. status: sidebar-new-item-button, split-add-button
            let add = crate::widgets::split_button::split_add_button(ui, "New note", |ui| {
                if ui.button("New note").clicked() {
                    self.app.new_note();
                    ui.close();
                }
                if ui.button("New board").clicked() {
                    self.app.new_board();
                    ui.close();
                }
                // status: canvas-create
                if ui.button("New canvas").clicked() {
                    self.app.new_canvas();
                    ui.close();
                }
            });
            if add.primary_clicked {
                self.app.new_note();
            }
        }
        if mode == "clusters" {
            // Clusters-mode `+` split-button: primary `+` opens the review tab
            // for a new tree with default params; the caret dropdown lists
            // tree-creation presets (built-in + user-saved) that prefill it.
            // status: cluster-editor-new-tree-action, cluster-preset
            use crate::clusters::panel::ReviewConfig;
            // Presets are vault notes (`hiker.kind: cluster-preset`) found via
            // the store's frontmatter query; cache the result so the header
            // doesn't re-query every frame. status: cluster-preset
            if self.app.clusters_state.preset_cache.is_none() {
                let vault = self.app.vault_session.vault.clone();
                let loaded = match self.app.vault_session.services.read_store.lock() {
                    Ok(store) => crate::clusters::preset::load(&store, &vault),
                    Err(_) => crate::clusters::preset::builtins(),
                };
                self.app.clusters_state.preset_cache = Some(loaded);
            }
            let presets = self.app.clusters_state.preset_cache.clone().unwrap_or_default();
            let add = crate::widgets::split_button::split_add_button(ui, "New cluster tree", |ui| {
                if ui.button("New tree").clicked() {
                    ReviewConfig::default().open(self.app);
                    ui.close();
                }
                ui.separator();
                ui.label(
                    egui::RichText::new("Presets").small().color(hiker_theme::muted()),
                );
                for preset in &presets {
                    if ui.button(&preset.params.name).clicked() {
                        preset.config().open(self.app);
                        ui.close();
                    }
                }
            });
            if add.primary_clicked {
                ReviewConfig::default().open(self.app);
            }
        }
        if mode == "canvases" {
            // Plain `+` button (no dropdown): mint a new canvas and open it.
            // status: canvas-create, canvas-activity-new-button
            let plus = ui
                .add(
                    egui::ImageButton::new(crate::icons::ICONS.image(crate::icons::Icon::Plus))
                        .corner_radius(crate::widgets::split_button::BUTTON_CORNER_RADIUS),
                )
                .on_hover_text("New canvas");
            if plus.clicked() {
                self.app.new_canvas();
            }
        }
        // Eye toggle for every sidebar view that shows hover previews — the three
        // context sub-views, the Vault lens (cluster-tree thumbnails), and the
        // canvases activity (canvas thumbnails). Appended after any mode-specific
        // buttons (e.g. the canvases `+`). status: preview-toggle
        if matches!(
            mode.as_str(),
            "context/backlinks" | "context/appears-in" | "context/related" | "vault" | "canvases"
        ) {
            self.hover_preview_eye(ui);
        }
    }

    fn side_bar_actions_menu(&mut self, ui: &mut egui::Ui, mode: &String) {
        if mode == "trash" {
            // status: feature-trash-panel
            let count = hiker_core::trash::Trash::open(&self.app.vault_session.vault_root)
                .list_from_disk()
                .map(|v| v.len())
                .unwrap_or(0);
            let enabled = count > 0;
            if ui
                .add_enabled(enabled, egui::Button::new("Empty trash"))
                .clicked()
            {
                self.app.session.modal = Some(crate::state::Modal::Confirm {
                    title: "Empty trash".to_string(),
                    body: format!(
                        "Permanently delete all {count} items in the trash? This can't be undone."
                    ),
                    confirm_label: "Empty trash".to_string(),
                    cancel_label: "Cancel".to_string(),
                    danger: true,
                    intent: crate::state::ConfirmIntent::EmptyTrash,
                });
                ui.close();
            }
            return;
        }
        if mode == "vault" {
            // Vault-mode `⋯`: the lens / grouping picker. status: vault-view-mode
            crate::vault_view::actions_menu(ui, self.app);
            return;
        }
        if mode == "files" {
            if ui.button("Refresh tree").clicked() {
                self.app.file_tree_state.invalidate_all();
                self.app.push_toast("File tree refreshed", crate::state::ToastLevel::Info);
                ui.close();
            }
            ui.separator();
            ui.label(
                egui::RichText::new("Sort by")
                    .color(hiker_theme::muted())
                    .small(),
            );
            use hiker_core::config::sections::TreeSortBy;
            let cur = self
                .app
                .vault_session
                .config
                .read()
                .ok()
                .map(|c| c.vault.tree.sort_by)
                .unwrap_or(TreeSortBy::NameAsc);
            for (label, val) in [
                ("Name A -> Z", TreeSortBy::NameAsc),
                ("Name Z -> A", TreeSortBy::NameDesc),
                ("Modified (newest)", TreeSortBy::MtimeDesc),
                ("Modified (oldest)", TreeSortBy::MtimeAsc),
            ] {
                let prefix = if cur == val { "* " } else { "  " };
                if ui.button(format!("{prefix}{label}")).clicked() {
                    let s = match val {
                        s => s.as_str(),
                    };
                    self.app.persist_tree_sort(s);
                    self.app.file_tree_state.invalidate_all();
                    ui.close();
                }
            }
        }
    }

    fn status_bar_ui(&mut self, ui: &mut egui::Ui) {
        // Active-tab-driven status content: when a Buffer tab is
        // focused, render the per-buffer version dropdown + indexer
        // state + Ln:Col / word count row that used to live inside
        // the editor pane. For any other tab kind, fall back to the
        // vault-level row (vault name + pending / task counters).
        // Other panels can grow their own status content the same way
        // by matching on `TabKind` here.
        // The buffer-map KEY of the active editor tab, for ANY source (a vault
        // path, or the composite key of a read-only snapshot / proposal / trash
        // preview) — so the status bar (and its version dropdown) render on
        // snapshot previews too, not just live vault buffers.
        let active_buffer_key = self
            .app
            .session
            .active_tab
            .and_then(|id| self.app.tab_by_id(id))
            .and_then(|t| match &t.kind {
                crate::tab::TabKind::Editor { buffer, .. } => {
                    Some(crate::buffer::buffer_key_for_source(buffer))
                }
                _ => None,
            });
        if let Some(key) = active_buffer_key {
            self.app.render_buffer_status_bar(ui, &key);
            return;
        }

        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            let vault = self
                .app
                .vault_session
                .vault_root
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("vault");
            ui.weak(vault);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let pending_n = self.app.ui_cache.pending_snapshot.len();
                if pending_n > 0 {
                    ui.weak(format!("{pending_n} staged"));
                    ui.separator();
                }
                let tasks_n = self.app.ui_cache.task_snapshot.len();
                if tasks_n > 0 {
                    ui.weak(format!("{tasks_n} task{}", if tasks_n == 1 { "" } else { "s" }));
                }
            });
        });
    }

    fn theme(&self, style: &egui::Style) -> Palette {
        // The default focused-group stroke (2px, `selection.bg_fill` at
        // ~60% alpha) paints on top of the tile rect — over a light
        // theme it alpha-blends to a near-white border around every edge
        // of the central pane and inside the minimap, which the user
        // reads as stray white padding. Zero out the width to disable
        // the overlay while keeping the rest of the default theme.
        Palette {
            focused_group_border_width: 0.0,
            ..Palette::from_egui_style(style)
        }
    }
}

/// Render the body of a hiker tab. Routes by `TabKind` to the existing
/// per-kind panel renderers. Lifted out of [`HikerWbBehavior::pane_ui`]
/// so the match doesn't fight the behavior trait's borrow contract.
impl<'a> HikerWbBehavior<'a> {
    /// Eye menu shared by every sidebar view that shows hover previews
    /// (`context/backlinks`, `context/appears-in`, `context/related`, `vault`,
    /// `canvases`): one toggle for `[ui].hover_previews_enabled`, read live and
    /// committed at `Scope::Vault`. status: preview-toggle
    fn hover_preview_eye(&mut self, ui: &mut egui::Ui) {
        let enabled = self
            .app
            .vault_session
            .config
            .read()
            .map(|c| c.ui.hover_previews_enabled)
            .unwrap_or(true);
        let resp = ui
            .add(
                egui::ImageButton::new(crate::icons::ICONS.image(crate::icons::Icon::Eye))
                    .corner_radius(crate::widgets::split_button::BUTTON_CORNER_RADIUS),
            )
            .on_hover_text("View options");
        egui::Popup::menu(&resp).show(|ui| {
            let mut show = enabled;
            if ui.checkbox(&mut show, "Show hover previews").changed() {
                self.app.set_setting(
                    hiker_core::config::SettingsScope::Vault,
                    "ui.hover_previews_enabled",
                    &serde_json::json!(show),
                    "Save hover preview toggle failed",
                );
            }
        });
    }

    fn render_tab_body(
        &mut self,
        ui: &mut egui::Ui,
        tab_id: TabId,
        slot: ChildSlot,
        kind: &TabKind,
    ) {
        crate::profile_scope!("render_tab_body", tab_kind_name(kind));
        let _g = crate::profiling::FrameProf::guard(tab_kind_name(kind));
        // A container's two children share its `TabId`. So `TabId`-keyed panels
        // never collide across the two slots (and never follow the slot on
        // swap), each child renders under a distinct synthetic id derived from
        // its `child_state_key` (the single rule for child identity): hash the
        // key, set the high bit so the id can never collide with a real
        // sequential id from `next_tab_id` (small, high-bit clear). The Primary
        // top-level slot keeps the real id verbatim so every existing call site
        // is behavior-identical. Source-keyed panels (CodeGraph) ignore
        // `tab_id` and warm-reuse by source — matching `child_state_key`'s rule.
        // status: container-tab
        let orig_tab_id = tab_id;
        let tab_id = match slot {
            ChildSlot::Primary => tab_id,
            ChildSlot::Secondary => synthetic_child_id(tab_id, slot, kind),
        };
        // A Container is handled before the borrow split below: it recurses
        // into `render_tab_body` for each child (which itself re-borrows
        // `self.app`), so it can't run inside the `let app = &mut *self.app`
        // scope the leaf kinds use. status: container-tab
        if let TabKind::Container { primary, secondary, swapped } = kind {
            self.render_container(ui, orig_tab_id, primary, secondary, *swapped);
            return;
        }
        let app = &mut *self.app;
        let rt = self.rt;
        match kind {
            TabKind::Editor { buffer, diff } => {
                use crate::tab::BufferSource;
                // Inlined `render_editor_tab`: every Editor-kind tab
                // renders through the same buffer panel — diff (when
                // set) layers via `diff_overlay::compute` on the live
                // editor widget. Vault sources hit the path-keyed
                // storage; non-vault sources (snapshot / staging /
                // trash) resolve to read-only buffers stored under
                // composite keys.
                let _ = diff;
                let key = match buffer {
                    BufferSource::Vault { path } => Some(path.clone()),
                    _ => crate::editor_pane::ensure_readonly_buffer_loaded(app, buffer),
                };
                if let Some(k) = key {
                    panels::buffer::show(ui, app, &k, rt);
                } else {
                    ui.colored_label(
                        egui::Color32::RED,
                        "Couldn't load the buffer for this tab.",
                    );
                }
            }
            TabKind::Home => panels::home::show(ui, app),
            TabKind::HomeDetail { which } => {
                // Inlined former `render_home_detail`: single-line
                // forward to the home panel's detail entry point.
                panels::home::show_detail(ui, app, which);
            }
            TabKind::Queue => panels::queue::show(ui, app),
            TabKind::QueueDetail { task_id } => panels::queue::show_detail(ui, app, task_id),
            TabKind::Settings => panels::settings::show(ui, app),
            TabKind::Properties { path } => panels::properties::show(ui, app, path),
            TabKind::Graph { .. } => panels::graph::show(ui, app, tab_id),
            TabKind::Board { path } => panels::board::show(ui, app, tab_id, path, rt),
            TabKind::Canvas { path } => panels::canvas::show(ui, app, tab_id, path, rt),
            TabKind::BoardsIndex => panels::boards_index::show(ui, app),
            TabKind::Rules => panels::rules::show(ui, app),
            TabKind::PatchReview => panels::patch_review::show(ui, app),
            TabKind::IndexerDetail => panels::indexer_detail::show(ui, app, rt),
            TabKind::GitDiff => panels::git_diff::show(ui, app),
            TabKind::ClusterReview { config_json } => {
                clusters::panel::show(ui, app, tab_id, config_json)
            }
            TabKind::ClusterGraph { tree_id } => panels::cluster_graph::show(ui, app, tree_id),
            // A single lens-view (the main interactive pane). The child slot
            // determines the default lens (primary-default vs specs-only) + the
            // lens-view key. The code-graph open path wraps two of these in a
            // `Container` (this primary pane + a peer corner-minimap, the corner
            // rendered by `render_container` via the borrowed-engine Minimap).
            // status: container-tab
            TabKind::CodeGraphLens { source } => {
                panels::code_graph::show_lens(ui, app, orig_tab_id, slot, source)
            }
            TabKind::ProjectConfig { source_note } => {
                panels::project_config::show(ui, app, tab_id, source_note.as_deref())
            }
            TabKind::ZimView { zim_path, article } => {
                panels::zim::show(ui, app, tab_id, zim_path, article)
            }
            TabKind::ChartBuilder { source } => panels::charts_tab::show(ui, app, source),
            // Handled above (early-returns into `render_container`); never
            // reaches the leaf-dispatch match. status: container-tab
            TabKind::Container { .. } => unreachable!("container handled before leaf dispatch"),
        }
    }

    /// Render a [`TabKind::Container`]: the visible primary fills the pane; the
    /// secondary draws as a corner inset. status: container-tab
    ///
    /// The CODE-GRAPH case (a `Peer(CodeGraphLens)` over the SAME source as a
    /// `CodeGraphLens` primary) renders the corner through the **Minimap chrome
    /// borrowing the secondary lens-view's own engine** ([`code_graph::show_secondary`])
    /// — NOT a recursive full render — so the code-graph view uses exactly two
    /// engines (one per lens-view). The swap (flip `swapped`) is owned here: it
    /// fires on the toolbar's "Swap" request OR a click on the corner inset. A
    /// generic `Peer` that is not a same-source code-graph lens keeps the Phase-A
    /// recursive-inset fallback. status: container-tab
    fn render_container(
        &mut self,
        ui: &mut egui::Ui,
        tab_id: TabId,
        primary: &TabKind,
        secondary: &crate::tab::ContainerSecondary,
        swapped: bool,
    ) {
        use crate::tab::ContainerSecondary;
        // Resolve which child shows large vs in the corner (honoring swap).
        // SelfOverview never swaps, so for it the primary always stays large.
        let (large, large_slot, corner) = match (secondary, swapped) {
            (ContainerSecondary::Peer(peer), true) => {
                (peer.as_ref(), ChildSlot::Secondary, Some((primary, ChildSlot::Primary)))
            }
            (ContainerSecondary::Peer(peer), false) => {
                (primary, ChildSlot::Primary, Some((peer.as_ref(), ChildSlot::Secondary)))
            }
            (ContainerSecondary::SelfOverview, _) => (primary, ChildSlot::Primary, None),
        };

        // Is this the code-graph container? (a same-source CodeGraphLens primary
        // + peer). Its corner renders via the borrowed-engine Minimap, not a
        // recursive inset. status: container-tab
        let code_source = code_container_source(primary, secondary);

        let full = ui.max_rect();
        // The large child fills the whole pane.
        {
            let mut large_ui = ui.new_child(egui::UiBuilder::new().max_rect(full));
            self.render_tab_body(&mut large_ui, tab_id, large_slot, large);
        }

        let Some((corner_kind, corner_slot)) = corner else { return };

        // CODE-GRAPH corner: the Minimap chrome borrows the corner lens-view's
        // engine over the SAME host rect as the large pane (an overlay inset, not
        // a separate framed box). A node click selects on the shared doc; the
        // toolbar's "Swap" request flips `swapped`. status: container-tab
        if let Some(source) = code_source {
            let swap = crate::panels::code_graph::show_secondary(
                ui,
                &mut *self.app,
                full,
                &source,
                corner_slot,
            );
            if swap
                && let Some(tab) = self.app.tab_by_id_mut(tab_id)
                && let TabKind::Container { swapped, .. } = &mut tab.kind
            {
                *swapped = !*swapped;
            }
            let _ = (corner_kind, corner_slot);
            return;
        }

        // GENERIC peer corner inset (Phase-A fallback): bottom-right framed box.
        let inset_w = (full.width() * 0.28).clamp(160.0, 360.0);
        let inset_h = (full.height() * 0.28).clamp(120.0, 280.0);
        let margin = 12.0;
        let inset = egui::Rect::from_min_size(
            egui::pos2(full.right() - inset_w - margin, full.bottom() - inset_h - margin),
            egui::vec2(inset_w, inset_h),
        );
        // Frame the inset so it reads as a distinct corner view.
        ui.painter().rect_filled(inset, 4.0, ui.visuals().panel_fill);
        ui.painter().rect_stroke(
            inset,
            4.0,
            ui.visuals().window_stroke(),
            egui::StrokeKind::Inside,
        );
        {
            let mut inset_ui = ui.new_child(egui::UiBuilder::new().max_rect(inset.shrink(2.0)));
            self.render_tab_body(&mut inset_ui, tab_id, corner_slot, corner_kind);
        }
        // Click-to-swap affordance (peer only): an invisible click sink over the
        // inset. Defer the flip past the render borrow. status: container-tab
        let resp =
            ui.interact(inset, ui.id().with(("container_swap", tab_id.0)), egui::Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked()
            && let Some(tab) = self.app.tab_by_id_mut(tab_id)
            && let TabKind::Container { swapped, .. } = &mut tab.kind
        {
            *swapped = !*swapped;
        }
    }
}

/// The [`CodeSource`] of a code-graph container — `Some` only when `primary` is a
/// [`TabKind::CodeGraphLens`] and `secondary` is a `Peer` `CodeGraphLens` over the SAME source. This
/// is the case whose corner renders through the borrowed-engine Minimap (two engines total), not a
/// recursive inset. status: container-tab
fn code_container_source(
    primary: &TabKind,
    secondary: &crate::tab::ContainerSecondary,
) -> Option<crate::tab::CodeSource> {
    use crate::tab::ContainerSecondary;
    let TabKind::CodeGraphLens { source: ps } = primary else { return None };
    let ContainerSecondary::Peer(peer) = secondary else { return None };
    match peer.as_ref() {
        TabKind::CodeGraphLens { source: ss } if ss == ps => Some(ps.clone()),
        _ => None,
    }
}

/// A stable synthetic `TabId` for a container child, so `TabId`-keyed panels
/// under one container never collide / never follow the slot on swap. Derived
/// from the child's [`crate::tab::child_state_key`] (the single source of truth
/// for child identity) by hashing it and setting the high bit — that bit can
/// never appear in a real sequential id from `next_tab_id`. status: container-tab
fn synthetic_child_id(tab_id: TabId, slot: ChildSlot, kind: &TabKind) -> TabId {
    use std::hash::{Hash, Hasher};
    let key = crate::tab::child_state_key(tab_id, slot, kind);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    TabId(0x8000_0000_0000_0000 | (hasher.finish() & 0x7fff_ffff_ffff_ffff))
}

/// Stable per-kind label for the frame profiler / puffin scopes.
const fn tab_kind_name(kind: &TabKind) -> &'static str {
    match kind {
        TabKind::Editor { .. } => "tab:Editor",
        TabKind::Home => "tab:Home",
        TabKind::HomeDetail { .. } => "tab:HomeDetail",
        TabKind::Queue => "tab:Queue",
        TabKind::QueueDetail { .. } => "tab:QueueDetail",
        TabKind::Settings => "tab:Settings",
        TabKind::Properties { .. } => "tab:Properties",
        TabKind::Graph { .. } => "tab:Graph",
        TabKind::Board { .. } => "tab:Board",
        TabKind::Canvas { .. } => "tab:Canvas",
        TabKind::BoardsIndex => "tab:BoardsIndex",
        TabKind::Rules => "tab:Rules",
        TabKind::PatchReview => "tab:PatchReview",
        TabKind::IndexerDetail => "tab:IndexerDetail",
        TabKind::GitDiff => "tab:GitDiff",
        TabKind::ClusterReview { .. } => "tab:ClusterReview",
        TabKind::ClusterGraph { .. } => "tab:ClusterGraph",
        TabKind::CodeGraphLens { .. } => "tab:CodeGraphLens",
        TabKind::ProjectConfig { .. } => "tab:ProjectConfig",
        TabKind::ZimView { .. } => "tab:ZimView",
        TabKind::ChartBuilder { .. } => "tab:ChartBuilder",
        TabKind::Container { .. } => "tab:Container",
    }
}

/// Title-case a kebab/snake view key for a side-bar section header:
/// `"appears-in"` → `"Appears In"`, `"backlinks"` → `"Backlinks"`.
/// [feature-multi-region-sidebar]
fn titleize(key: &str) -> String {
    key.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}
