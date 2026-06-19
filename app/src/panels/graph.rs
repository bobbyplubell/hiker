//! Vault-wide note-link graph panel. Directed TYPED graph: one node per .md
//! (typed from `hiker.kind` — `vault-graph-kind-nodes`), edges unioned from
//! body wikilinks + board/trail membership (`vault-graph-typed-edges`; the
//! data model lives in [`super::graph_data`]). The pan/zoom, layout,
//! view-options menu, and the node/edge/label/hover/preview rendering all
//! live in the shared `hiker_graph_view` engine; this panel is the
//! vault-specific [`graph_view::source::Source`] adapter plus the vault walk
//! that builds the graph, the toolbar filter controls, and the tab-linking
//! (FOLLOW / DRIVE) wiring.
//!
//! Tree layouts need a tree; the vault graph is not one, so we BFS a
//! spanning tree rooted on the active note (when it's in the graph) or the
//! highest-degree node otherwise. Non-tree edges are still drawn — the
//! tree only shapes positions.

use std::collections::HashMap;

use eframe::egui;
use petgraph::graph::{DiGraph, NodeIndex};

use crate::editor_pane;
use crate::state::{AppState, NavTarget};
use crate::tab::Scope;
use crate::panels::graph_data::{
    self, Detail, NodeData, ScopeState, VaultData, VaultEdgeKind, VaultKind,
};
use crate::panels::graph_spec;
use hiker_code::GovState;
use hiker_graph_view::graph_view;
use hiker_graph_view::graph_view::source::{LayoutConfig, NodeDescriptor, NodeShape, Source};
use hiker_graph_view::graph_view::styling::{Palette, Style};
use hiker_core::store::dto::{BoardCardRow, ListRefRow, WaypointRow};
use hiker_core::vault::Vault;
use hiker_graph::{bfs_tree, dfs_tree, LayoutKind, LayoutTree};
use hiker_theme as theme;

/// Re-scan the vault for new/removed files no more often than this. Layout
/// runs in the background, but rebuilds still trigger file I/O so keep this
/// generous; users can hit "Rebuild" for an explicit refresh.
const REBUILD_AFTER_SECS: u64 = 300;
const LAYOUT_BOX: f32 = 1000.0;
const VAULT_CFG: LayoutConfig = LayoutConfig {
    area: LAYOUT_BOX * LAYOUT_BOX,
    seed_box: LAYOUT_BOX,
};

/// The vault graph panel's persistent state: the built graph + the shared
/// render engine + the vault-specific "Orphans" toggle. Lives on
/// `AppState::panels.graph` (a persisted singleton).
pub struct VaultPanel {
    pub data: VaultData,
    pub engine: graph_view::State,
    pub show_orphans: bool,
    /// Per-edge-kind visibility, auto-populated from the kinds present in
    /// the built graph (a vault with no boards offers no dead toggle) and
    /// color-keyed to the edges. status: vault-graph-edge-toggles
    edge_filter: Vec<(VaultEdgeKind, bool)>,
    /// Per-node-kind visibility, same data-driven shape as the code graph's
    /// entity-kind filter. status: vault-graph-kind-filters
    kind_filter: Vec<(VaultKind, bool)>,
    /// Coarse detail dial: Containers (boards/trails/queries only) or
    /// Everything. status: vault-graph-lod-containers
    detail: Detail,
    /// Display scope: the full vault (`Overview`) or the focus anchor's
    /// 1–3-hop neighbourhood over the typed edges — the code graph's
    /// overview ⇄ focus grammar on the vault graph. status: graph-nav-extract
    scope: Scope,
    /// The focus anchor's note rel-path. Set by an overview click (so the
    /// hops dial has a centre), a hops-mode drill, "Open in graph"
    /// (`open-in-graph`), or a Back/Forward restore.
    focus: Option<String>,
    /// The query-doc scope, executed per rebuild: the member set bounds the
    /// node universe ("graph of this smart folder"), orthogonal to — and
    /// composing with — the hops focus above. A failed query keeps its loud
    /// error here (the smart-folder posture). status: graph-scoped-query
    scope_query: Option<ScopeState>,
    /// Whether spec drift badges are shown. status: vault-graph-spec-drift-badge
    drift_badges: bool,
    /// The vault-side governance rollup (per-spec drift fold + repo map),
    /// loaded on the first badge enable / spec jump and cached — the code
    /// overlay's lazy posture. `pub(crate)` for `graph_spec::jump_to_spec`.
    pub(crate) drift: Option<graph_spec::DriftStates>,
    /// Per-note badge states folded from `drift` × the data's anchors map;
    /// recomputed on rebuild and on badge enable.
    note_badges: HashMap<String, GovState>,
    /// Whether the persisted view state (saved positions/projection/pan-zoom)
    /// has been applied to `engine` yet. Applied once, on the first render after
    /// a (re)build, by `apply_persisted_view`. status: graph-view-state-persist
    view_restored: bool,
    /// "Find / jump to node" popup (Ctrl+F). Reuses the shared standalone
    /// autocomplete picker over the note paths; a pick navigates to that note.
    /// Independent of the editor's Ctrl+F. status: graph-find-popup
    find: crate::widgets::autocomplete_picker::PickerState,
    /// Latched right-click menu: the right-clicked note's rel-path + the
    /// pointer position the popup opens at (the engine owns its pane response,
    /// so the menu is hosted in a popup instead of `Response::context_menu`).
    /// Right-click is a menu, never a direct action (`interaction.md`
    /// [rightclick-menu-always]).
    node_menu: Option<(String, egui::Pos2)>,
}

/// The session key the singleton vault graph persists its view state under — the
/// same key its tab uses (`:graph`). status: graph-view-state-persist
const GRAPH_VIEW_KEY: &str = ":graph";

pub fn show(ui: &mut egui::Ui, app: &mut AppState, tab_id: crate::tab::TabId) {
    // This tab's cross-tab wiring (FOLLOW source / DRIVE target). status: tab-linking
    let link = app.tab_by_id(tab_id).map(|t| t.link).unwrap_or_default();
    let active_path = highlighted_note_path(app, link);

    // First frame: build + install, then seed the global nav stack with the
    // panel's initial location, so a Back after the first drill returns here
    // instead of skipping out of the tab — unless a pending focus owns the
    // seeding (`open_focused` pushes its own overview + focused pair).
    // status: graph-nav-extract
    if app.panels.graph.is_none() {
        let data = Builder { app }.build_data();
        install_and_layout(app, data, active_path.as_deref());
        if !app.session.nav.locked
            && app.panels.graph_pending_nav.is_none()
            && let Some(vg) = app.panels.graph.as_ref()
        {
            app.session
                .nav
                .push(NavTarget::VaultGraphNode { focus: vg.focus.clone(), scope: vg.scope });
        }
    }
    // Apply a pending focus/nav target (open-in-graph, or a restore that
    // arrived before the panel was built) — BEFORE the nav snapshot below,
    // so it's never re-recorded. status: graph-tab-focus
    if let Some((focus, scope)) = app.panels.graph_pending_nav.take() {
        apply_nav_target(app, focus, scope);
    }
    // Apply a pending query scope ("Open in graph, scoped" before the
    // panel's first render). status: graph-scoped-query
    if let Some(query) = app.panels.graph_pending_scope.take() {
        apply_scope(app, Some(query));
    }

    // The Esc ladder's middle rung needs to know whether an open find popup /
    // latched node menu consumes this frame's Esc — captured BEFORE they
    // process input. [keyboard-esc-ladder]
    let esc_taken_by_popup = app
        .panels
        .graph
        .as_ref()
        .is_some_and(|vg| vg.find.is_open() || vg.node_menu.is_some());
    // Snapshot the focus location before any interaction this frame, to
    // detect a user-driven drill afterwards and record it globally.
    let before = nav_snapshot(app);

    let t = toolbar(ui, app, tab_id);
    // A toolbar ⟵/⟶ press drives the GLOBAL Back/Forward (`nav_go`); the
    // restore must not be re-recorded as a fresh entry. status: graph-nav-extract
    let nav_restoring = t.nav_delta.is_some();
    if let Some(delta) = t.nav_delta {
        editor_pane::nav_go(app, delta);
    }
    // The drift-badge toggle loads the rollup on first enable (it binds the
    // project repos' SCIP adapters, so it can't run under the toolbar's
    // panel borrow). status: vault-graph-spec-drift-badge
    if t.drift_toggled {
        toggle_drift(app);
    }

    let stale = app
        .panels
        .graph
        .as_ref()
        .map(|vg| vg.data.built_at.elapsed().as_secs() > REBUILD_AFTER_SECS)
        .unwrap_or(true);
    if t.rebuild || stale {
        let data = Builder { app }.build_data();
        install_and_layout(app, data, active_path.as_deref());
    } else if t.relayout || t.filters_changed {
        relayout_vault(app, active_path.as_deref());
    }

    // Ctrl+F opens the "Find / jump to note" popup (independent of the editor's Ctrl+F — this graph
    // tab is what's showing). status: graph-find-popup
    if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F))
        && let Some(vg) = app.panels.graph.as_mut()
    {
        vg.find.open();
    }

    let clicked = render_canvas(ui, app, active_path.as_deref());
    node_menu_ui(ui, app);
    // A find-popup pick navigates exactly like a node click (same routing,
    // scope-dependent like the click). status: graph-find-popup
    let jumped = find_popup(ui, app);
    route_pick(app, link, clicked.or(jumped));

    // Esc = up one level: pop a hops focus back to the overview (toolbar
    // Back still walks the global nav stack). The shared gate skips it when
    // this frame's Esc went to the find popup / node menu, and while any
    // text field holds focus. [keyboard-esc-ladder]
    if let Some(vg) = app.panels.graph.as_mut()
        && crate::panels::graph_nav::esc_pops_focus(ui, vg.scope, esc_taken_by_popup)
    {
        vg.scope = Scope::Overview;
    }

    // A dial / drill / Esc changed the focus DISPLAY (the scope, or the
    // anchor while focused): re-lay-out and record the new location on the
    // GLOBAL nav stack. Overview anchor changes are silent — the click that
    // set them already recorded its file open — and a Back/Forward restore
    // (`nav_restoring` / `nav.locked`) is never re-recorded; `NavState::push`
    // dedupes consecutive equal targets. status: graph-nav-extract
    let after = nav_snapshot(app);
    let display_changed =
        before.1 != after.1 || (matches!(after.1, Scope::Hops(_)) && before.0 != after.0);
    if display_changed {
        relayout_vault(app, active_path.as_deref());
        if !nav_restoring && !app.session.nav.locked {
            let (focus, scope) = after;
            app.session.nav.push(NavTarget::VaultGraphNode { focus, scope });
        }
    }
}

/// The vault panel's navigation snapshot `(focus, scope)` — the fields a
/// drill changes. `(None, Overview)` when the panel isn't built yet.
fn nav_snapshot(app: &AppState) -> (Option<String>, Scope) {
    app.panels
        .graph
        .as_ref()
        .map_or((None, Scope::Overview), |vg| (vg.focus.clone(), vg.scope))
}

/// Restore a focus location onto the vault panel — the apply side of a
/// global Back/Forward (`NavTarget::VaultGraphNode`) and of `open_focused`.
/// Falls back to the pending slot when the panel isn't built yet (consumed
/// silently by `show` on its next frame). Clamps the hop depth to the
/// dial's 1–3 range. status: graph-tab-focus
pub(crate) fn apply_nav_target(app: &mut AppState, focus: Option<String>, scope: Scope) {
    let scope = match scope {
        Scope::Hops(d) => Scope::Hops(d.clamp(1, 3)),
        Scope::Overview => Scope::Overview,
    };
    let Some(vg) = app.panels.graph.as_mut() else {
        app.panels.graph_pending_nav = Some((focus, scope));
        return;
    };
    vg.focus = focus;
    vg.scope = scope;
    relayout_vault(app, None);
}

/// Open/focus the singleton Graph tab focused on `path`'s depth-bounded
/// neighbourhood — the "Open in graph" dispatch (`open-in-graph`,
/// `open-in-graph-containers`). The nav stack is seeded overview-then-focused
/// so Back from the neighbourhood is the full-vault overview.
/// status: graph-tab-focus
pub fn open_focused(app: &mut AppState, path: &str, depth: u8) {
    use crate::tab::{GraphFocus, Tab, TabKind};
    let depth = depth.clamp(1, 3);
    let focus = Some(GraphFocus { path: path.to_string(), depth });
    match app.session.tabs.iter().position(|t| matches!(t.kind, TabKind::Graph { .. })) {
        Some(i) => {
            // A live query scope survives a focused open — focus drills
            // WITHIN the scoped universe (`graph-scoped-query` composes).
            let scope_query = match &app.session.tabs[i].kind {
                TabKind::Graph { scope_query, .. } => scope_query.clone(),
                _ => None,
            };
            app.session.tabs[i].kind = TabKind::Graph { focus, scope_query };
            let id = app.session.tabs[i].id;
            app.session.active_tab = Some(id);
        }
        None => {
            let id = app.next_tab_id();
            app.session.tabs.push(Tab::new(
                id,
                TabKind::Graph { focus, scope_query: None },
                true,
            ));
            app.session.active_tab = Some(id);
        }
    }
    if !app.session.nav.locked {
        app.session.nav.push(NavTarget::VaultGraphNode { focus: None, scope: Scope::Overview });
        app.session.nav.push(NavTarget::VaultGraphNode {
            focus: Some(path.to_string()),
            scope: Scope::Hops(depth),
        });
    }
    apply_nav_target(app, Some(path.to_string()), Scope::Hops(depth));
}

/// Open/focus the singleton Graph tab scoped to `query_path`'s match set —
/// the "Open in graph, scoped" dispatch from a query-doc / smart-folder
/// menu. The scope bounds the node UNIVERSE and is orthogonal to the hops
/// focus (which drills within it); the landing is the scoped overview.
/// Scope is display state like the filters — it rides the persisted view
/// state, not the nav stack. status: graph-scoped-query
pub fn open_scoped(app: &mut AppState, query_path: &str) {
    use crate::tab::{Tab, TabKind};
    let scope_query = Some(query_path.to_string());
    match app.session.tabs.iter().position(|t| matches!(t.kind, TabKind::Graph { .. })) {
        Some(i) => {
            app.session.tabs[i].kind = TabKind::Graph { focus: None, scope_query };
            let id = app.session.tabs[i].id;
            app.session.active_tab = Some(id);
        }
        None => {
            let id = app.next_tab_id();
            app.session.tabs.push(Tab::new(
                id,
                TabKind::Graph { focus: None, scope_query },
                true,
            ));
            app.session.active_tab = Some(id);
        }
    }
    apply_scope(app, Some(query_path.to_string()));
}

/// Set or clear the panel's query scope: execute the query (per-set, and
/// again on every rebuild — never from a snapshot), land on the scoped
/// overview, and re-lay-out. Falls back to the pending slot when the panel
/// isn't built yet. status: graph-scoped-query
pub(crate) fn apply_scope(app: &mut AppState, query_path: Option<String>) {
    if app.panels.graph.is_none() {
        app.panels.graph_pending_scope = query_path;
        return;
    }
    let scope = query_path.map(|q| run_scope_query(app, &q));
    if let Some(vg) = app.panels.graph.as_mut() {
        vg.scope_query = scope;
        vg.scope = Scope::Overview;
        vg.focus = None;
    }
    relayout_vault(app, None);
}

/// Execute a query-doc against the index for the graph scope: parse the
/// doc, run it through the one shared `run_query` path, and collect the
/// member path set. Failures land as the loud error string the toolbar
/// renders (the smart-folder posture — never a silent empty or
/// match-everything fallback). status: graph-scoped-query
fn run_scope_query(app: &AppState, path: &str) -> ScopeState {
    use hiker_core::queries;
    let result = (|| {
        let src = app
            .vault_session
            .vault
            .read_file(path)
            .map_err(|e| e.to_string())?;
        let query = queries::parse_query_doc_for(path, &src).map_err(|e| e.to_string())?;
        let store = app
            .vault_session
            .services
            .read_store
            .lock()
            .map_err(|_| "index store unavailable (lock poisoned)".to_string())?;
        let rows = queries::run_query(&store, &app.vault_session.services.kinds, &query, &[])
            .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(|r| r.path).collect())
    })();
    ScopeState { path: path.to_string(), title: basename(path), result }
}

/// Flip the drift-badge toggle (`vault-graph-spec-drift-badge`), loading
/// the vault-side governance rollup on the first enable (binding each
/// project repo's SCIP adapter + drift-checking its links.json — the code
/// overlay's first-switch cost) and refolding the per-note badge states.
/// Pure recolor: badges ride the Painter pass, no relayout.
fn toggle_drift(app: &mut AppState) {
    let needs_load = app
        .panels
        .graph
        .as_mut()
        .map(|vg| {
            vg.drift_badges = !vg.drift_badges;
            vg.drift_badges && vg.drift.is_none()
        })
        .unwrap_or(false);
    if needs_load {
        let drift = graph_spec::load(app);
        if let Some(vg) = app.panels.graph.as_mut() {
            vg.drift = Some(drift);
        }
    }
    if let Some(vg) = app.panels.graph.as_mut() {
        vg.note_badges = vg
            .drift
            .as_ref()
            .map(|d| graph_spec::note_badges(&vg.data, d))
            .unwrap_or_default();
        vg.engine.invalidate_paint_cache();
    }
}

/// Route a node click / find pick. In hops scope the click DRILLS — it
/// re-anchors the neighbourhood, the focus-nav exception to click-opens
/// (`interaction.md` [click-exceptions]; opening stays one right-click away
/// on the node menu). In the overview it opens the note (DRIVE-aware,
/// unchanged) and anchors the scope dial so the hop settings have a node to
/// centre on. status: graph-nav-extract
fn route_pick(app: &mut AppState, link: crate::tab::TabLink, pick: Option<String>) {
    let Some(path) = pick else { return };
    let mut drilled = false;
    if let Some(vg) = app.panels.graph.as_mut() {
        vg.focus = Some(path.clone());
        drilled = matches!(vg.scope, Scope::Hops(_));
    }
    if drilled {
        return; // show()'s nav-snapshot diff re-lays-out and records the drill
    }
    // DRIVE: when this graph targets a linked group, open the clicked
    // note there; otherwise the historical self-open. status: tab-linking
    match editor_pane::drive_target_group(app, link.target) {
        Some(group) => editor_pane::open_file_in_group(app, &path, group, false),
        None => editor_pane::open_file(app, &path, false),
    }
}

/// Outcome of a toolbar pass: rebuild / relayout requests plus the GLOBAL
/// nav delta a Back/Forward control asked for (the caller drives `nav_go`,
/// which needs `app.session.nav` free). status: graph-nav-extract
#[derive(Default)]
struct ToolbarResult {
    rebuild: bool,
    relayout: bool,
    filters_changed: bool,
    nav_delta: Option<i32>,
    /// The drift-badge toggle flipped — the caller loads the rollup with
    /// `app` free (the toolbar holds the panel borrow).
    /// status: vault-graph-spec-drift-badge
    drift_toggled: bool,
}

/// The graph toolbar: heading + tab-link control, Back/Forward, the
/// Rebuild / Reset view / Find buttons, the shared scope dial
/// (`graph-nav-extract`), the eye view-menu, the typed-graph filter
/// controls, and the status line.
fn toolbar(ui: &mut egui::Ui, app: &mut AppState, tab_id: crate::tab::TabId) -> ToolbarResult {
    let can_back = app.session.nav.can_back();
    let can_fwd = app.session.nav.can_forward();
    let mut out = ToolbarResult::default();
    let mut reset_view = false;
    ui.horizontal_wrapped(|ui| {
        ui.heading("Graph");
        link_control(ui, app, tab_id);
        out.nav_delta = crate::panels::graph_nav::nav_controls(ui, can_back, can_fwd);
        if ui.small_button("Rebuild").clicked() {
            out.rebuild = true;
        }
        if ui.small_button("Reset view").clicked() {
            reset_view = true;
        }
        if ui
            .small_button("Find")
            .on_hover_text("Find / jump to note (Ctrl+F)")
            .clicked()
            && let Some(vg) = app.panels.graph.as_mut()
        {
            vg.find.open();
        }
        if let Some(vg) = app.panels.graph.as_mut() {
            // The shared scope dial: Overview, or the focus anchor's 1–3-hop
            // neighbourhood over the typed edges. A change rides `show`'s
            // nav-snapshot diff (relayout + nav record). status: graph-nav-extract
            ui.separator();
            let anchor_label = vg.focus.as_deref().map(basename);
            crate::panels::graph_nav::scope_dial(ui, &mut vg.scope, anchor_label.as_deref());
            out.relayout = vg.engine.view_options_menu(
                ui,
                crate::icons::ICONS.image(crate::icons::Icon::Eye),
                &mut [("Orphans", &mut vg.show_orphans)],
            );
            // Typed-graph display controls: edge-kind toggles, node-kind
            // filter, detail dial. A change re-lays-out (the filters shape
            // the drawn topology, not just the paint).
            out.filters_changed = filter_controls(ui, vg);
            // The query-scope chip + the drift-badge toggle.
            let overlay = overlay_controls(ui, vg);
            out.filters_changed |= overlay.scope_cleared;
            out.drift_toggled = overlay.drift_toggled;
            let status = match vg.engine.worker.as_ref() {
                Some(w) if w.is_running() => format!("· layout {} iters", w.iters_done()),
                _ => String::new(),
            };
            ui.label(
                egui::RichText::new(format!(
                    "{} · {} notes · {} links · zoom {:.2}x {}",
                    vg.engine.layout_kind.label(),
                    vg.data.graph.node_count(),
                    vg.data.graph.edge_count(),
                    vg.engine.view.zoom,
                    status,
                ))
                .color(theme::muted())
                .small(),
            );
        }
    });
    if reset_view
        && let Some(vg) = app.panels.graph.as_mut()
    {
        vg.engine.needs_fit = true;
    }
    out
}

/// The typed-graph display controls, mirroring the code graph's toolbar
/// grammar: per-edge-kind toggles (color-keyed, so the toolbar doubles as
/// the edge legend — `vault-graph-edge-toggles`), the data-driven node-kind
/// filter (`vault-graph-kind-filters`), and the coarse detail dial
/// (`vault-graph-lod-containers`). Returns whether anything changed; the
/// caller re-lays-out, since the filters shape the drawn/laid-out topology.
fn filter_controls(ui: &mut egui::Ui, vg: &mut VaultPanel) -> bool {
    let mut changed = false;
    ui.separator();
    for (kind, on) in &mut vg.edge_filter {
        let text = egui::RichText::new(kind.label())
            .small()
            .color(kind.color().unwrap_or_else(theme::muted));
        if ui.selectable_label(*on, text).on_hover_text(kind.describe()).clicked() {
            *on = !*on;
            changed = true;
        }
    }
    ui.separator();
    for (kind, on) in &mut vg.kind_filter {
        let text = egui::RichText::new(kind.label())
            .small()
            .color(kind.color().unwrap_or_else(theme::muted));
        if ui.selectable_label(*on, text).clicked() {
            *on = !*on;
            changed = true;
        }
    }
    ui.separator();
    ui.label(egui::RichText::new("Detail:").small().color(theme::muted()));
    for d in [Detail::Containers, Detail::Everything] {
        let on = vg.detail == d;
        if ui.selectable_label(on, egui::RichText::new(d.label()).small()).clicked() && !on {
            vg.detail = d;
            changed = true;
        }
    }
    changed
}

/// Outcome of the toolbar's scope/drift section.
#[derive(Default)]
struct OverlayControls {
    scope_cleared: bool,
    drift_toggled: bool,
}

/// The query-scope chip (`graph-scoped-query`) and the drift-badge toggle
/// (`vault-graph-spec-drift-badge`). The chip names the scoping query-doc
/// with its live member count — or its LOUD error, the smart-folder
/// posture — plus a Clear button to return to the full vault; the Drift toggle is a
/// recolor whose data the caller loads (it needs `app`).
fn overlay_controls(ui: &mut egui::Ui, vg: &mut VaultPanel) -> OverlayControls {
    let mut out = OverlayControls::default();
    if let Some(scope) = &vg.scope_query {
        ui.separator();
        ui.label(egui::RichText::new("Scoped:").small().color(theme::muted()));
        match &scope.result {
            Ok(members) => {
                ui.label(
                    egui::RichText::new(format!("{} ({})", scope.title, members.len()))
                        .small()
                        .color(theme::kind_query()),
                );
            }
            Err(e) => {
                ui.label(
                    egui::RichText::new(format!("{}: query error: {e}", scope.title))
                        .small()
                        .color(egui::Color32::RED),
                );
            }
        }
        if ui
            .small_button("Clear")
            .on_hover_text("Clear the query scope (show the full vault)")
            .clicked()
        {
            vg.scope_query = None;
            out.scope_cleared = true;
        }
    }
    ui.separator();
    if ui
        .selectable_label(vg.drift_badges, egui::RichText::new("Drift").small())
        .on_hover_text(
            "Spec drift badges (ok / drifted / missing) from each project repo's              links.json baseline; loads on first enable, empty when no project              carries one",
        )
        .clicked()
    {
        out.drift_toggled = true;
    }
    out
}

/// Drive one frame of the "Find / jump to note" popup. While the panel's picker is open, render the
/// shared autocomplete picker over the vault's note paths; returns the chosen note rel-path on a
/// pick, for the caller to navigate to (same routing as a node click). status: graph-find-popup
fn find_popup(ui: &mut egui::Ui, app: &mut AppState) -> Option<String> {
    use crate::widgets::autocomplete_picker::{self, PickerOutcome};
    let vg = app.panels.graph.as_mut()?;
    if !vg.find.is_open() {
        return None;
    }
    // The picker queries per frame; collect the note paths once per open frame and rank over them.
    let paths: Vec<String> =
        vg.data.graph.node_weights().map(|n| n.path.clone()).collect();
    let source = crate::panels::graph_find::VaultNodeFindSource::new(&paths);
    match autocomplete_picker::show(ui, &mut vg.find, &source) {
        PickerOutcome::Selected(item) => Some(item.insert.to_string()),
        PickerOutcome::Cancelled | PickerOutcome::Open => None,
    }
}

/// Install freshly built `data`, preserving the engine view options and the
/// display filters across rebuilds (only the graph is replaced), then
/// (re)run the layout. The filter rows re-derive from the fresh data
/// (data-driven: exactly the kinds present), keeping the user's offs and
/// defaulting any first-appearing kind to visible.
fn install_and_layout(app: &mut AppState, data: VaultData, active_path: Option<&str>) {
    let prev = app.panels.graph.take();
    // The focus location survives a rebuild like the other display controls;
    // a stale anchor (note deleted) degrades to the overview display at the
    // source. status: graph-nav-extract
    let (mut scope, mut focus) = prev
        .as_ref()
        .map_or((Scope::Overview, None), |vg| (vg.scope, vg.focus.clone()));
    // The query scope's PATH survives the rebuild; its member set re-executes
    // below against the fresh index (per-rebuild execution, never a snapshot).
    // status: graph-scoped-query
    let mut scope_path = prev
        .as_ref()
        .and_then(|vg| vg.scope_query.as_ref().map(|sc| sc.path.clone()));
    let (mut drift_badges, drift) = prev
        .as_ref()
        .map_or((false, None), |vg| (vg.drift_badges, vg.drift.clone()));
    let (mut engine, show_orphans, mut view_restored, prev_edge, prev_kind, mut detail) =
        match prev {
            Some(vg) => (
                vg.engine,
                vg.show_orphans,
                vg.view_restored,
                vg.edge_filter,
                vg.kind_filter,
                vg.detail,
            ),
            None => (
                graph_view::State::new(Style::flat(), LayoutKind::ForceDirected),
                true,
                false,
                Vec::new(),
                Vec::new(),
                Detail::default(),
            ),
        };
    let mut edge_filter = graph_data::merge_filter(&prev_edge, graph_data::edge_filter_for(&data));
    let mut kind_filter = graph_data::merge_filter(&prev_kind, graph_data::kind_filter_for(&data));
    // First build of this session: seed the persisted view (projection / toggles /
    // pan-zoom + warm-seed positions) onto the fresh engine BEFORE the layout, so
    // `recompute_layout` morphs onto the saved shape instead of scattering. A later
    // rebuild keeps `view_restored = true` and skips this. status: graph-view-state-persist
    let mut restored = false;
    if !view_restored {
        if let Some(saved) = app.session.graph_views.get(GRAPH_VIEW_KEY) {
            let snap = view_state_to_snapshot(saved);
            engine.restore_view(&snap);
            // The display filters ride the same record, stored as the HIDDEN
            // kinds so a kind first appearing after a rebuild stays visible.
            // status: vault-graph-edge-toggles, vault-graph-kind-filters
            for (k, on) in &mut edge_filter {
                *on = !saved.hidden_edge_kinds.iter().any(|s| s == k.persist_str());
            }
            for (k, on) in &mut kind_filter {
                *on = !saved.hidden_node_kinds.iter().any(|s| s == k.persist_str());
            }
            detail = Detail::from_persist_str(&saved.detail);
            // The focus-nav location rides the same record (the code graph's
            // scope/selected posture). status: graph-nav-extract
            scope = crate::panels::graph_nav::scope_from_persist_str(&saved.scope);
            focus = saved.focus.clone();
            // The query scope + drift toggle ride it too; the scope's member
            // set re-executes below, the drift rollup reloads on demand.
            // status: graph-scoped-query, vault-graph-spec-drift-badge
            scope_path = saved.scope_query.clone();
            drift_badges = saved.drift_badges;
            restored = true;
        }
        view_restored = true;
    }
    let scope_query = scope_path.map(|q| run_scope_query(app, &q));
    let note_badges = drift
        .as_ref()
        .map(|d| graph_spec::note_badges(&data, d))
        .unwrap_or_default();
    let mut vg = VaultPanel {
        data,
        engine,
        show_orphans,
        edge_filter,
        kind_filter,
        detail,
        scope,
        focus,
        scope_query,
        drift_badges,
        drift,
        note_badges,
        view_restored,
        find: crate::widgets::autocomplete_picker::PickerState::default(),
        node_menu: None,
    };
    let vault = app.vault_session.vault.clone();
    {
        let VaultPanel {
            data,
            engine,
            show_orphans,
            edge_filter,
            kind_filter,
            detail,
            scope,
            focus,
            scope_query,
            drift_badges,
            note_badges,
            ..
        } = &mut vg;
        let filters = Filters {
            show_orphans: *show_orphans,
            edge_filter,
            kind_filter,
            detail: *detail,
            scope: *scope,
            focus: focus.as_deref(),
            query_scope: scope_query.as_ref(),
            badges: drift_badges.then_some(&*note_badges),
        };
        let source = VaultSource::new(data, vault.as_ref(), active_path, &filters);
        engine.recompute_layout(&source, VAULT_CFG);
        // `recompute_layout` flags `needs_fit`; a restored view wins over the
        // fresh-build framing, so clear it after the layout. status: graph-view-state-persist
        if restored {
            engine.needs_fit = false;
        }
    }
    app.panels.graph = Some(vg);
}

/// Snapshot the vault graph engine's view state into the session map under the
/// singleton `:graph` key, so it survives the panel being dropped and feeds
/// tab-state persistence on exit. No-op when the panel isn't built yet.
/// status: graph-view-state-persist
pub(crate) fn capture_graph_view(app: &mut AppState) {
    let Some(vg) = app.panels.graph.as_ref() else {
        return;
    };
    let mut state = snapshot_to_view_state(&vg.engine.view_snapshot());
    // The display filters ride the same record, stored as the HIDDEN kinds so
    // a kind first appearing after a rebuild defaults to visible (mirrors the
    // code graph's `hidden_kinds`). status: vault-graph-edge-toggles,
    // vault-graph-kind-filters, vault-graph-lod-containers
    state.hidden_edge_kinds = vg
        .edge_filter
        .iter()
        .filter(|&&(_, on)| !on)
        .map(|&(k, _)| k.persist_str().to_string())
        .collect();
    state.hidden_node_kinds = vg
        .kind_filter
        .iter()
        .filter(|&&(_, on)| !on)
        .map(|&(k, _)| k.persist_str().to_string())
        .collect();
    state.detail = vg.detail.persist_str().to_string();
    // The focus-nav location persists beside the filters (the code graph's
    // scope/selected posture). status: graph-nav-extract
    state.scope = crate::panels::graph_nav::scope_persist_str(vg.scope);
    state.focus = vg.focus.clone();
    // The query scope persists as its DOC PATH only (the member set
    // re-executes per rebuild); the drift toggle as its flag.
    // status: graph-scoped-query, vault-graph-spec-drift-badge
    state.scope_query = vg.scope_query.as_ref().map(|sc| sc.path.clone());
    state.drift_badges = vg.drift_badges;
    app.session.graph_views.insert(GRAPH_VIEW_KEY.to_string(), state);
}

/// Convert the engine's plain [`graph_view::source::Snapshot`] into the
/// serializable [`hiker_core::autosave::GraphViewState`]. The vault-only
/// display-filter fields default empty here; `capture_graph_view` fills them
/// (the code graph's embedded engine state leaves them unused).
/// status: graph-view-state-persist
pub(crate) fn snapshot_to_view_state(
    snap: &graph_view::source::Snapshot,
) -> hiker_core::autosave::GraphViewState {
    hiker_core::autosave::GraphViewState {
        positions: snap.positions.clone(),
        pan_x: snap.pan_x,
        pan_y: snap.pan_y,
        zoom: snap.zoom,
        projection_kind: snap.projection_kind.clone(),
        projection_strength: snap.projection_strength,
        projection_size_falloff: snap.projection_size_falloff,
        focus_mode: snap.focus_mode.clone(),
        show_labels: snap.show_labels,
        show_edges: snap.show_edges,
        show_preview: snap.show_preview,
        lod_full_mag: snap.lod_full_mag,
        lod_marker_mag: snap.lod_marker_mag,
        ..Default::default()
    }
}

/// Convert a stored [`hiker_core::autosave::GraphViewState`] back into the
/// engine's plain [`graph_view::source::Snapshot`]. status: graph-view-state-persist
pub(crate) fn view_state_to_snapshot(
    state: &hiker_core::autosave::GraphViewState,
) -> graph_view::source::Snapshot {
    graph_view::source::Snapshot {
        positions: state.positions.clone(),
        pan_x: state.pan_x,
        pan_y: state.pan_y,
        zoom: state.zoom,
        projection_kind: state.projection_kind.clone(),
        projection_strength: state.projection_strength,
        projection_size_falloff: state.projection_size_falloff,
        focus_mode: state.focus_mode.clone(),
        show_labels: state.show_labels,
        show_edges: state.show_edges,
        show_preview: state.show_preview,
        lod_full_mag: state.lod_full_mag,
        lod_marker_mag: state.lod_marker_mag,
    }
}

/// Recompute positions in place after a layout-kind or filter change (graph
/// data unchanged; the filters reshape the drawn/laid-out topology).
fn relayout_vault(app: &mut AppState, active_path: Option<&str>) {
    let vault = app.vault_session.vault.clone();
    let Some(vg) = app.panels.graph.as_mut() else {
        return;
    };
    let VaultPanel {
        data,
        engine,
        show_orphans,
        edge_filter,
        kind_filter,
        detail,
        scope,
        focus,
        scope_query,
        drift_badges,
        note_badges,
        ..
    } = vg;
    let filters = Filters {
        show_orphans: *show_orphans,
        edge_filter,
        kind_filter,
        detail: *detail,
        scope: *scope,
        focus: focus.as_deref(),
        query_scope: scope_query.as_ref(),
        badges: drift_badges.then_some(&*note_badges),
    };
    let source = VaultSource::new(data, vault.as_ref(), active_path, &filters);
    engine.recompute_layout(&source, VAULT_CFG);
}

/// Drive the shared engine for one frame; returns its click/hover output.
fn render_canvas(
    ui: &mut egui::Ui,
    app: &mut AppState,
    active_path: Option<&str>,
) -> Option<String> {
    let vault = app.vault_session.vault.clone();
    let vg = app.panels.graph.as_mut()?;
    let VaultPanel {
        data,
        engine,
        show_orphans,
        edge_filter,
        kind_filter,
        detail,
        scope,
        focus,
        scope_query,
        drift_badges,
        note_badges,
        node_menu,
        ..
    } = vg;
    let filters = Filters {
        show_orphans: *show_orphans,
        edge_filter,
        kind_filter,
        detail: *detail,
        scope: *scope,
        focus: focus.as_deref(),
        query_scope: scope_query.as_ref(),
        badges: drift_badges.then_some(&*note_badges),
    };
    let source = VaultSource::new(data, vault.as_ref(), active_path, &filters);
    let clicked = engine
        .ui(ui, &source, |p: &egui::Painter, r: egui::Rect, t: &str, b: &str, a: egui::Pos2| {
            paint_preview_card(p, r, t, b, a);
        });
    // Right-click a node → latch its MENU (never a direct action, per
    // `interaction.md` [rightclick-menu-always]); `node_menu_ui` renders it
    // and applies the picked verb.
    if let Some(path) = engine.take_secondary_click() {
        let pos = ui.ctx().pointer_latest_pos().unwrap_or_else(|| ui.min_rect().center());
        *node_menu = Some((path, pos));
    }
    clicked
}

/// A vault-graph node menu verb: the shared note-item base, plus the spec
/// notes' jump to the code graph. status: vault-graph-spec-drift-badge
enum NodeMenuAction {
    Base(crate::item_menu::ItemAction),
    /// Open the code graph with this spec slug preselected (lit) — the
    /// bridge-by-navigation rule: the two graphs never merge node sets.
    JumpSpec(String),
}

/// Render the latched node context menu and apply the picked verb. A vault
/// graph node is a plain note ref, so its menu starts from the shared
/// note-item base (Open · Reveal in file tree · Open in graph · Copy path ·
/// Properties) — same item kind, same options as every note list
/// (`interaction.md` [rightclick-menu-always]); its "Open in graph" simply
/// re-focuses this tab on the node. A node defining `[slug]` spec anchors
/// composes an "Open in code graph" section on top (one entry per anchor;
/// resolution through the link-store baseline happens at dispatch, so an
/// ungoverned slug answers with a loud toast, never a dead end).
fn node_menu_ui(ui: &mut egui::Ui, app: &mut AppState) {
    use crate::item_menu::{self, BaseOpts};
    // Render under a short panel borrow, then apply with `app` free.
    let picked = {
        let Some(vg) = app.panels.graph.as_mut() else { return };
        let Some((path, _)) = vg.node_menu.clone() else { return };
        let mut menu =
            item_menu::note_item_base(&path, BaseOpts { reveal: true }, NodeMenuAction::Base);
        if let Some(slugs) = vg.data.anchors_by_note.get(&path) {
            menu = menu.section();
            let mut sub = egui_workbench::menu::Menu::new();
            for slug in slugs {
                sub = sub.action(format!("Light {slug}"), NodeMenuAction::JumpSpec(slug.clone()));
            }
            menu = menu.submenu("Open in code graph", sub);
        }
        item_menu::latched_menu_popup(
            ui,
            egui::Id::new("vault-graph-node-menu"),
            &mut vg.node_menu,
            menu,
        )
        .map(|action| (action, path))
    };
    match picked {
        Some((NodeMenuAction::Base(action), path)) => {
            item_menu::apply_item_action(app, action, &path);
        }
        Some((NodeMenuAction::JumpSpec(slug), _)) => graph_spec::jump_to_spec(app, &slug),
        None => {}
    }
}

/// The display-filter slice of [`VaultPanel`] the source resolves against —
/// split-borrowed so the engine can run mutably beside it.
struct Filters<'a> {
    show_orphans: bool,
    edge_filter: &'a [(VaultEdgeKind, bool)],
    kind_filter: &'a [(VaultKind, bool)],
    detail: Detail,
    /// Focus-nav display scope + anchor (`graph-nav-extract`): `Hops` clamps
    /// the drawn set to the anchor's typed-edge neighbourhood.
    scope: Scope,
    focus: Option<&'a str>,
    /// The executed query scope, when one is set: its member set bounds the
    /// node universe BEFORE the focus walk (the two compose — scope filters
    /// the universe, focus drills within it). status: graph-scoped-query
    query_scope: Option<&'a ScopeState>,
    /// Per-note drift badge states, when the toggle is on.
    /// status: vault-graph-spec-drift-badge
    badges: Option<&'a HashMap<String, GovState>>,
}

/// Vault adapter from `VaultData` to the shared graph engine. The kind
/// filter + detail dial resolve to a per-node visibility mask and the edge
/// toggles to a filtered edge list ONCE at construction (the engine calls
/// `edges()` several times per frame); node indices stay stable so hidden
/// nodes keep their layout slots.
struct VaultSource<'a> {
    graph: &'a DiGraph<NodeData, VaultEdgeKind>,
    vault: &'a Vault,
    active_path: Option<&'a str>,
    show_orphans: bool,
    /// Per-node visibility under the kind filter + detail dial.
    visible: Vec<bool>,
    /// The drawn/laid-out edge pairs (kind toggles + endpoint visibility
    /// applied), aligned with `edge_colors`.
    edges: Vec<(u32, u32)>,
    /// Per-edge color override (`None` = the style's edge color), keyed by
    /// the surviving edge's kind. status: vault-graph-edge-toggles
    edge_colors: Vec<Option<egui::Color32>>,
    /// Whether the display is a focus neighbourhood. Orphan-hiding is skipped
    /// then — every member reached the anchor through a drawn edge by
    /// construction, and the anchor itself may be degree-0.
    /// status: graph-nav-extract
    in_focus: bool,
    /// Whether a query scope bounds the universe. Orphan-hiding is skipped
    /// then too: the scope IS the folder's member set, and node degree is
    /// global — hiding members that happen to lack in-scope edges would
    /// silently shrink the folder. status: graph-scoped-query
    scoped: bool,
    /// Per-note drift badge states (the toggle is on), painted as the
    /// engine's badge dot in the governance palette.
    /// status: vault-graph-spec-drift-badge
    badges: Option<&'a HashMap<String, GovState>>,
}

impl<'a> VaultSource<'a> {
    fn new(
        data: &'a VaultData,
        vault: &'a Vault,
        active_path: Option<&'a str>,
        filters: &Filters<'a>,
    ) -> Self {
        let mut base = graph_data::visible_nodes(data, filters.kind_filter, filters.detail);
        // Query scope first (`graph-scoped-query`): the member set bounds
        // the node universe; the focus walk below then drills WITHIN it
        // (composition by construction — the BFS sees only scoped drawn
        // edges). A failed query displays only the query-doc, beside the
        // toolbar's loud error (the smart-folder posture).
        let scoped = filters.query_scope.is_some();
        if let Some(sc) = filters.query_scope {
            base = match &sc.result {
                Ok(members) => graph_data::restrict_to_scope(data, &base, members),
                Err(_) => graph_data::scope_error_mask(data, &sc.path),
            };
        }
        // Focus scope (`graph-nav-extract`): clamp the display to the
        // anchor's depth-bounded neighbourhood over the toggled-on typed
        // edges. A stale anchor (gone from the rebuild) falls back to the
        // overview display, mirroring the code graph.
        let (visible, in_focus) = match (filters.scope, filters.focus) {
            (Scope::Hops(d), Some(path)) => {
                match graph_data::focus_nodes(data, filters.edge_filter, &base, path, d) {
                    Some(mask) => (mask, true),
                    None => (base, false),
                }
            }
            _ => (base, false),
        };
        let kept = graph_data::visible_edges(data, filters.edge_filter, &visible);
        let edges = kept.iter().map(|&(a, b, _)| (a, b)).collect();
        let edge_colors = kept.iter().map(|&(_, _, k)| k.color()).collect();
        Self {
            graph: &data.graph,
            vault,
            active_path,
            show_orphans: filters.show_orphans,
            visible,
            edges,
            edge_colors,
            in_focus,
            scoped,
            badges: filters.badges,
        }
    }

    /// Tree-layout root: the active note when it's in the graph, else the
    /// highest-degree node.
    fn pick_root(&self) -> usize {
        if let Some(p) = self.active_path {
            for idx in self.graph.node_indices() {
                if self.graph[idx].path == p {
                    return idx.index();
                }
            }
        }
        let mut best_i = 0usize;
        let mut best_d = 0u32;
        for idx in self.graph.node_indices() {
            let d = self.graph[idx].degree;
            if d > best_d {
                best_d = d;
                best_i = idx.index();
            }
        }
        best_i
    }
}

impl Source for VaultSource<'_> {
    fn node_count(&self) -> usize {
        self.graph.node_count()
    }

    fn nodes(&self, positions: &[egui::Vec2], style: &Style) -> Vec<NodeDescriptor> {
        let (node_color, active_color) = match style.palette {
            Palette::Flat { node, active } => (node, active),
            Palette::Policy { cluster, .. } => (cluster, cluster),
        };
        let mut out = Vec::new();
        for idx in self.graph.node_indices() {
            let i = idx.index();
            if i >= positions.len() {
                continue;
            }
            let n = &self.graph[idx];
            // Kind filter + detail dial (`vault-graph-kind-filters`,
            // `vault-graph-lod-containers`), then hide orphans (degree 0)
            // when toggled off; keeps the canvas on the linked subgraph. A
            // focus neighbourhood keeps orphans — only its anchor can be one.
            if !self.visible[i]
                || (!self.show_orphans && n.degree == 0 && !self.in_focus && !self.scoped)
            {
                continue;
            }
            let is_active = self.active_path == Some(n.path.as_str());
            // Container kinds (board/trail/query) take the square +
            // larger-label treatment the cluster graph uses for high-level
            // nodes, hued from the theme; plain notes keep the flat
            // (user-editable) palette. status: vault-graph-kind-nodes
            let container = n.kind.is_container();
            let fill = if is_active {
                active_color
            } else {
                n.kind.color().unwrap_or(node_color)
            };
            out.push(NodeDescriptor {
                index: i,
                world_pos: positions[i],
                radius: node_radius(n.degree),
                shape: if container { NodeShape::Square } else { NodeShape::Circle },
                fill,
                resting_stroke: egui::Stroke::new(0.5, theme::divider()),
                hover_stroke: egui::Stroke::new(2.0, active_color),
                // Drift badge (`vault-graph-spec-drift-badge`): a spec
                // note's folded governance state as the top-right mark, in
                // the code overlay's palette — ok / drifted / missing; a
                // note with no governed anchors wears nothing.
                badge: self
                    .badges
                    .and_then(|m| m.get(&n.path))
                    .map(|&st| crate::panels::code_governance::gov_color(st)),
                bug_badge: None,
                label: Some(basename(&n.path)),
                label_min_zoom: 0.0,
                label_scale: if container { 1.3 } else { 1.0 },
                click_path: Some(n.path.clone()),
                tooltip: None,
            });
        }
        out
    }

    fn edges(&self) -> Vec<(u32, u32)> {
        self.edges.clone()
    }

    fn edge_color(&self, index: usize) -> Option<egui::Color32> {
        self.edge_colors.get(index).copied().flatten()
    }

    fn layout_tree(&self, kind: LayoutKind) -> LayoutTree {
        let root = self.pick_root();
        let n = self.graph.node_count();
        // Radial wants a shallow tree (one ring per depth → BFS); the
        // vertical/horizontal layouts want depth → DFS. Spanning over the
        // FILTERED edges so the tree shape matches what's drawn.
        match kind {
            LayoutKind::Radial => bfs_tree(n, &self.edges, root),
            _ => dfs_tree(n, &self.edges, root),
        }
    }

    fn node_key(&self, index: usize) -> Option<String> {
        // The note's rel-path is stable across vault rebuilds, so it carries
        // each node's layout position through a re-walk.
        self.graph.node_weight(NodeIndex::new(index)).map(|n| n.path.clone())
    }

    fn preview_for(&self, index: usize) -> Option<(String, String)> {
        let n = self.graph.node_weight(NodeIndex::new(index))?;
        let title = basename(&n.path);
        let body = self
            .vault
            .read_file(&n.path)
            .ok()
            .map(|s| preview_snippet(skip_frontmatter(&s)))
            .unwrap_or_else(|| "(unable to read note)".to_string());
        Some((title, body))
    }
}

/// The note the graph highlights as "active": the note active in the linked
/// FOLLOW source group when set, else the global active buffer. status: tab-linking
fn highlighted_note_path(app: &AppState, link: crate::tab::TabLink) -> Option<String> {
    editor_pane::followed_note_path(app, link.source).or_else(|| {
        app.session
            .active_tab
            .and_then(|id| app.tab_by_id(id))
            .and_then(|t| t.buffer_path())
            .map(std::string::ToString::to_string)
    })
}

/// Small "Link" control: opens a popup to wire this graph tab to follow /
/// drive another editor group. status: tab-linking
fn link_control(ui: &mut egui::Ui, app: &mut AppState, tab_id: crate::tab::TabId) {
    let linked = app
        .tab_by_id(tab_id)
        .map(|t| t.link.source.is_some() || t.link.target.is_some())
        .unwrap_or(false);
    let label = if linked { "Link *" } else { "Link" };
    let resp = ui
        .small_button(label)
        .on_hover_text("Link this graph to another tab group");
    egui::Popup::menu(&resp).show(|ui| {
        editor_pane::link_menu_ui(ui, app, tab_id);
    });
}

fn node_radius(degree: u32) -> f32 {
    6.0 + ((degree as f32) + 1.0).ln() * 2.0
}

/// File basename without directory or `.md`. `pub(crate)` so the cluster
/// graph panel can reuse it.
pub(crate) fn basename(path: &str) -> String {
    let stem = path.rsplit('/').next().unwrap_or(path);
    stem.strip_suffix(".md").unwrap_or(stem).to_string()
}

/// Truncate a note body to a preview snippet (≤500 chars post-frontmatter).
/// `pub(crate)` — shared with the cluster graph preview.
pub(crate) fn preview_snippet(body: &str) -> String {
    const MAX: usize = 500;
    if body.chars().count() <= MAX {
        body.to_string()
    } else {
        let mut out: String = body.chars().take(MAX).collect();
        out.push('…');
        out
    }
}

/// Skip a YAML frontmatter block (and trailing blank lines) at the start of
/// a markdown file so previews open on real content. Returns the input
/// unchanged when it doesn't open with `---`. `pub(crate)` so the cluster
/// graph panel can reuse it.
pub(crate) fn skip_frontmatter(source: &str) -> &str {
    let trimmed_left = source.trim_start_matches(['\u{feff}']);
    let Some(rest) = trimmed_left
        .strip_prefix("---\n")
        .or_else(|| trimmed_left.strip_prefix("---\r\n"))
    else {
        return source;
    };
    let mut search_from = 0;
    while let Some(idx) = rest[search_from..].find("\n---") {
        let start = search_from + idx + 1; // line start of the closing fence
        let after_fence = start + 3; // past the three dashes
        let tail = &rest[after_fence..];
        if tail.starts_with('\n') || tail.starts_with("\r\n") || tail.is_empty() {
            let skip = if tail.starts_with("\r\n") { 2 } else { 1 };
            return rest[after_fence + skip..].trim_start_matches(['\n', '\r']);
        }
        search_from = after_fence;
    }
    source
}

/// Paint a small preview card anchored near `anchor`. Shared between the
/// vault graph, cluster graph, and wikilink-hover panels.
pub(crate) fn paint_preview_card(
    painter: &egui::Painter,
    canvas: egui::Rect,
    title: &str,
    body: &str,
    anchor: egui::Pos2,
) -> Option<egui::Rect> {
    paint_preview_card_with(painter, canvas, title, body, anchor, 0.0).map(|p| p.card_rect)
}

/// Returned geometry from [`paint_preview_card_with`]. Lets a caller
/// implement scrollable bodies and hit-test the pointer against `card_rect`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PreviewCardGeometry {
    pub card_rect: egui::Rect,
    pub max_scroll_y: f32,
}

/// Variant of [`paint_preview_card`] supporting a vertical scroll offset on
/// the body. Callers managing their own hover lifecycle feed in a clamped
/// `scroll_y` per frame.
pub(crate) fn paint_preview_card_with(
    painter: &egui::Painter,
    canvas: egui::Rect,
    title: &str,
    body: &str,
    anchor: egui::Pos2,
    scroll_y: f32,
) -> Option<PreviewCardGeometry> {
    let pad = 8.0;
    let max_size = egui::vec2(320.0, 180.0);
    let card_size = max_size.min(canvas.size() - egui::vec2(pad * 2.0, pad * 2.0));
    if card_size.x < 80.0 || card_size.y < 60.0 {
        return None;
    }
    // Try bottom-right of cursor first; flip quadrants to avoid clipping.
    let offset = egui::vec2(14.0, 14.0);
    let mut min = anchor + offset;
    if min.x + card_size.x > canvas.right() - pad {
        min.x = anchor.x - offset.x - card_size.x;
    }
    if min.y + card_size.y > canvas.bottom() - pad {
        min.y = anchor.y - offset.y - card_size.y;
    }
    min.x = min.x.clamp(canvas.left() + pad, canvas.right() - pad - card_size.x);
    min.y = min.y.clamp(canvas.top() + pad, canvas.bottom() - pad - card_size.y);
    let card_rect = egui::Rect::from_min_size(min, card_size);

    let bg = egui::Color32::from_rgb(0xfa, 0xfa, 0xfa);
    let border = egui::Color32::from_rgb(0xc8, 0xcd, 0xd4);
    let title_color = egui::Color32::from_rgb(0x1a, 0x1e, 0x24);
    let body_color = egui::Color32::from_rgb(0x4a, 0x52, 0x5c);

    painter.rect_filled(card_rect, 4.0, bg);
    painter.rect_stroke(
        card_rect,
        4.0,
        egui::Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );

    let inner = card_rect.shrink(8.0);
    let title_galley = painter.layout(
        title.to_string(),
        egui::FontId::proportional(13.0),
        title_color,
        inner.width(),
    );
    let title_size = title_galley.size();
    painter.galley(inner.left_top(), title_galley, title_color);

    let body_top = inner.left_top() + egui::vec2(0.0, title_size.y + 6.0);
    if body_top.y >= inner.bottom() {
        return Some(PreviewCardGeometry {
            card_rect,
            max_scroll_y: 0.0,
        });
    }
    let body_rect = egui::Rect::from_min_max(body_top, inner.right_bottom());
    let body_galley = painter.layout(
        body.to_string(),
        egui::FontId::proportional(11.0),
        body_color,
        body_rect.width(),
    );
    let body_h = body_galley.size().y;
    let max_scroll_y = (body_h - body_rect.height()).max(0.0);
    let scroll_clamped = scroll_y.clamp(0.0, max_scroll_y);
    let clip_painter = painter.with_clip_rect(body_rect);
    clip_painter.galley(
        body_rect.left_top() - egui::vec2(0.0, scroll_clamped),
        body_galley,
        body_color,
    );
    Some(PreviewCardGeometry {
        card_rect,
        max_scroll_y,
    })
}

/// Vault-graph builder. Bundles `&AppState` so the multi-step build
/// (walk → wikilink scan → store reads → typed edge union) is a set of
/// inherent methods; the pure assembly itself lives in
/// [`graph_data::Assembler`]. status: vault-graph-typed-edges
struct Builder<'a> {
    app: &'a AppState,
}

/// The store-side inputs of one vault-graph build: every note's kind
/// classification, the three derived membership tables, and the spec-anchor
/// map (slug → defining note paths). status: vault-graph-typed-edges
struct IndexedInputs {
    kinds: HashMap<String, VaultKind>,
    boards: Vec<BoardCardRow>,
    trails: Vec<WaypointRow>,
    lists: Vec<ListRefRow>,
    anchors: HashMap<String, Vec<String>>,
}

impl Builder<'_> {
    /// Walk the vault, classify every note's kind off the `note_meta` index
    /// (against the kind registry's shapes) + the spec-anchor index, and
    /// union the five typed edge sets: wikilinks + spec references (body
    /// scan), board membership (`board_cards`), trail membership
    /// (`trail_waypoints`), list membership (`list_refs`) — the derived
    /// tables are indexed store reads, never a re-parse.
    fn build_data(&self) -> VaultData {
        let app = self.app;
        let paths: Vec<String> = app
            .vault_session
            .vault
            .walk_indexable_files("")
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.ends_with(".md"))
            .collect();
        let inputs = self.indexed_inputs();
        let mut asm = graph_data::Assembler::new(&paths, &inputs.kinds, inputs.anchors);
        for p in &paths {
            if let Ok(body) = app.vault_session.vault.read_file(p) {
                asm.add_wikilinks(p, &body);
            }
        }
        asm.add_board_cards(&inputs.boards);
        asm.add_trail_waypoints(&inputs.trails);
        asm.add_list_refs(&inputs.lists);
        asm.finish()
    }

    /// One store pass for the typed inputs: every note's `hiker.kind` (via
    /// `notes_with_meta`, classified against the registry's SHAPES — never a
    /// hardcoded PM name list), the three derived membership tables, and the
    /// spec-anchor table. A store error / poisoned lock degrades to a
    /// kindless wikilink-only graph — the same posture as the vault-view
    /// lens reads.
    fn indexed_inputs(&self) -> IndexedInputs {
        let registry = &self.app.vault_session.services.kinds;
        let Ok(store) = self.app.vault_session.services.read_store.lock() else {
            tracing::warn!(
                "graph: read store lock poisoned; degrading to a kindless wikilink-only graph \
                 (no kinds, board edges, trail edges, list edges, or spec edges)"
            );
            return IndexedInputs {
                kinds: HashMap::new(),
                boards: Vec::new(),
                trails: Vec::new(),
                lists: Vec::new(),
                anchors: HashMap::new(),
            };
        };
        // slug → defining paths (store order: sorted), feeding [[spec:…]]
        // resolution; its key set feeds the Spec classification.
        // status: vault-graph-spec-edges
        let mut anchors: HashMap<String, Vec<String>> = HashMap::new();
        for (slug, path) in store.all_spec_anchors().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "graph: all_spec_anchors failed; spec edges omitted");
            Vec::new()
        }) {
            anchors.entry(slug).or_default().push(path);
        }
        let defining: std::collections::HashSet<&str> =
            anchors.values().flatten().map(String::as_str).collect();
        let kinds = store
            .notes_with_meta()
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "graph: notes_with_meta failed; nodes render kindless");
                Vec::new()
            })
            .into_iter()
            .map(|n| {
                let kind = VaultKind::classify(
                    n.kind.as_deref(),
                    defining.contains(n.path.as_str()),
                    registry,
                );
                (n.path, kind)
            })
            .collect();
        IndexedInputs {
            kinds,
            boards: store.all_board_cards().unwrap_or_else(|e| {
                tracing::warn!(error = %e, "graph: all_board_cards failed; board edges omitted");
                Vec::new()
            }),
            trails: store.all_trail_waypoints().unwrap_or_else(|e| {
                tracing::warn!(error = %e, "graph: all_trail_waypoints failed; trail edges omitted");
                Vec::new()
            }),
            lists: store.all_list_refs().unwrap_or_else(|e| {
                tracing::warn!(error = %e, "graph: all_list_refs failed; list edges omitted");
                Vec::new()
            }),
            anchors,
        }
    }
}
