//! Code-graph view panel (`code-graph-view-source`). Renders a project note's **repo source** as
//! a precise entity graph through the shared `hiker_graph_view` engine — a third
//! `graph_view::source::Source` beside the vault link-graph and the cluster-tree graph.
//!
//! The note (`hiker.kind: project`) is parsed by `hiker-projects`, whose repo source binds the
//! SCIP adapter (`hiker-code`); the adapter's `code_graph()` is mapped to colored/sized nodes
//! (by entity kind) + typed edges (calls / implements), with edge-type toggles, an orphan-hiding
//! default for large repos, and a read-only click→detail (signature location).
//!
//! State lives on `AppState::panels.code_graph`, keyed by the project-note path, so flipping
//! tabs keeps each project's layout (and its non-Clone adapter + background worker) warm.

use eframe::egui;

use std::path::{Component, Path, PathBuf};

use crate::state::{AppState, NavTarget};
use crate::tab::{CodeSource, Scope, Tab, TabId, TabKind};
use hiker_code::{collapse, CodeGraph, CollapsedGraph, ScipAdapter};
use hiker_graph::{LayoutKind, LayoutTree};
use hiker_graph_view::graph_view;
use hiker_graph_view::graph_view::source::{LayoutConfig, NodeDescriptor, NodeShape, Source};
use hiker_graph_view::graph_view::styling::Style;
use hiker_projects::{repo::Backend, Project};
use hiker_theme as theme;
use spec_engine::{DerivedNodeSource, EdgeKind, NodeHandle, SourceId};

const FR_BOX: f32 = 1200.0;
const CODE_CFG: LayoutConfig = LayoutConfig { area: FR_BOX * FR_BOX, seed_box: 80.0 };

/// Per-source panel state: the render engine + bound adapter + the **full** graph and the currently
/// **displayed** (scope- and kind-filtered, collapsed) graph + view controls.
pub struct View {
    engine: graph_view::State,
    /// `None` only when `error` is set (the source failed to bind); the cached error stops a costly
    /// re-bind every frame.
    adapter: Option<ScipAdapter>,
    src: SourceId,
    /// The complete graph (with containment), filtered/collapsed into `graph`.
    full: CodeGraph,
    /// What's actually rendered this frame (collapsed).
    graph: CodeGraph,
    /// Display scope: the whole graph, or 1–3 hops around the selected node.
    /// status: code-graph-scope-hops
    scope: Scope,
    /// Per-kind visibility, auto-populated from the kinds present in `full` (sorted) — the
    /// filter section offers exactly what the data contains, nothing hardcoded.
    /// status: code-graph-kind-filters
    kind_filter: Vec<(String, bool)>,
    show_calls: bool,
    show_impls: bool,
    /// Whether disconnected (degree-0) nodes are shown in the overview. Off by default: orphans lay
    /// out as a noisy ring around the connected core.
    show_orphans: bool,
    /// Weight node size by LOC (the SCIP enclosing-range body span) instead of degree.
    size_by_loc: bool,
    /// "Find / jump to node" popup (Ctrl+F). Reuses the shared standalone
    /// autocomplete picker over the full node list; a pick selects the node (and drills
    /// from the overview). Independent of the editor's Ctrl+F. status: graph-find-popup
    find: crate::widgets::autocomplete_picker::PickerState,
    /// Last-applied display inputs, for change detection.
    applied: Applied,
    /// The click-selected node's id (stable across rebuilds). Selection drives the detail
    /// line, the edge highlight, and — when scope is `Hops` — the neighbourhood anchor.
    selected: Option<String>,
    /// The fill-overlay layer: mode dial (Kind / Spec / Diff), the governance +
    /// diff data behind it, and the lit spec. Overlay changes recolor — they
    /// never trigger a relayout.
    overlay: crate::panels::code_governance::Overlay,
    /// Latched right-click target: the node's moniker + the pointer position the
    /// menu opens at (the engine owns its pane response, so the menu is hosted in
    /// a popup instead of `Response::context_menu`). Right-click is a menu, never
    /// a direct action (`interaction.md`).
    node_menu: Option<(String, egui::Pos2)>,
    error: Option<String>,
    /// Whether persisted view state (filters / scope / engine view) has been
    /// applied to this view yet. Applied once, on the first render.
    /// status: graph-view-state-persist
    view_restored: bool,
}

/// The display-shaping inputs as last applied by [`rebuild_display`]. `anchor` is the selection
/// only while scope is `Hops` — so overview clicks (selection-only changes) don't trigger a
/// needless relayout, while hops-mode clicks recenter.
#[derive(Clone, PartialEq, Eq)]
struct Applied {
    scope: Scope,
    anchor: Option<String>,
    calls: bool,
    impls: bool,
    orphans: bool,
    kinds: Vec<(String, bool)>,
}

impl View {
    /// The hops anchor: the selected node, only while scope is `Hops`.
    fn anchor(&self) -> Option<String> {
        match self.scope {
            Scope::Hops(_) => self.selected.clone(),
            Scope::Overview => None,
        }
    }

    /// The current display-shaping snapshot, compared against [`Applied`] for rebuilds.
    fn current_applied(&self) -> Applied {
        Applied {
            scope: self.scope,
            anchor: self.anchor(),
            calls: self.show_calls,
            impls: self.show_impls,
            orphans: self.show_orphans,
            kinds: self.kind_filter.clone(),
        }
    }

    /// The navigation snapshot `(selected, scope)` — the fields a drill changes. Used to
    /// detect a user-driven change and to build the global `NavTarget::CodeGraphNode` entry.
    fn nav_snapshot(&self) -> (Option<String>, Scope) {
        (self.selected.clone(), self.scope)
    }

    /// Whether `kind` is visible under the current filter. A kind missing from the filter
    /// rows (shouldn't happen — rows are derived from the data) defaults to visible.
    fn kind_on(&self, kind: &str) -> bool {
        self.kind_filter.iter().find(|(k, _)| k == kind).is_none_or(|(_, on)| *on)
    }
}

/// Restore a drill location (`selected`, `scope`) onto `view` and re-settle the display —
/// the apply side of a global Back/Forward (`NavTarget::CodeGraphNode`). A pure field-setter
/// (no egui) so it's unit-testable. status: code-graph-view-source
pub(crate) fn apply_nav_target(view: &mut View, selected: Option<String>, scope: Scope) {
    view.selected = selected;
    view.scope = scope;
    rebuild_display(view);
}

impl View {
    /// Point the click-selection at node `id` (a SCIP moniker) so the detail line shows it. Used by
    /// spec→code navigation to highlight the resolved symbol after opening the tab — a no-op for the
    /// render itself (selection only drives the detail line). status: spec-code-link
    pub(crate) fn preselect(&mut self, id: String) {
        self.selected = Some(id);
    }
}

/// Light `spec` on the code-graph view for `key` — the landing half of the vault graph's
/// spec → code-graph jump (`vault-graph-spec-drift-badge`): flip the overlay to governance (loading
/// the rollup if needed), light the spec, and pulse its nodes. A not-yet-built view takes the spec
/// through a pending slot consumed (once) by `show` after the build — the `graph-tab-focus` pending
/// pattern on the code side.
pub(crate) fn light_spec(app: &mut AppState, key: &str, spec: &str) {
    let Some(view) = app.panels.code_graph.get_mut(key) else {
        app.panels.code_graph_pending_light = Some((key.to_string(), spec.to_string()));
        return;
    };
    let View { overlay, adapter, src, .. } = view;
    if let Some(adapter) = adapter {
        overlay.mode = crate::panels::code_governance::OverlayMode::Governance;
        overlay.ensure_governance(adapter, src);
        overlay.light(Some(spec.to_string()), adapter, src);
        view.engine.invalidate_paint_cache();
        pulse_lit(view);
    }
}

/// Snapshot a loaded code-graph view's persisted state (display controls + the
/// engine view) into the session map under `key` (`CodeSource::key()`), so it
/// survives the view being dropped and feeds tab-state persistence on exit.
/// No-op when no view is loaded for `key`. status: graph-view-state-persist
pub(crate) fn capture_code_graph_view(app: &mut AppState, key: &str) {
    let Some(view) = app.panels.code_graph.get(key) else {
        return;
    };
    let engine = crate::panels::graph::snapshot_to_view_state(&view.engine.view_snapshot());
    let state = hiker_core::autosave::CodeGraphViewState {
        scope: crate::panels::graph_nav::scope_persist_str(view.scope),
        selected: view.selected.clone(),
        // The HIDDEN kinds persist (not the visible ones), so a kind first appearing
        // after a reindex defaults to visible. status: code-graph-kind-filters
        hidden_kinds: view
            .kind_filter
            .iter()
            .filter(|(_, on)| !on)
            .map(|(k, _)| k.clone())
            .collect(),
        show_calls: view.show_calls,
        show_impls: view.show_impls,
        show_orphans: view.show_orphans,
        size_by_loc: view.size_by_loc,
        engine,
    };
    app.session.code_graph_views.insert(key.to_string(), state);
}

/// Apply persisted view state to a freshly-built code-graph view, once. Called on
/// the first render for `key`: if the session map has saved state, restore the
/// display controls + focus + the engine view (positions/projection/pan-zoom),
/// rebuild the display so the new filters take effect, and suppress the
/// fresh-build auto-fit so the view opens where the user left it. The
/// `view_restored` guard makes this idempotent. status: graph-view-state-persist
pub(crate) fn apply_persisted_view(app: &mut AppState, key: &str) {
    let saved = app.session.code_graph_views.get(key).cloned();
    let Some(view) = app.panels.code_graph.get_mut(key) else {
        return;
    };
    if view.view_restored {
        return;
    }
    view.view_restored = true;
    let Some(saved) = saved else {
        return;
    };
    // Don't restore onto a view that failed to bind (no graph to lay out).
    if view.error.is_some() {
        return;
    }
    view.scope = crate::panels::graph_nav::scope_from_persist_str(&saved.scope);
    view.show_calls = saved.show_calls;
    view.show_impls = saved.show_impls;
    view.show_orphans = saved.show_orphans;
    view.size_by_loc = saved.size_by_loc;
    view.selected = saved.selected.clone();
    for (kind, on) in &mut view.kind_filter {
        *on = !saved.hidden_kinds.contains(kind);
    }
    // Seed the engine's warm-layout positions + view before the rebuild so the
    // layout morphs onto the saved shape. `rebuild_display` runs `recompute_layout`,
    // which warm-seeds from these.
    let snap = crate::panels::graph::view_state_to_snapshot(&saved.engine);
    view.engine.restore_view(&snap);
    rebuild_display(view);
    view.engine.needs_fit = false;
}

/// Find-or-focus a code-graph tab for `source` (a project note or a `.scip` index), opening one if
/// none exists.
pub fn open(app: &mut AppState, source: CodeSource) -> TabId {
    if let Some(existing) = app
        .session
        .tabs
        .iter()
        .find(|t| matches!(&t.kind, TabKind::CodeGraph { source: s } if *s == source))
    {
        let id = existing.id;
        app.session.active_tab = Some(id);
        return id;
    }
    let id = app.next_tab_id();
    app.session.tabs.push(Tab::new(id, TabKind::CodeGraph { source }, true));
    app.session.active_tab = Some(id);
    id
}

/// True if the `.md` at `rel` is a project note (`hiker.kind: project`). Reads + parses the file —
/// called on click / menu open, never per-frame (mirrors `is_board_doc`).
pub fn is_project_doc(app: &AppState, rel: &str) -> bool {
    if !rel.ends_with(".md") {
        return false;
    }
    app.vault_session
        .vault
        .read_file(rel)
        .ok()
        .map(|src| Project::parse(&src, std::path::Path::new(rel)).is_ok())
        .unwrap_or(false)
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState, _tab_id: TabId, source: &CodeSource) {
    let short = source.path().rsplit('/').next().unwrap_or(source.path());
    ui.heading(format!("Code graph · {short}"));

    let key = source.key();
    if !app.panels.code_graph.contains_key(&key) {
        let view = build_view(app, source);
        // Seed the global nav stack with this view's initial (overview) drill location, so a Back
        // after the first drill returns to the overview rather than skipping straight out of the
        // code tab. Skipped while a back/forward is driving (`nav.locked`). status: code-graph-view-source
        if !app.session.nav.locked {
            let (selected, scope) = view.nav_snapshot();
            app.session.nav.push(NavTarget::CodeGraphNode {
                source: source.clone(),
                selected,
                scope,
            });
        }
        app.panels.code_graph.insert(key.clone(), view);
    }

    // Apply any persisted view state (display controls + engine view) onto the
    // freshly-built view, once. Guarded by `view_restored` so it's a no-op on
    // later frames. status: graph-view-state-persist
    apply_persisted_view(app, &key);

    // Consume a pending spec-light (the vault graph's spec → code-graph jump
    // arriving before this view was built). status: vault-graph-spec-drift-badge
    if let Some((_, spec)) =
        app.panels.code_graph_pending_light.take_if(|(k, _)| k == &key)
    {
        light_spec(app, &key, &spec);
    }

    // Surface a load error and stop.
    if let Some(view) = app.panels.code_graph.get(&key) {
        if let Some(err) = &view.error {
            ui.colored_label(egui::Color32::RED, err);
            return;
        }
    }

    // Ctrl+F opens the "Find / jump to node" popup (independent of the editor's Ctrl+F — only this
    // tab is showing). Read it the same way the toolbar reads its shortcuts. status: graph-find-popup
    if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
        if let Some(view) = app.panels.code_graph.get_mut(&key) {
            view.find.open();
        }
    }

    // The Esc ladder's middle rung (`interaction.md` [keyboard-esc-ladder]):
    // Esc pops focus-nav back to the overview — but an open find popup or
    // latched node menu consumes Esc first (both close on it), so capture
    // whether one was open BEFORE they process this frame's input.
    let esc_taken_by_popup = app
        .panels
        .code_graph
        .get(&key)
        .is_some_and(|v| v.find.is_open() || v.node_menu.is_some());

    // Snapshot the three settle-triggering fields before any interaction this frame, so we can
    // detect a user-driven change afterwards and record it onto the global nav stack.
    let before = app.panels.code_graph.get(&key).map(View::nav_snapshot);

    let toolbar = toolbar(ui, app, &key);
    let mut dirty = toolbar.dirty;
    // A toolbar ⟵/⟶ press or a mouse Extra button drives the GLOBAL Back/Forward (`nav_go`), which
    // restores the drill location via `navigate_to` → `apply_nav_target` (rebuilds the display
    // itself). Such a restore must NOT be re-recorded as a fresh entry, so flag it. (Alt+←/→ +
    // Mod-[/] are handled by the global keybind path, not here.)
    let nav_restoring = toolbar.nav_delta.is_some();
    if let Some(delta) = toolbar.nav_delta {
        crate::editor_pane::nav_go(app, delta);
    }
    // Overlay changes are pure recolors: invalidate the GPU paint cache (fills
    // are baked into the cached affine batch) and pulse a freshly-lit spec's
    // nodes through the fluid highlight — never a relayout.
    if toolbar.recolor || toolbar.pulse {
        if let Some(view) = app.panels.code_graph.get_mut(&key) {
            if toolbar.recolor {
                view.engine.invalidate_paint_cache();
            }
            if toolbar.pulse {
                pulse_lit(view);
            }
        }
    }
    detail_line(ui, app, &key);

    let clicked = render_canvas(ui, app, &key);
    node_menu_ui(ui, app, &key);
    // A find-popup pick selects the chosen node like a click; from the overview it also
    // switches to hops scope so the picked node is revealed even when the kind filter or
    // collapse would hide it. Back/Forward then steps through popup jumps too.
    // status: graph-find-popup
    let jumped = find_popup(ui, app, &key);
    if let Some(view) = app.panels.code_graph.get_mut(&key) {
        if let Some(id) = clicked.clone().or(jumped.clone()) {
            // Clicking always selects; the scope dial decides whether the display
            // recenters. Re-selecting a neighbour in hops scope navigates node-by-node.
            view.selected = Some(id);
            if jumped.is_some() && view.scope == Scope::Overview {
                view.scope = Scope::Hops(2);
            }
        }
        // Esc = up one level: pop a hops focus back to the overview (toolbar
        // Back still walks the global nav stack). The shared gate skips it
        // when this frame's Esc went to the find popup / node menu, and while
        // any text field holds focus. [keyboard-esc-ladder]
        if crate::panels::graph_nav::esc_pops_focus(ui, view.scope, esc_taken_by_popup) {
            view.scope = Scope::Overview;
        }
        dirty |= view.applied != view.current_applied();
    }

    if dirty {
        if let Some(view) = app.panels.code_graph.get_mut(&key) {
            rebuild_display(view);
        }
    }

    // Record a user-driven change (drill / hops / level) onto the GLOBAL nav stack — but never a
    // Back/Forward restore (`nav_restoring`, which just moved the cursor) nor any frame where a
    // back/forward is driving navigation (`nav.locked`). `NavState::push` dedupes consecutive equal
    // targets, so pushing the post-change location each settle is fine. status: code-graph-view-source
    if !nav_restoring && !app.session.nav.locked {
        if let (Some(before), Some(view)) = (before, app.panels.code_graph.get(&key)) {
            let after = view.nav_snapshot();
            if after != before {
                let (selected, scope) = after;
                app.session.nav.push(NavTarget::CodeGraphNode {
                    source: source.clone(),
                    selected,
                    scope,
                });
            }
        }
    }
    summary(ui, app, &key);
}

/// Build the view: resolve the source → bind a SCIP adapter → `code_graph()` → scope → seed the
/// engine layout. Errors are stored on the view (rendered by `show`) rather than panicking.
fn build_view(app: &mut AppState, source: &CodeSource) -> View {
    let empty = || CodeGraph { nodes: Vec::new(), edges: Vec::new() };
    let mut engine = graph_view::State::new(Style::flat(), LayoutKind::ForceDirected);
    // The code graph colours nodes by entity kind, not the flat vault palette — hide
    // the inapplicable "Nodes"/"Active note" colour pickers in the view menu.
    engine.palette_editable = false;
    let mut view = View {
        engine,
        adapter: None,
        src: SourceId(String::new()),
        full: empty(),
        graph: empty(),
        scope: Scope::Overview,
        kind_filter: Vec::new(),
        show_calls: true,
        show_impls: true,
        show_orphans: false,
        size_by_loc: false,
        find: crate::widgets::autocomplete_picker::PickerState::default(),
        applied: Applied {
            scope: Scope::Overview,
            anchor: None,
            calls: true,
            impls: true,
            orphans: false,
            kinds: Vec::new(),
        },
        selected: None,
        overlay: crate::panels::code_governance::Overlay::default(),
        node_menu: None,
        error: None,
        view_restored: false,
    };
    let built = match source {
        CodeSource::Project(note) => bind_project(app, note),
        CodeSource::Index(scip) => bind_index(app, scip),
    };
    match built {
        Ok((adapter, src)) => {
            view.full = adapter.code_graph();
            view.kind_filter = kind_filter_for(&view.full);
            view.adapter = Some(adapter);
            view.src = src;
            rebuild_display(&mut view); // filters/collapses `full` into `graph` + lays out
        }
        Err(e) => view.error = Some(e),
    }
    view
}

/// The kind-filter rows for `full`: every distinct node kind present in the data, sorted —
/// the filter section auto-populates from what was fed, nothing hardcoded. Defaults: all
/// visible for small graphs; only structural objects (types/modules) for large ones, the
/// legibility default the old fixed level tiers provided. status: code-graph-kind-filters
fn kind_filter_for(full: &CodeGraph) -> Vec<(String, bool)> {
    const ALL_VISIBLE_MAX: usize = 2_000;
    let mut kinds: Vec<String> = full.nodes.iter().map(|n| n.kind.clone()).collect();
    kinds.sort();
    kinds.dedup();
    let small = full.nodes.len() <= ALL_VISIBLE_MAX;
    kinds
        .into_iter()
        .map(|k| {
            let on = small || matches!(k.as_str(), "code:type" | "code:module");
            (k, on)
        })
        .collect()
}

/// Recompute the displayed graph from `full` per the current scope + kind filter: scope to the
/// selected node's n-hop neighbourhood (when `Scope::Hops`), then collapse hidden kinds (members
/// lift to their nearest visible ancestor, edges aggregate), then re-lay-out. Reuses the shared
/// `hiker_code::collapse` helper — the policy (what's visible) lives here.
/// status: code-graph-scope-hops
fn rebuild_display(view: &mut View) {
    // Entering/leaving hops scope (or re-anchoring it) swaps the whole node set (overview ⇄ a
    // small neighbourhood). Warm-seeding the new set from the old layout would scatter it
    // across the overview's wide spread (and the fit would zoom past the few nodes), so lay
    // out fresh on any anchor change. Hop-count / filter tweaks keep the warm morph.
    let anchor = view.anchor();
    if view.applied.anchor != anchor {
        view.engine.reset_layout_history();
    }
    // Hops scope: BFS over the full (unfiltered) adjacency, so hop distance is structural —
    // the kind filter decides what's *drawn*, with hidden in-scope nodes still lifting their
    // edges through `collapse`. A cleared/stale selection falls back to the overview display.
    let anchor_idx =
        anchor.as_ref().and_then(|id| view.full.nodes.iter().position(|n| &n.id == id));
    let mask = match (view.scope, anchor_idx) {
        (Scope::Hops(d), Some(fi)) => Some(crate::panels::graph_nav::hop_mask(
            view.full.nodes.len(),
            view.full.edges.iter().map(|&(a, b, _)| (a, b)),
            fi,
            d as usize,
        )),
        _ => None,
    };
    let in_overview = mask.is_none();
    let visible = |i: usize| {
        let n = &view.full.nodes[i];
        let in_scope = mask.as_ref().is_none_or(|m| m[i]);
        // The anchor itself always shows, even when its kind is filtered out — a
        // neighbourhood without its centre would read as a bug.
        in_scope && (view.kind_on(&n.kind) || Some(&n.id) == anchor.as_ref())
    };
    let collapsed: CollapsedGraph = collapse(&view.full, visible);
    let materialized = materialize(&view.full, &collapsed);
    // Orphans (degree-0 nodes) lay out as a noisy ring; hide them in the overview unless
    // toggled on. A hops neighbourhood keeps them — they're within reach of the anchor by
    // construction, and dropping the anchor's filtered-down remainder would empty the view.
    view.graph = if in_overview && !view.show_orphans {
        drop_orphans(&materialized)
    } else {
        materialized
    };
    view.applied = view.current_applied();
    let source = CodeGraphSource {
        graph: &view.graph,
        show_calls: view.show_calls,
        show_impls: view.show_impls,
        size_by_loc: view.size_by_loc,
        overlay: &view.overlay,
    };
    view.engine.recompute_layout(&source, CODE_CFG);
}

/// Materialize a collapsed subset back into a `CodeGraph` for rendering (parents are dropped — they
/// carry no meaning in the collapsed view; click→expand maps back through `full` by node id).
fn materialize(full: &CodeGraph, c: &CollapsedGraph) -> CodeGraph {
    let nodes = c
        .nodes
        .iter()
        .map(|&i| {
            let mut n = full.nodes[i].clone();
            n.parent = None;
            n
        })
        .collect();
    CodeGraph { nodes, edges: c.edges.clone() }
}

/// Drop every node with in+out degree 0 (disconnected "orphans") and remap the surviving edges.
/// Orphans lay out as a noisy ring around the connected core, so the overview hides them by default.
fn drop_orphans(g: &CodeGraph) -> CodeGraph {
    let mut degree = vec![0usize; g.nodes.len()];
    for &(a, b, _) in &g.edges {
        degree[a] += 1;
        degree[b] += 1;
    }
    let mut remap = vec![usize::MAX; g.nodes.len()];
    let mut nodes = Vec::new();
    for (i, n) in g.nodes.iter().enumerate() {
        if degree[i] > 0 {
            remap[i] = nodes.len();
            nodes.push(n.clone());
        }
    }
    let edges = g.edges.iter().map(|&(a, b, k)| (remap[a], remap[b], k)).collect();
    CodeGraph { nodes, edges }
}

/// Bind a project note: parse it → first `repo` source descriptor → SCIP adapter (the consumer
/// composes; hiker-projects is decoupled). Only the SCIP backend is implemented today.
fn bind_project(app: &AppState, note: &str) -> Result<(ScipAdapter, SourceId), String> {
    let text = app.vault_session.vault.read_file(note).map_err(|e| format!("read note: {e}"))?;
    let project =
        Project::parse(&text, std::path::Path::new(note)).map_err(|e| format!("project note: {e}"))?;
    let repo = project
        .repo_sources()
        .next()
        .ok_or_else(|| "project note has no `kind: repo` source".to_string())?;
    if repo.backend != Backend::Scip {
        return Err("only the SCIP backend is supported (LSP is not implemented yet)".to_string());
    }
    // CODE-IN-VAULT trust invariant: hiker only reads inside the vault. The note's `index`/`root`
    // may be relative (resolve against the vault root) or absolute external paths (rejected).
    let vault_root = app.vault_session.vault.root();
    let index = resolve_in_vault(vault_root, &repo.index);
    let root = resolve_in_vault(vault_root, &repo.root);
    require_in_vault(vault_root, &index)?;
    require_in_vault(vault_root, &root)?;
    let src = SourceId(repo.repo_id.clone());
    let adapter =
        ScipAdapter::load(&index, &root, src.clone()).map_err(|e| format!("load index: {e}"))?;
    Ok((adapter, src))
}

/// Bind a `.scip` opened directly from the file tree (no project note). The index path is
/// vault-relative; the repo root for previews defaults to the index's own directory (correct for
/// `rust-analyzer scip .`). Reads stay vault-clamped by the adapter (`safe_join`).
fn bind_index(app: &AppState, scip_rel: &str) -> Result<(ScipAdapter, SourceId), String> {
    let vault_root = app.vault_session.vault.root();
    let abs = vault_root.join(scip_rel);
    let root = abs.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
    // The scip path is already vault-relative (joined to the vault root above); validate defensively
    // so a crafted `..`-laden relative path can't escape the vault (CODE-IN-VAULT trust invariant).
    require_in_vault(vault_root, &abs)?;
    require_in_vault(vault_root, &root)?;
    let stem = std::path::Path::new(scip_rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("index")
        .to_string();
    let src = SourceId(stem);
    let adapter =
        ScipAdapter::load(&abs, &root, src.clone()).map_err(|e| format!("load index: {e}"))?;
    Ok((adapter, src))
}

/// Resolve a configured path against the vault root: relative paths join the vault root; absolute
/// paths are returned as-is (and then rejected by `require_in_vault` if they escape the vault).
pub(crate) fn resolve_in_vault(vault_root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        vault_root.join(p)
    }
}

/// CODE-IN-VAULT trust invariant gate: error unless `p` lives inside `vault_root`.
pub(crate) fn require_in_vault(vault_root: &Path, p: &Path) -> Result<(), String> {
    if within_vault(vault_root, p) {
        Ok(())
    } else {
        Err(format!(
            "index/repo must live inside the vault (CODE-IN-VAULT trust invariant): {}",
            p.display()
        ))
    }
}

/// Whether `p` resolves inside `vault_root`. Prefers canonicalization (resolves symlinks + `..`);
/// falls back to a lexical check (no `..`/root-escapes, then `starts_with`) when either path can't
/// be canonicalized (e.g. the target doesn't exist yet).
fn within_vault(vault_root: &Path, p: &Path) -> bool {
    match (vault_root.canonicalize(), p.canonicalize()) {
        (Ok(root), Ok(path)) => path.starts_with(root),
        _ => {
            let root = lexical_normalize(vault_root);
            // A lexical path containing `..` can't be safely cleared as in-vault.
            if p.components().any(|c| matches!(c, Component::ParentDir)) {
                return false;
            }
            lexical_normalize(p).starts_with(&root)
        }
    }
}

/// Lexically normalize a path: drop `.` and collapse `..` against preceding normal components,
/// without touching the filesystem. Used only by the `within_vault` lexical fallback.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Outcome of a toolbar pass: whether the displayed graph must be rebuilt, and the GLOBAL nav delta
/// a Back/Forward control requested (`-1` back, `+1` forward) — the caller drives `nav_go` after the
/// toolbar returns (it borrows `app.session.nav`, which the toolbar's `view` borrow can't hold).
#[derive(Default)]
struct ToolbarResult {
    dirty: bool,
    nav_delta: Option<i32>,
    /// Node fills changed (overlay mode / lighting / diff reload) — the caller
    /// invalidates the engine's GPU paint cache (no relayout).
    recolor: bool,
    /// The lit spec changed — the caller pulses the lit nodes through the fluid
    /// highlight.
    pulse: bool,
}

/// Toolbar: the scope dial (Overview / 1 / 2 / 3 hops around the selection), the auto-populated
/// entity-kind filter, edge-type toggles (Calls / Implements), reset-view, and Back/Forward
/// navigation. Returns whether the displayed graph must be rebuilt and the GLOBAL nav delta any
/// Back/Forward control requested.
fn toolbar(ui: &mut egui::Ui, app: &mut AppState, key: &str) -> ToolbarResult {
    let can_back = app.session.nav.can_back();
    let can_fwd = app.session.nav.can_forward();
    // The overlay section reads the vault git engine + root; clone them out
    // before the panel-map borrow.
    let git = app.vault_session.services.git_sync.clone();
    let vault_root = app.vault_session.vault.root().to_path_buf();
    let Some(view) = app.panels.code_graph.get_mut(key) else { return ToolbarResult::default() };
    let mut menu_relayout = false;
    let mut nav_delta = None;
    let mut overlay = crate::panels::code_governance::OverlayResult::default();
    ui.horizontal_wrapped(|ui| {
        nav_delta = crate::panels::graph_nav::nav_controls(ui, can_back, can_fwd);
        ui.separator();
        // Find / jump to node: opens the shared autocomplete picker over the full
        // node list (also bound to Ctrl+F). A pick drills to the symbol — finding it
        // even when the overview would hide it. status: graph-find-popup
        if ui
            .small_button("Find")
            .on_hover_text("Find / jump to node (Ctrl+F)")
            .clicked()
        {
            view.find.open();
        }
        ui.separator();
        // The scope dial: Overview, or 1/2/3 hops around the selected node. The hop
        // settings need a selection to anchor on — the shared dial disables them
        // (with a hint) until one exists. status: code-graph-scope-hops
        let anchor_label = view.selected.as_ref().map(|id| {
            view.full
                .nodes
                .iter()
                .find(|n| &n.id == id)
                .map_or("?", |n| n.name.as_str())
                .to_string()
        });
        crate::panels::graph_nav::scope_dial(ui, &mut view.scope, anchor_label.as_deref());
        ui.separator();
        // The kind filter: one toggle per entity kind PRESENT in the data (auto-populated
        // at bind, nothing hardcoded), colour-keyed to the nodes. Hidden kinds collapse —
        // their members lift edges to the nearest visible ancestor rather than vanishing.
        // status: code-graph-kind-filters
        for (kind, on) in &mut view.kind_filter {
            let label = kind.strip_prefix("code:").unwrap_or(kind);
            let text = egui::RichText::new(label).small().color(kind_color(kind));
            if ui.selectable_label(*on, text).clicked() {
                *on = !*on;
            }
        }
        ui.separator();
        // The fill-overlay dial + the spec-lighting dropdown. Pure recolor: the
        // section never flags `dirty` (no relayout), only `recolor`/`pulse`.
        if let Some(adapter) = &view.adapter {
            overlay = crate::panels::code_governance::toolbar_section(
                ui,
                &mut view.overlay,
                adapter,
                &view.src,
                git.as_deref(),
                &vault_root,
            );
        }
        ui.separator();
        let mut extra: Vec<(&str, &mut bool)> = vec![
            ("Calls", &mut view.show_calls),
            ("Implements", &mut view.show_impls),
            ("Orphans", &mut view.show_orphans),
            ("Size by LOC", &mut view.size_by_loc),
        ];
        menu_relayout = view.engine.view_options_menu(
            ui,
            crate::icons::ICONS.image(crate::icons::Icon::Eye),
            &mut extra,
        );
        if ui.small_button("Reset view").clicked() {
            view.engine.needs_fit = true;
        }
    });
    // Rebuild when scope / anchor / edge-toggles / kind filter changed since the last build.
    let dirty = menu_relayout || view.applied != view.current_applied();
    ToolbarResult { dirty, nav_delta, recolor: overlay.recolor, pulse: overlay.pulse }
}

/// Read-only click→detail (`code-node-detail`): the selected node's kind + definition `file:line`,
/// resolved through the adapter (no new editable tab). Looks the node up in `full` (by id) so detail
/// works regardless of the collapsed display.
fn detail_line(ui: &mut egui::Ui, app: &AppState, key: &str) {
    let Some(view) = app.panels.code_graph.get(key) else { return };
    let Some(id) = &view.selected else { return };
    let Some(node) = view.full.nodes.iter().find(|n| &n.id == id) else { return };
    let Some(adapter) = &view.adapter else { return };
    let handle = NodeHandle { source: view.src.clone(), id: node.id.clone() };
    let loc = adapter
        .locate(&handle)
        .map(|l| format!("{}:{}", l.file, l.start_line + 1))
        .unwrap_or_else(|| node.file.clone());
    // Governance detail (once the rollup is loaded): folded state + governing
    // specs, with any planned/partial status spelled out — the per-node read the
    // fill color summarizes.
    let gov = view
        .overlay
        .detail_fragment(&node.id)
        .map(|f| format!("  ·  {f}"))
        .unwrap_or_default();
    ui.label(
        egui::RichText::new(format!("→ {}  ·  {}  @ {}{gov}  (scope 1/2/3 shows this node's neighbourhood)", node.name, node.kind, loc))
            .color(theme::muted())
            .small(),
    );
}

/// Drive the engine for one frame; returns the clicked node id (its SCIP moniker), if any.
fn render_canvas(ui: &mut egui::Ui, app: &mut AppState, key: &str) -> Option<String> {
    let view = app.panels.code_graph.get_mut(key)?;
    let View {
        engine, graph, show_calls, show_impls, size_by_loc, selected, overlay, node_menu, ..
    } = view;
    // Mark the clicked / drilled-into node so the engine keeps its edges
    // highlighted (the "selected node's edges" view-menu option).
    engine.selected_node = selected
        .as_ref()
        .and_then(|id| graph.nodes.iter().position(|n| &n.id == id));
    let source = CodeGraphSource {
        graph: &*graph,
        show_calls: *show_calls,
        show_impls: *show_impls,
        size_by_loc: *size_by_loc,
        overlay,
    };
    let size = egui::vec2(ui.available_width(), (ui.available_height() - 24.0).max(50.0));
    let clicked = ui
        .allocate_ui(size, |ui| {
            engine.ui(ui, &source, |p, r, t, b, a| {
                crate::panels::graph::paint_preview_card(p, r, t, b, a);
            })
        })
        .inner;
    // Right-click a node → latch its MENU (never a direct action, per
    // `interaction.md` [rightclick-menu-always]); `node_menu_ui` renders it and
    // applies the picked verb.
    if let Some(moniker) = engine.take_secondary_click() {
        let pos = ui.ctx().pointer_latest_pos().unwrap_or_else(|| ui.min_rect().center());
        *node_menu = Some((moniker, pos));
    }
    clicked
}

/// Render the latched node context menu and apply the picked verb: Open source
/// (the read-only code view), Open diff vs HEAD (the editor tab with the
/// `GitRef` overlay — graph shows where the change is, the click drops into
/// hunks), or Light spec.
fn node_menu_ui(ui: &mut egui::Ui, app: &mut AppState, key: &str) {
    use crate::panels::code_governance::{self, NodeAction};
    let has_git = app.vault_session.services.git_sync.is_some();
    // Build the menu inputs + render under a short view borrow, then apply the
    // picked action with `app` free.
    let picked = {
        let Some(view) = app.panels.code_graph.get_mut(key) else { return };
        let Some((moniker, _)) = view.node_menu.clone() else { return };
        let file = view
            .full
            .nodes
            .iter()
            .find(|n| n.id == moniker)
            .map(|n| n.file.clone())
            .unwrap_or_default();
        let repo_root = view.adapter.as_ref().map(|a| a.repo_root().to_path_buf());
        let diff = view.overlay.diff_verb(&file, has_git);
        let specs: Vec<String> = view
            .overlay
            .governance()
            .map(|g| g.specs_of(&moniker).to_vec())
            .unwrap_or_default();
        let menu = code_governance::node_menu(&moniker, diff, &specs);
        crate::item_menu::latched_menu_popup(
            ui,
            egui::Id::new("code-graph-node-menu"),
            &mut view.node_menu,
            menu,
        )
        .and_then(|action| Some((action, file, repo_root?)))
    };
    let Some((action, file, repo_root)) = picked else { return };
    match action {
        // The node's on-disk source (repo_root + file), read-only. The repo root
        // is vault-clamped at load, so the join stays inside the vault.
        NodeAction::OpenSource => {
            let abs = repo_root.join(&file);
            let vault_root = app.vault_session.vault.root().to_path_buf();
            match abs.strip_prefix(&vault_root) {
                Ok(rel) => crate::editor_pane::open_code_file(app, &rel.to_string_lossy()),
                Err(_) => app.push_toast(
                    format!("Source file is outside the vault: {}", abs.display()),
                    crate::state::ToastLevel::Error,
                ),
            }
        }
        // The (a)+(b) tie-in: the same `open_diff_tab` the diff-summary panel's
        // rows use, against HEAD (the diff overlay's base).
        NodeAction::OpenDiff => {
            let abs = repo_root.join(&file);
            let vault_root = app.vault_session.vault.root().to_path_buf();
            match abs.strip_prefix(&vault_root) {
                Ok(rel) => {
                    let rel = rel.to_string_lossy().into_owned();
                    crate::panels::git_diff::open_diff_tab(app, &rel, "HEAD", false);
                }
                Err(_) => app.push_toast(
                    format!("Source file is outside the vault: {}", abs.display()),
                    crate::state::ToastLevel::Error,
                ),
            }
        }
        // Same effect as the toolbar dropdown; also flips the overlay to
        // governance so the lighting is visible.
        NodeAction::LightSpec(spec) => {
            if let Some(view) = app.panels.code_graph.get_mut(key) {
                let View { overlay, adapter, src, .. } = view;
                if let Some(adapter) = adapter {
                    overlay.mode = code_governance::OverlayMode::Governance;
                    overlay.light(Some(spec), adapter, src);
                }
                view.engine.invalidate_paint_cache();
                pulse_lit(view);
            }
        }
    }
}

/// Pulse the lit spec's nodes (mapped onto the displayed graph) through the
/// fluid highlight — the one-shot "light up" moment; the steady signal is the
/// dimmed-fill contrast. Collapsed-away lit members don't pulse (their visible
/// ancestor carries the structure, not the spec claim).
fn pulse_lit(view: &mut View) {
    let idx: Vec<usize> = match view.overlay.lit_ids() {
        Some(lit) => view
            .graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| lit.contains(&n.id))
            .map(|(i, _)| i)
            .collect(),
        None => return,
    };
    view.engine.pulse_nodes(&idx);
}

/// Drive one frame of the "Find / jump to node" popup. While the view's picker is open, render the
/// shared autocomplete picker over the full node list; returns the chosen node id (a SCIP moniker)
/// on a pick, for the caller to drill to. status: graph-find-popup
fn find_popup(ui: &mut egui::Ui, app: &mut AppState, key: &str) -> Option<String> {
    use crate::widgets::autocomplete_picker::{self, PickerOutcome};
    let view = app.panels.code_graph.get_mut(key)?;
    if !view.find.is_open() {
        return None;
    }
    // Split-borrow: the source reads `full.nodes` while the picker mutably drives `find`.
    let View { find, full, .. } = view;
    let source = crate::panels::graph_find::CodeNodeFindSource::new(&full.nodes);
    match autocomplete_picker::show(ui, find, &source) {
        PickerOutcome::Selected(item) => Some(item.insert.to_string()),
        PickerOutcome::Cancelled | PickerOutcome::Open => None,
    }
}

fn summary(ui: &mut egui::Ui, app: &AppState, key: &str) {
    let Some(view) = app.panels.code_graph.get(key) else { return };
    let Some(adapter) = &view.adapter else { return };
    // In governance mode, append the coverage breakdown over the FULL graph —
    // the `coverage_specs` altitude split, numerically, next to its spatial
    // rendering.
    let gov = match (view.overlay.mode, view.overlay.governance()) {
        (crate::panels::code_governance::OverlayMode::Governance, Some(gov)) => {
            let [ok, drifted, missing, ungoverned] = crate::panels::code_governance::gov_counts(
                gov,
                view.full.nodes.iter().map(|n| n.id.as_str()),
            );
            format!(" · spec: {ok} ok / {drifted} drifted / {missing} missing / {ungoverned} ungoverned")
        }
        _ => String::new(),
    };
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(format!(
            "[{}] showing {} of {} entities · {} edges · impl-edges: {}{gov}",
            adapter.tool(),
            view.graph.nodes.len(),
            view.full.nodes.len(),
            view.graph.edges.len(),
            adapter.impl_source(),
        ))
        .color(theme::muted())
        .small(),
    );
}

/// The code adapter from a [`CodeGraph`] to the shared graph engine. Maps entity kind → shape
/// (constant across overlays) and routes the fill through the active overlay (kind color /
/// governance state / diff status — `code_governance::Overlay::node_fill`), and filters edges by
/// the active toggles.
struct CodeGraphSource<'a> {
    graph: &'a CodeGraph,
    show_calls: bool,
    show_impls: bool,
    size_by_loc: bool,
    /// Fill/badge policy.
    overlay: &'a crate::panels::code_governance::Overlay,
}

fn kind_color(kind: &str) -> egui::Color32 {
    match kind {
        "code:type" => egui::Color32::from_rgb(0x4f, 0x83, 0xcc),
        "code:function" => egui::Color32::from_rgb(0x4c, 0xaf, 0x72),
        "code:method" => egui::Color32::from_rgb(0x3f, 0xb6, 0xa8),
        "code:module" => egui::Color32::from_rgb(0x95, 0x75, 0xcd),
        "code:macro" => egui::Color32::from_rgb(0xc9, 0x8b, 0x3a),
        "code:constant" => egui::Color32::from_rgb(0xc7, 0x5b, 0x6d),
        "code:field" => egui::Color32::from_rgb(0xb0, 0x89, 0x4a),
        _ => egui::Color32::from_rgb(0x9e, 0x9e, 0x9e),
    }
}

/// Zoom at/above which a node of `kind` reveals its label — the semantic label
/// ladder. Low LOD (zoomed out) shows only the top of the hierarchy (modules =
/// 0.0, always); types/functions/methods then leaf consts/fields appear as you
/// zoom in. Dedup of identical names keeps the always-on module labels from
/// piling up. Thresholds are vs the affine `view.zoom` (range ~0.005..6).
fn label_min_zoom_for(kind: &str) -> f32 {
    match kind {
        "code:module" => 0.0,
        "code:type" => 0.15,
        "code:macro" => 0.25,
        "code:function" => 0.35,
        "code:method" => 0.5,
        "code:constant" | "code:field" => 0.8,
        _ => 0.45,
    }
}

/// Font-size multiplier per kind, so high-level nodes read as larger text —
/// crates/modules biggest, leaves smallest.
fn label_scale_for(kind: &str) -> f32 {
    match kind {
        "code:module" => 1.5,
        "code:type" => 1.15,
        "code:constant" | "code:field" => 0.9,
        _ => 1.0,
    }
}

const fn edge_kept(kind: EdgeKind, show_calls: bool, show_impls: bool) -> bool {
    match kind {
        EdgeKind::Implements => show_impls,
        _ => show_calls, // Calls / TypeRef / Imports ride the "Calls" toggle for v1
    }
}

impl Source for CodeGraphSource<'_> {
    fn node_count(&self) -> usize {
        self.graph.nodes.len()
    }

    fn nodes(&self, positions: &[egui::Vec2], _style: &Style) -> Vec<NodeDescriptor> {
        let mut degree = vec![0u32; self.graph.nodes.len()];
        for &(a, b, _) in &self.graph.edges {
            degree[a] += 1;
            degree[b] += 1;
        }
        let maxd = degree.iter().copied().max().unwrap_or(1).max(1) as f32;
        // LOC weighting: radius ∝ √(lines) (area ∝ LOC reads better than radius ∝ LOC,
        // where a 1500-line module would dwarf everything). Normalised to the graph's
        // largest body so the range stays ~4..13px like the degree weighting.
        let max_loc =
            self.graph.nodes.iter().map(|n| n.lines).max().unwrap_or(1).max(1) as f32;
        self.graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i < positions.len())
            .map(|(index, n)| NodeDescriptor {
                index,
                world_pos: positions[index],
                radius: if self.size_by_loc {
                    4.0 + 9.0 * (n.lines as f32 / max_loc).sqrt()
                } else {
                    4.0 + 7.0 * (degree[index] as f32 / maxd)
                },
                // Kind → shape stays constant across overlays; the FILL is the
                // overlay's channel (kind color / governance / diff), the badge
                // marks planned/partial-spec nodes in governance mode, and its
                // top-left twin marks nodes with open bugs.
                shape: if n.kind == "code:type" { NodeShape::Square } else { NodeShape::Circle },
                fill: self.overlay.node_fill(kind_color(&n.kind), &n.id, &n.file),
                resting_stroke: egui::Stroke::NONE,
                hover_stroke: egui::Stroke::new(1.5, egui::Color32::WHITE),
                badge: self.overlay.node_badge(&n.id),
                bug_badge: self.overlay.node_bug_badge(&n.id),
                label: Some(n.name.clone()),
                label_min_zoom: label_min_zoom_for(&n.kind),
                label_scale: label_scale_for(&n.kind),
                click_path: Some(n.id.clone()),
                tooltip: Some(n.file.clone()),
            })
            .collect()
    }

    fn edges(&self) -> Vec<(u32, u32)> {
        self.graph
            .edges
            .iter()
            .filter(|&&(_, _, k)| edge_kept(k, self.show_calls, self.show_impls))
            .map(|&(a, b, _)| (a as u32, b as u32))
            .collect()
    }

    fn layout_tree(&self, _kind: LayoutKind) -> LayoutTree {
        LayoutTree::from_parents(&vec![None; self.graph.nodes.len()])
    }

    fn node_key(&self, index: usize) -> Option<String> {
        self.graph.nodes.get(index).map(|n| n.id.clone())
    }

    fn preview_for(&self, index: usize) -> Option<(String, String)> {
        let n = self.graph.nodes.get(index)?;
        Some((n.name.clone(), format!("{} · {}", n.kind, n.file)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hiker_code::GraphNode;

    fn node(id: &str, kind: &str) -> GraphNode {
        GraphNode {
            id: id.to_string(),
            name: id.to_string(),
            kind: kind.to_string(),
            file: format!("{id}.rs"),
            start_line: 0,
            lines: 1,
            parent: None,
        }
    }

    /// Minimal view over a 3-node line graph A—B—C (two types + a method), for the pure
    /// scope/filter rebuild tests (no egui context needed).
    fn view() -> View {
        let full = CodeGraph {
            nodes: vec![node("A", "code:type"), node("B", "code:type"), node("C", "code:method")],
            edges: vec![(0, 1, EdgeKind::Calls), (1, 2, EdgeKind::Calls)],
        };
        let kind_filter = kind_filter_for(&full);
        let empty = CodeGraph { nodes: Vec::new(), edges: Vec::new() };
        View {
            engine: graph_view::State::new(Style::flat(), LayoutKind::ForceDirected),
            adapter: None,
            src: SourceId(String::new()),
            full,
            graph: empty,
            scope: Scope::Overview,
            kind_filter,
            show_calls: true,
            show_impls: true,
            show_orphans: false,
            size_by_loc: false,
            find: crate::widgets::autocomplete_picker::PickerState::default(),
            applied: Applied {
                scope: Scope::Overview,
                anchor: None,
                calls: true,
                impls: true,
                orphans: false,
                kinds: Vec::new(),
            },
            selected: None,
            overlay: crate::panels::code_governance::Overlay::default(),
            node_menu: None,
            error: None,
            view_restored: false,
        }
    }

    /// `apply_nav_target` (the global Back/Forward apply side) sets selection + scope and
    /// rebuilds the displayed graph as the selection's n-hop neighbourhood.
    #[test]
    fn apply_nav_target_sets_selection_and_hops_neighbourhood() {
        let mut v = view();
        apply_nav_target(&mut v, Some("B".to_string()), Scope::Hops(1));
        assert_eq!(v.selected.as_deref(), Some("B"));
        assert_eq!(v.scope, Scope::Hops(1));
        // 1-hop neighbourhood of B is {A, B, C}.
        let names: Vec<&str> = v.graph.nodes.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"A") && names.contains(&"B") && names.contains(&"C"));
    }

    /// Restoring the overview keeps the whole (kind-filtered) graph; clearing the selection
    /// while scope is hops falls back to the overview display too.
    #[test]
    fn apply_nav_target_overview_and_anchorless_hops_fall_back() {
        let mut v = view();
        apply_nav_target(&mut v, None, Scope::Overview);
        assert_eq!(v.graph.nodes.len(), 3, "small graph defaults to all kinds visible");
        // Hops with no selection: nothing to anchor on → overview display.
        apply_nav_target(&mut v, None, Scope::Hops(2));
        assert_eq!(v.graph.nodes.len(), 3);
    }

    /// The kind filter auto-populates from the data (sorted, distinct) and hiding a kind
    /// collapses its nodes out of the display.
    #[test]
    fn kind_filter_autopopulates_and_hides() {
        let mut v = view();
        let kinds: Vec<&str> = v.kind_filter.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, vec!["code:method", "code:type"], "exactly the kinds in the data");
        assert!(v.kind_filter.iter().all(|(_, on)| *on), "small graph: all visible");

        // Hide methods → C drops out of the overview; the types stay.
        v.kind_filter = vec![("code:method".into(), false), ("code:type".into(), true)];
        rebuild_display(&mut v);
        assert!(v.graph.nodes.iter().all(|n| n.kind == "code:type"));
        assert_eq!(v.graph.nodes.len(), 2);
    }

    /// The hops anchor always shows, even when its own kind is filtered out.
    #[test]
    fn hops_anchor_survives_its_kind_filter() {
        let mut v = view();
        v.kind_filter = vec![("code:method".into(), false), ("code:type".into(), true)];
        apply_nav_target(&mut v, Some("C".to_string()), Scope::Hops(1));
        let names: Vec<&str> = v.graph.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"C"), "anchor visible despite hidden kind");
        assert!(names.contains(&"B"), "1-hop neighbour visible");
        assert!(!names.contains(&"A"), "outside the 1-hop mask");
    }

}
