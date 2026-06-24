//! Code-graph view panel (`code-graph-view-source`). Renders a project note's **repo source** as
//! the **unified entity graph** (`entity_graph`) — every code symbol AND every spec, with all their
//! edges — through the shared `hiker_graph_view` engine.
//!
//! Phase B split (status: container-tab): the old monolithic `View` is gone. State now lives in two
//! source-/slot-keyed maps on `AppState::panels`:
//! - [`doc::CodeGraphDoc`] in `code_graph_docs` (one per [`CodeSource`], keyed by [`CodeSource::key`]) —
//!   the full universe + governance + changes + palette + SHARED selection/hover.
//! - [`lens::LensView`] in `code_graph_lenses` (one per visible lens, keyed by [`child_state_key`]) —
//!   a single engine + its filtered display + its independent drill scope.
//!
//! A spec is a real node — select it to highlight its edges. Governance drift is a direct
//! `Governs`-edge color and "changed vs HEAD" a direct node ring.
//!
//! The note (`hiker.kind: project`) is parsed by `hiker-projects`, whose repo source binds the SCIP
//! adapter (`hiker-code`); the adapter's `code_graph()` is merged with the spec layer into the one
//! [`EntityGraph`].

mod doc;
mod lens;

pub use doc::CodeGraphDoc;
pub use lens::LensView;

use eframe::egui;

use std::path::{Component, Path, PathBuf};

use crate::panels::code_governance::{self, Changes, DiffVerb, NodeAction};
use crate::panels::entity_graph::{self, Lens, SPEC_KIND};
use crate::state::{AppState, NavTarget};
use crate::tab::{child_state_key, ChildSlot, CodeSource, Scope, Tab, TabId, TabKind};
use hiker_code::ScipAdapter;
use hiker_graph_view::graph_view::source::LayoutConfig;
use hiker_projects::{repo::Backend, Project};
use hiker_theme as theme;
use spec_engine::{DerivedNodeSource, NodeHandle, SourceId};

const FR_BOX: f32 = 1200.0;
pub(crate) const CODE_CFG: LayoutConfig = LayoutConfig { area: FR_BOX * FR_BOX, seed_box: 80.0 };

/// Restore a drill location (`selected`, `scope`) — the apply side of a global Back/Forward
/// (`NavTarget::CodeGraphNode`). Rebuilds the doc's lens displays (a scope change re-scopes).
/// `source` keys the doc; the drill lands on the PRIMARY lens-view. status: code-graph-view-source
pub(crate) fn apply_nav_target(
    app: &mut AppState,
    source: &CodeSource,
    selected: Option<String>,
    scope: Scope,
) {
    let dkey = source.key();
    if let Some(doc) = app.panels.code_graph_docs.get_mut(&dkey) {
        doc.selected = selected;
    }
    let lkey = primary_lens_key(source);
    let Some(doc) = app.panels.code_graph_docs.get(&dkey) else { return };
    if let Some(lensview) = app.panels.code_graph_lenses.get_mut(&lkey) {
        lensview.scope = scope;
        // A nav restore can land directly in a Hops scope — re-derive the stored anchor from the
        // restored selection before the rebuild reads it. status: code-graph-scope-hops
        lensview.hops_anchor = None;
        lens::sync_hops_anchor(lensview, doc);
        lens::rebuild_display(lensview, doc);
    }
}

/// Select `spec`'s node — the landing half of the vault graph's spec → code-graph jump
/// (`vault-graph-spec-drift-badge`). A not-yet-built doc takes the spec through a pending slot
/// consumed once by `show`.
pub(crate) fn select_spec(app: &mut AppState, key: &str, spec: &str) {
    let Some(doc) = app.panels.code_graph_docs.get_mut(key) else {
        app.panels.code_graph_pending_select = Some((key.to_string(), spec.to_string()));
        return;
    };
    // Just select it — the focus spotlight shows the spec's footprint on the full graph, so there's
    // no need to drill the scope to a 1-hop subgraph. status: code-graph-spec-lighting
    doc.selected = Some(spec.to_string());
}

/// Snapshot a loaded view's persisted state into the session map under `key` (the source key). The
/// engine half + per-lens config come from the PRIMARY lens-view; the secondary lens config from
/// the secondary lens-view (when present). status: graph-view-state-persist
pub(crate) fn capture_code_graph_view(app: &mut AppState, key: &str) {
    let Some(source) = CodeSource::from_key(key) else { return };
    let Some(doc) = app.panels.code_graph_docs.get(key) else { return };
    let pkey = primary_lens_key(&source);
    let skey = secondary_lens_key(&source);
    let Some(primary) = app.panels.code_graph_lenses.get(&pkey) else { return };
    let secondary_lens = app
        .panels
        .code_graph_lenses
        .get(&skey)
        .map(|l| l.lens.clone())
        .unwrap_or_else(|| Lens::specs_only(&doc.graph));
    let engine = crate::panels::graph::snapshot_to_view_state(&primary.engine.view_snapshot());
    let state = hiker_core::autosave::CodeGraphViewState {
        scope: crate::panels::graph_nav::scope_persist_str(primary.scope),
        selected: doc.selected.clone(),
        primary: lens_to_state(&primary.lens),
        secondary: lens_to_state(&secondary_lens),
        show_changes: doc.show_changes,
        focus_hops: doc.focus_hops,
        minimap_on: app.panels.code_graph_minimap_on.get(key).copied().unwrap_or(false),
        palette: doc.palette.iter().map(|(k, c)| (k.clone(), [c.r(), c.g(), c.b()])).collect(),
        engine,
    };
    app.session.code_graph_views.insert(key.to_string(), state);
}

/// Apply persisted SHARED state (palette / changes / selection) to a freshly-built doc, once.
/// status: graph-view-state-persist
fn apply_persisted_doc(app: &mut AppState, key: &str) {
    let saved = app.session.code_graph_views.get(key).cloned();
    let Some(doc) = app.panels.code_graph_docs.get_mut(key) else { return };
    if doc.view_restored {
        return;
    }
    doc.view_restored = true;
    let Some(saved) = saved else { return };
    if doc.error.is_some() {
        return;
    }
    doc.selected = saved.selected.clone();
    doc.show_changes = saved.show_changes;
    doc.last_show_changes = saved.show_changes;
    // A `0` here is an older record with no hop radius — `clamp(1, 3)` floors it to the default `1`.
    doc.focus_hops = saved.focus_hops.clamp(1, 3);
    doc.palette = saved
        .palette
        .iter()
        .map(|(k, [r, g, b])| (k.clone(), egui::Color32::from_rgb(*r, *g, *b)))
        .collect();
}

/// Apply persisted per-lens state to a lens-view, once. `slot` picks which saved lens config to
/// apply (primary vs secondary). status: graph-view-state-persist
fn apply_persisted_lens(app: &mut AppState, key: &str, lkey: &str, slot: ChildSlot) {
    let saved = app.session.code_graph_views.get(key).cloned();
    let Some(doc) = app.panels.code_graph_docs.get(key) else { return };
    if doc.error.is_some() {
        if let Some(lensview) = app.panels.code_graph_lenses.get_mut(lkey) {
            lensview.view_restored = true;
        }
        return;
    }
    // The lens-view borrows the doc to rebuild; resolve to an owned copy of what we need first to
    // avoid an overlapping &/&mut on app.panels.
    let saved = saved.clone();
    let Some(lensview) = app.panels.code_graph_lenses.get_mut(lkey) else { return };
    if lensview.view_restored {
        return;
    }
    lensview.view_restored = true;
    let Some(saved) = saved else {
        // No persisted state — still relayout against the (possibly restored) doc selection.
        let doc = app.panels.code_graph_docs.get(key).unwrap();
        let lensview = app.panels.code_graph_lenses.get_mut(lkey).unwrap();
        lens::rebuild_display(lensview, doc);
        return;
    };
    let lens_state = match slot {
        ChildSlot::Primary => &saved.primary,
        ChildSlot::Secondary => &saved.secondary,
    };
    apply_lens_state(&mut lensview.lens, lens_state);
    lensview.scope = crate::panels::graph_nav::scope_from_persist_str(&saved.scope);
    if slot == ChildSlot::Primary {
        let snap = crate::panels::graph::view_state_to_snapshot(&saved.engine);
        lensview.engine.restore_view(&snap);
    }
    let doc = app.panels.code_graph_docs.get(key).unwrap();
    let lensview = app.panels.code_graph_lenses.get_mut(lkey).unwrap();
    // A restored scope can be Hops — derive the stored anchor from the restored selection before the
    // rebuild reads it. status: code-graph-scope-hops
    lens::sync_hops_anchor(lensview, doc);
    lens::rebuild_display(lensview, doc);
    if slot == ChildSlot::Primary {
        lensview.engine.needs_fit = false;
    }
}

/// Project a [`Lens`] into its persisted [`LensState`] (HIDDEN kinds, so a new kind defaults
/// visible). status: graph-view-state-persist
fn lens_to_state(lens: &Lens) -> hiker_core::autosave::LensState {
    hiker_core::autosave::LensState {
        hidden_kinds: lens.kinds.iter().filter(|(_, on)| !on).map(|(k, _)| k.clone()).collect(),
        show_calls: lens.show_calls,
        show_impls: lens.show_impls,
        show_governs: lens.show_governs,
        show_refs: lens.show_refs,
        size_by_loc: lens.size_by_loc,
        changed_only: lens.changed_only,
        hide_orphans: lens.hide_orphans,
        bundling: lens.bundling,
    }
}

/// Apply a persisted [`LensState`] onto an auto-populated lens. status: graph-view-state-persist
fn apply_lens_state(lens: &mut Lens, saved: &hiker_core::autosave::LensState) {
    for (kind, on) in &mut lens.kinds {
        *on = !saved.hidden_kinds.contains(kind);
    }
    lens.show_calls = saved.show_calls;
    lens.show_impls = saved.show_impls;
    lens.show_governs = saved.show_governs;
    lens.show_refs = saved.show_refs;
    lens.size_by_loc = saved.size_by_loc;
    lens.changed_only = saved.changed_only;
    lens.hide_orphans = saved.hide_orphans;
    lens.bundling = saved.bundling;
}

/// The state-map key for a source's PRIMARY lens-view (slot-keyed via [`child_state_key`], but
/// CodeGraph is source-keyed so the tab id is irrelevant — pass a sentinel).
fn primary_lens_key(source: &CodeSource) -> String {
    format!("{}|primary", child_state_key(TabId(0), ChildSlot::Primary, &cg_kind(source)))
}

/// The state-map key for a source's SECONDARY (corner-minimap) lens-view.
fn secondary_lens_key(source: &CodeSource) -> String {
    format!("{}|secondary", child_state_key(TabId(0), ChildSlot::Secondary, &cg_kind(source)))
}

/// The `CodeGraphLens` kind for `source`, used only to derive its source-keyed `child_state_key`.
fn cg_kind(source: &CodeSource) -> TabKind {
    TabKind::CodeGraphLens { source: source.clone() }
}

/// Find-or-focus a code-graph tab for `source`, opening one if none exists. The open shape is a
/// two-lens [`Container`](TabKind::Container): a primary `CodeGraphLens` (the main pane) + a
/// `Peer` secondary `CodeGraphLens` over the SAME source (the corner minimap), both reading one
/// shared [`doc::CodeGraphDoc`]. Dedup is by the container's primary child source.
/// status: container-tab, code-graph-view-source
pub fn open(app: &mut AppState, source: CodeSource) -> TabId {
    if let Some(existing) = app.session.tabs.iter().find(|t| is_code_container_for(&t.kind, &source))
    {
        let id = existing.id;
        app.session.active_tab = Some(id);
        return id;
    }
    // Build the shared doc once + seed both lens-views (primary-default + specs-only) so the
    // container renders immediately. status: container-tab
    let dkey = ensure_doc(app, &source);
    ensure_lensview(app, &dkey, &primary_lens_key(&source), ChildSlot::Primary);
    ensure_lensview(app, &dkey, &secondary_lens_key(&source), ChildSlot::Secondary);

    let id = app.next_tab_id();
    app.session.tabs.push(Tab::new(id, code_container(&source), true));
    app.session.active_tab = Some(id);
    id
}

/// The two-lens container kind for `source` (primary lens + a same-source peer-lens corner).
/// status: container-tab
pub(crate) fn code_container(source: &CodeSource) -> TabKind {
    TabKind::Container {
        primary: Box::new(TabKind::CodeGraphLens { source: source.clone() }),
        secondary: crate::tab::ContainerSecondary::Peer(Box::new(TabKind::CodeGraphLens {
            source: source.clone(),
        })),
        swapped: false,
    }
}

/// True if `kind` is the code-graph container whose PRIMARY lens is over `source` (dedup key).
fn is_code_container_for(kind: &TabKind, source: &CodeSource) -> bool {
    matches!(
        kind,
        TabKind::Container { primary, .. }
            if matches!(primary.as_ref(), TabKind::CodeGraphLens { source: s } if s == source)
    )
}

/// True if the `.md` at `rel` is a project note (`hiker.kind: project`).
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

/// Ensure the doc for `source` exists (building it once), recording a nav anchor on first build.
/// Returns the source key. status: code-graph-view-source
fn ensure_doc(app: &mut AppState, source: &CodeSource) -> String {
    let key = source.key();
    if !app.panels.code_graph_docs.contains_key(&key) {
        let doc = doc::build_doc(app, source);
        if !app.session.nav.locked {
            let selected = doc.selected.clone();
            app.session.nav.push(NavTarget::CodeGraphNode {
                source: source.clone(),
                selected,
                scope: Scope::Overview,
            });
        }
        app.panels.code_graph_docs.insert(key.clone(), doc);
    }
    apply_persisted_doc(app, &key);
    key
}

/// Ensure a lens-view exists under `lkey`, seeding it with `seed` (the default lens for this slot)
/// + bumping the doc refcount. status: container-tab
fn ensure_lensview(app: &mut AppState, dkey: &str, lkey: &str, slot: ChildSlot) {
    if app.panels.code_graph_lenses.contains_key(lkey) {
        return;
    }
    let Some(doc) = app.panels.code_graph_docs.get(dkey) else { return };
    let seed = match slot {
        ChildSlot::Primary => Lens::primary_default(&doc.graph),
        ChildSlot::Secondary => Lens::specs_only(&doc.graph),
    };
    let lensview = LensView::new(seed);
    app.panels.code_graph_lenses.insert(lkey.to_string(), lensview);
    if let Some(doc) = app.panels.code_graph_docs.get_mut(dkey) {
        doc.refcount = doc.refcount.saturating_add(1);
    }
    apply_persisted_lens(app, dkey, lkey, slot);
}

/// The single-lens entry point ([`TabKind::CodeGraphLens`]) — renders ONE lens-view over a shared
/// doc as the main interactive pane: the full toolbar (nav / find / scope / filter / changes /
/// minimap / view options), the detail line, the canvas, node menu, find popup, nav recording, and
/// the summary. The `slot` picks which lens-view this is (primary-default vs specs-only) + its
/// state key. The corner minimap (the *other* lens-view) is NOT rendered here — the
/// [`Container`](TabKind::Container) renders the secondary into the corner via [`show_secondary`]
/// (borrowing that lens-view's engine through `Minimap::ui_for`). A standalone lens tab (no
/// container) simply never shows a corner; the Minimap toggle then has no peer to draw.
/// status: container-tab, code-graph-view-source
pub fn show_lens(
    ui: &mut egui::Ui,
    app: &mut AppState,
    _tab_id: TabId,
    slot: ChildSlot,
    source: &CodeSource,
) {
    let dkey = ensure_doc(app, source);
    // `lkey` is the lens-view THIS render drives (the large pane = `slot`'s lens-view): the toolbar's
    // nav/find/scope/filter/view-options edit it. `ckey` is the CORNER lens-view (the opposite slot)
    // the minimap shows: the toolbar's Minimap dropdown edits it. When the container is swapped, the
    // large slot is Secondary, so `lkey`/`ckey` flip with it and the controls stay consistent.
    // status: container-tab
    let (lkey, ckey) = match slot {
        ChildSlot::Primary => (primary_lens_key(source), secondary_lens_key(source)),
        ChildSlot::Secondary => (secondary_lens_key(source), primary_lens_key(source)),
    };
    let corner_slot = match slot {
        ChildSlot::Primary => ChildSlot::Secondary,
        ChildSlot::Secondary => ChildSlot::Primary,
    };
    ensure_lensview(app, &dkey, &lkey, slot);
    // The corner lens-view must exist for the toolbar's Minimap dropdown (the container also seeds
    // it; seed it here too for a robust standalone path).
    ensure_lensview(app, &dkey, &ckey, corner_slot);
    consume_pending(app, &dkey);

    if doc_error(app, &dkey, ui) {
        return;
    }

    let short = source.path().rsplit('/').next().unwrap_or(source.path());
    ui.heading(format!("Code graph · {short}"));

    // Ctrl+F opens this lens-view's find.
    if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
        if let Some(lensview) = app.panels.code_graph_lenses.get_mut(&lkey) {
            lensview.find.open();
        }
    }
    let esc_taken_by_popup = app
        .panels
        .code_graph_lenses
        .get(&lkey)
        .is_some_and(|v| v.find.is_open())
        || app.panels.code_graph_node_menu.get(&dkey).is_some();

    let before = nav_snapshot(app, &dkey, &lkey);

    let toolbar = toolbar(ui, app, source, &dkey, &lkey, &ckey);
    let nav_restoring = toolbar.nav_delta.is_some();
    if let Some(delta) = toolbar.nav_delta {
        crate::editor_pane::nav_go(app, delta);
    }
    if toolbar.load_changes {
        load_changes(app, &dkey);
    }

    detail_line(ui, app, &dkey);

    // Drive this lens (the main pane) — selection lands on the shared doc.
    drive_lens(ui, app, source, &dkey, &lkey, false);
    node_menu_ui(ui, app, &dkey);
    let jumped = find_popup_apply(ui, app, &dkey, &lkey);
    if let Some(id) = jumped {
        if let Some(lensview) = app.panels.code_graph_lenses.get_mut(&lkey) {
            if lensview.scope == Scope::Overview {
                lensview.scope = Scope::Hops(2);
            }
        }
        if let Some(doc) = app.panels.code_graph_docs.get_mut(&dkey) {
            doc.selected = Some(id);
        }
    }
    if let Some(lensview) = app.panels.code_graph_lenses.get_mut(&lkey) {
        if crate::panels::graph_nav::esc_pops_focus(ui, lensview.scope, esc_taken_by_popup) {
            lensview.scope = Scope::Overview;
        }
    }
    relayout_if_dirty(app, &dkey, &lkey);

    if !nav_restoring && !app.session.nav.locked {
        if let (Some(before), Some(after)) = (before, nav_snapshot(app, &dkey, &lkey)) {
            if after != before {
                let (selected, scope) = after;
                app.session
                    .nav
                    .push(NavTarget::CodeGraphNode { source: source.clone(), selected, scope });
            }
        }
    }
    summary(ui, app, &dkey);
}

/// Render the SECONDARY lens-view as the corner minimap over `host_rect` — the corner-render seam
/// that retires the third engine. The secondary lens-view's OWN engine (which already holds a force
/// layout) is borrowed by [`Minimap::ui_for`]; no `Minimap`-owned `State` exists. A node click in
/// the minimap selects it on the shared doc (round-tripping to the primary) and recenters the
/// primary; the minimap chrome reads `code_graph_minimap_on` to enable. Returns whether the minimap
/// requested a swap (a corner click that wasn't a node) so the container can flip `swapped`.
/// status: container-tab, spec-minimap-swap
pub fn show_secondary(
    ui: &mut egui::Ui,
    app: &mut AppState,
    host_rect: egui::Rect,
    source: &CodeSource,
    corner_slot: ChildSlot,
) -> bool {
    let dkey = source.key();
    // The corner renders the `corner_slot` lens-view (`skey` below); the OTHER lens-view (`pkey`) is
    // the large pane the selection recenters. (When the container is swapped, the corner is the
    // PRIMARY lens-view.) status: container-tab
    let (skey, pkey) = match corner_slot {
        ChildSlot::Secondary => (secondary_lens_key(source), primary_lens_key(source)),
        ChildSlot::Primary => (primary_lens_key(source), secondary_lens_key(source)),
    };
    ensure_lensview(app, &dkey, &skey, corner_slot);

    let minimap_on =
        app.panels.code_graph_minimap_on.get(&source.key()).copied().unwrap_or(false);
    // Ensure a per-doc Minimap chrome exists (chrome only — projection / corner / swap-anim / nav;
    // it borrows the secondary engine for positions, owning no `State`). status: container-tab
    if !app.panels.code_graph_minimaps.contains_key(&dkey) {
        let mut minimap = hiker_graph_view::graph_view::minimap::Minimap::new();
        minimap.set_labels(true); // the spec minimap shows labels
        app.panels.code_graph_minimaps.insert(dkey.clone(), minimap);
    }
    if let Some(m) = app.panels.code_graph_minimaps.get_mut(&dkey) {
        m.enabled = minimap_on;
    }
    if !minimap_on {
        return false;
    }

    // Rebuild the secondary display + relayout when its lens/changes changed.
    {
        let Some(doc) = app.panels.code_graph_docs.get(&dkey) else { return false };
        // Keep the corner lens's stored Hops anchor synced to its scope before its dirty-check reads
        // it via `display_sig`. status: code-graph-scope-hops
        if let Some(sec) = app.panels.code_graph_lenses.get_mut(&skey) {
            lens::sync_hops_anchor(sec, doc);
        }
        let resig = app
            .panels
            .code_graph_lenses
            .get(&skey)
            .map(|s| s.applied.as_ref() != Some(&s.display_sig(doc)))
            .unwrap_or(false);
        if resig {
            let panels = &mut app.panels;
            if let (Some(doc), Some(sec)) = split_borrow(panels, &dkey, &skey) {
                // `rebuild_display` invalidates the secondary engine's own paint
                // cache — and that BORROWED engine is exactly what the minimap
                // renders in the corner (`ui_for`), so there's no separate
                // minimap-owned cache to invalidate. status: container-tab
                lens::rebuild_display(sec, doc);
            }
        }
    }

    // Drive the minimap through the BORROWED secondary engine — `Minimap::ui_for` projects the
    // engine's own positions into the corner disk; no third engine. status: container-tab
    let clicked = {
        let panels = &mut app.panels;
        // The minimap chrome + the borrowed engine live in DIFFERENT maps, and the doc in a third —
        // take the three disjoint borrows together.
        let doc = panels.code_graph_docs.get(&dkey);
        let (Some(doc), Some(sec), Some(minimap)) = (
            doc,
            panels.code_graph_lenses.get_mut(&skey),
            panels.code_graph_minimaps.get_mut(&dkey),
        ) else {
            return false;
        };
        let ring = doc.ring();
        let src = entity_graph::EntityGraphSource::new(
            &sec.display,
            sec.lens.size_by_loc,
            ring,
            doc.gov.governance(),
        )
        .with_dot_radius(3.0)
        .with_palette(&doc.palette)
        .with_importance(&doc.label_importance);
        let out = minimap.ui_for(ui, host_rect, &mut sec.engine, &src, None);
        out.clicked.or(out.focused_on_collapse)
    };

    // A clicked dot selects it on the SHARED doc (so the primary reflects it; a spec lights its
    // governed footprint) and recenters the primary on the node (or its footprint centroid).
    // status: spec-minimap-swap
    if let Some(id) = clicked {
        if let Some(doc) = app.panels.code_graph_docs.get_mut(&dkey) {
            doc.selected = Some(id.clone());
        }
        let target = {
            let panels = &mut app.panels;
            let doc = panels.code_graph_docs.get(&dkey);
            let prim = panels.code_graph_lenses.get(&pkey);
            match (doc, prim) {
                (Some(doc), Some(prim)) => {
                    let on_node = prim
                        .display
                        .nodes
                        .iter()
                        .position(|n| n.id == id)
                        .and_then(|i| prim.engine.positions.get(i).copied());
                    on_node.or_else(|| footprint_centroid(doc, prim, &id))
                }
                _ => None,
            }
        };
        if let (Some(pos), Some(prim)) = (target, app.panels.code_graph_lenses.get_mut(&pkey)) {
            prim.engine.center_on(pos);
        }
    }
    // The container owns the primary↔secondary swap; the toolbar's "Swap" button sets the per-source
    // request which the container consumes. status: container-tab
    app.panels.code_graph_swap_request.remove(&source.key())
}

/// A drill snapshot `(doc.selected, primary.scope)` — the fields a drill changes.
fn nav_snapshot(app: &AppState, dkey: &str, pkey: &str) -> Option<(Option<String>, Scope)> {
    let doc = app.panels.code_graph_docs.get(dkey)?;
    let lensview = app.panels.code_graph_lenses.get(pkey)?;
    Some((doc.selected.clone(), lensview.scope))
}

/// Consume the pending spec-select + the transient hover for a doc (set by adjacent panels).
fn consume_pending(app: &mut AppState, dkey: &str) {
    if let Some((_, spec)) = app.panels.code_graph_pending_select.take_if(|(k, _)| k == dkey) {
        select_spec(app, dkey, &spec);
    }
    // Transient hover from the Specs side panel: take it and mirror onto the doc so the focus
    // spotlight follows the hover without changing the selection. status: code-graph-spec-lighting
    let hover =
        app.panels.code_graph_hover_spec.take().filter(|(k, _)| k == dkey).map(|(_, s)| s);
    if let Some(doc) = app.panels.code_graph_docs.get_mut(dkey) {
        doc.hover_specs = hover.unwrap_or_default();
    }
}

/// Render the doc error (if any) and report whether the caller should bail.
fn doc_error(app: &AppState, dkey: &str, ui: &mut egui::Ui) -> bool {
    if let Some(doc) = app.panels.code_graph_docs.get(dkey) {
        if let Some(err) = &doc.error {
            ui.colored_label(egui::Color32::RED, err);
            return true;
        }
    }
    false
}

/// Drive one lens-view's canvas for a frame: render, then apply any click/right-click/background
/// onto the shared doc (so the selection reflects in every lens). `standalone` skips the per-frame
/// relayout-if-dirty + node-menu/find that the multi-lens `show` does itself (the standalone tab
/// runs the full cycle here). The borrow is split: the doc and lens live in DIFFERENT maps on
/// `app.panels`, so we take an immutable doc ref + a mutable lens ref simultaneously via a
/// split-borrow helper. status: container-tab
fn drive_lens(
    ui: &mut egui::Ui,
    app: &mut AppState,
    source: &CodeSource,
    dkey: &str,
    lkey: &str,
    standalone: bool,
) {
    // Ctrl+F (standalone only — the multi-lens `show` handles it itself).
    if standalone && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
        if let Some(lensview) = app.panels.code_graph_lenses.get_mut(lkey) {
            lensview.find.open();
        }
    }
    let esc_taken = standalone
        && (app.panels.code_graph_lenses.get(lkey).is_some_and(|v| v.find.is_open())
            || app.panels.code_graph_node_menu.get(dkey).is_some());
    let before = standalone.then(|| nav_snapshot(app, dkey, lkey)).flatten();

    let out = {
        let panels = &mut app.panels;
        let (Some(doc), Some(lensview)) =
            split_borrow(panels, dkey, lkey)
        else {
            return;
        };
        lens::render_canvas(ui, lensview, doc)
    };

    if let Some((id, pos)) = out.secondary {
        app.panels.code_graph_node_menu.insert(dkey.to_string(), (id, pos));
    }
    if let Some(doc) = app.panels.code_graph_docs.get_mut(dkey) {
        if let Some(id) = out.clicked {
            doc.selected = Some(id);
        } else if out.background {
            doc.selected = None;
        }
    }

    if standalone {
        node_menu_ui(ui, app, dkey);
        let jumped = find_popup_apply(ui, app, dkey, lkey);
        if let Some(id) = jumped {
            if let Some(lensview) = app.panels.code_graph_lenses.get_mut(lkey) {
                if lensview.scope == Scope::Overview {
                    lensview.scope = Scope::Hops(2);
                }
            }
            if let Some(doc) = app.panels.code_graph_docs.get_mut(dkey) {
                doc.selected = Some(id);
            }
        }
        if let Some(lensview) = app.panels.code_graph_lenses.get_mut(lkey) {
            if crate::panels::graph_nav::esc_pops_focus(ui, lensview.scope, esc_taken) {
                lensview.scope = Scope::Overview;
            }
        }
        relayout_if_dirty(app, dkey, lkey);
        if !app.session.nav.locked {
            if let (Some(before), Some(after)) = (before, nav_snapshot(app, dkey, lkey)) {
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
    }
}

/// Borrow a doc (`&`) and a lens-view (`&mut`) simultaneously — they live in DIFFERENT maps on
/// `Panels`, so the borrow checker permits the disjoint pair. status: container-tab
fn split_borrow<'a>(
    panels: &'a mut crate::state::PanelStates,
    dkey: &str,
    lkey: &str,
) -> (Option<&'a CodeGraphDoc>, Option<&'a mut LensView>) {
    let doc = panels.code_graph_docs.get(dkey);
    let lensview = panels.code_graph_lenses.get_mut(lkey);
    (doc, lensview)
}

/// Rebuild + relayout a lens display when its inputs changed (filter / scope / hops anchor / change
/// load), and refresh the change rings on a "Changes" toggle. status: spec-graph-lens
fn relayout_if_dirty(app: &mut AppState, dkey: &str, lkey: &str) {
    let Some(doc) = app.panels.code_graph_docs.get(dkey) else { return };
    let Some(lensview) = app.panels.code_graph_lenses.get_mut(lkey) else { return };
    // Latch / clear the stored Hops anchor for this frame's (possibly just-changed) scope BEFORE the
    // dirty-check reads it via `display_sig`. The one place every scope-change route funnels through.
    // status: code-graph-scope-hops
    lens::sync_hops_anchor(lensview, doc);
    if lensview.applied.as_ref() != Some(&lensview.display_sig(doc)) {
        lens::rebuild_display(lensview, doc);
    }
    // A "Changes" toggle flips the rings without changing the node set (when not changed-only):
    // refresh the baked paint without a relayout.
    if let Some(doc) = app.panels.code_graph_docs.get_mut(dkey) {
        if doc.last_show_changes != doc.show_changes {
            doc.last_show_changes = doc.show_changes;
            if let Some(lensview) = app.panels.code_graph_lenses.get_mut(lkey) {
                lensview.engine.invalidate_paint_cache();
            }
        }
    }
}

/// Apply the find popup's pick (if any) — the standalone selection lands on the doc by the caller.
fn find_popup_apply(
    ui: &mut egui::Ui,
    app: &mut AppState,
    dkey: &str,
    lkey: &str,
) -> Option<String> {
    let panels = &mut app.panels;
    let (Some(doc), Some(lensview)) = split_borrow(panels, dkey, lkey) else { return None };
    lens::find_popup(ui, lensview, doc)
}

/// Build a SCIP adapter for a project note → its first `repo` source descriptor.
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

/// Bind a `.scip` opened directly from the file tree (no project note).
fn bind_index(app: &AppState, scip_rel: &str) -> Result<(ScipAdapter, SourceId), String> {
    let vault_root = app.vault_session.vault.root();
    let abs = vault_root.join(scip_rel);
    let root = abs.parent().unwrap_or(std::path::Path::new(".")).to_path_buf();
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

/// Resolve a configured path against the vault root.
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

/// Whether `p` resolves inside `vault_root` (canonicalize, with a lexical fallback).
fn within_vault(vault_root: &Path, p: &Path) -> bool {
    match (vault_root.canonicalize(), p.canonicalize()) {
        (Ok(root), Ok(path)) => path.starts_with(root),
        _ => {
            let root = lexical_normalize(vault_root);
            if p.components().any(|c| matches!(c, Component::ParentDir)) {
                return false;
            }
            lexical_normalize(p).starts_with(&root)
        }
    }
}

/// Lexically normalize a path: drop `.` and collapse `..`, without touching the filesystem.
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

/// Load the git change set behind the change ring (once per "Changes" enable). status: code-graph-open-diff-from-node
fn load_changes(app: &mut AppState, dkey: &str) {
    let git = app.vault_session.services.git_sync.clone();
    let vault_root = app.vault_session.vault.root().to_path_buf();
    let Some(doc) = app.panels.code_graph_docs.get_mut(dkey) else { return };
    let Some(adapter) = &doc.adapter else { return };
    doc.changes = match git {
        Some(git) => Some(Changes::load(&git, adapter, &vault_root)),
        None => Some(Err("git isn't enabled for this vault".to_string())),
    };
}

/// Outcome of a toolbar pass.
#[derive(Default)]
struct ToolbarResult {
    nav_delta: Option<i32>,
    /// "Changes" turned on and the change set isn't loaded yet.
    load_changes: bool,
}

/// The (decluttered) toolbar: Back/Forward, Find, the scope dial, a **Filter** dropdown (entity +
/// edge kinds + sizing + only-changed), the **Changes** toggle, a **Minimap** dropdown (show + its
/// filter + swap), the eye view-options menu, and Reset view. Shared toggles (changes, palette) live
/// on the doc; the primary lens config + scope on the primary lens-view; the minimap lens config on
/// the secondary lens-view. status: spec-graph-lens
fn toolbar(
    ui: &mut egui::Ui,
    app: &mut AppState,
    source: &CodeSource,
    dkey: &str,
    pkey: &str,
    skey: &str,
) -> ToolbarResult {
    let can_back = app.session.nav.can_back();
    let can_fwd = app.session.nav.can_forward();
    let has_git = app.vault_session.services.git_sync.is_some();
    let mut out = ToolbarResult::default();
    let mut palette_changed = false;
    let mut do_swap = false;

    let key = source.key();
    let minimap_on = app.panels.code_graph_minimap_on.entry(key.clone()).or_insert(false);
    let mut minimap_on = *minimap_on;

    // Snapshot the shared show_changes + selection-name (read-only for the toolbar) up front.
    let (show_changes, changes_loaded, anchor_label) = {
        let Some(doc) = app.panels.code_graph_docs.get(dkey) else { return out };
        let label = doc.selected.as_ref().map(|id| {
            doc.graph
                .nodes
                .iter()
                .find(|n| &n.id == id)
                .map_or("?", |n| n.name.as_str())
                .to_string()
        });
        (doc.show_changes, doc.changes_loaded(), label)
    };
    let mut new_show_changes = show_changes;

    ui.horizontal_wrapped(|ui| {
        out.nav_delta = crate::panels::graph_nav::nav_controls(ui, can_back, can_fwd);
        ui.separator();
        if ui.small_button("Find").on_hover_text("Find / jump to node (Ctrl+F)").clicked() {
            if let Some(lensview) = app.panels.code_graph_lenses.get_mut(pkey) {
                lensview.find.open();
            }
        }
        ui.separator();
        if let Some(lensview) = app.panels.code_graph_lenses.get_mut(pkey) {
            crate::panels::graph_nav::scope_dial(ui, &mut lensview.scope, anchor_label.as_deref());
        }
        ui.separator();
        // The entity/edge filter, folded into one dropdown — edits the primary lens + the shared
        // palette (on the doc).
        ui.menu_button("Filter", |ui| {
            if let Some(doc) = app.panels.code_graph_docs.get_mut(dkey) {
                if let Some(lensview) = app.panels.code_graph_lenses.get_mut(pkey) {
                    palette_changed |=
                        lens_menu(ui, &mut lensview.lens, show_changes, &mut doc.palette);
                }
            }
        })
        .response
        .on_hover_text("Which entities + edges to show");
        // Direct git-change ring (shared toggle on the doc).
        let changes_resp = ui
            .add_enabled(has_git, egui::Button::selectable(show_changes, "Changes"))
            .on_hover_text("Ring changed entities vs HEAD");
        if !has_git {
            changes_resp.clone().on_hover_text("Git isn't enabled for this vault");
        }
        if changes_resp.clicked() {
            new_show_changes = !show_changes;
            if new_show_changes && !changes_loaded {
                out.load_changes = true;
            }
        }
        ui.separator();
        // The corner minimap, its lens, and the swap — grouped into one dropdown.
        ui.menu_button("Minimap", |ui| {
            ui.checkbox(&mut minimap_on, "Show minimap");
            if minimap_on {
                ui.separator();
                ui.label(egui::RichText::new("Minimap shows").small().color(theme::muted()));
                if let Some(doc) = app.panels.code_graph_docs.get_mut(dkey) {
                    if let Some(sec) = app.panels.code_graph_lenses.get_mut(skey) {
                        palette_changed |=
                            lens_menu(ui, &mut sec.lens, show_changes, &mut doc.palette);
                    }
                }
                ui.separator();
                if ui.button("Swap with main view").clicked() {
                    do_swap = true;
                    ui.close();
                }
            }
        });
        ui.separator();
        let mut no_extra: Vec<(&str, &mut bool)> = Vec::new();
        if let Some(lensview) = app.panels.code_graph_lenses.get_mut(pkey) {
            lensview.engine.view_options_menu(
                ui,
                crate::icons::ICONS.image(crate::icons::Icon::Eye),
                &mut no_extra,
            );
            if ui.small_button("Reset view").clicked() {
                lensview.engine.needs_fit = true;
            }
        }
    });

    // Apply the shared show_changes flip onto the doc.
    if new_show_changes != show_changes {
        if let Some(doc) = app.panels.code_graph_docs.get_mut(dkey) {
            doc.show_changes = new_show_changes;
        }
    }
    // The primary↔secondary swap now lives on the CONTAINER (it flips `swapped`, exchanging which
    // lens-view fills the pane vs the corner). The toolbar only REQUESTS it; the container consumes
    // the per-source request and flips. status: container-tab
    if do_swap {
        app.panels.code_graph_swap_request.insert(key.clone());
    }
    app.panels.code_graph_minimap_on.insert(key, minimap_on);

    // A recolour rebakes the GPU fills on both lens engines. The minimap renders
    // the secondary engine in the corner (`ui_for`), so invalidating that engine's
    // cache (below) is all the corner needs — no minimap-owned cache.
    // status: container-tab
    if palette_changed {
        if let Some(p) = app.panels.code_graph_lenses.get_mut(pkey) {
            p.engine.invalidate_paint_cache();
        }
        if let Some(s) = app.panels.code_graph_lenses.get_mut(skey) {
            s.engine.invalidate_paint_cache();
        }
    }
    out
}

/// One lens's filter checklist (entity kinds + edge kinds + sizing + only-changed) — the body of
/// the Filter / Minimap dropdowns. Each entity row leads with a clickable color swatch. Returns
/// whether the palette changed. status: spec-graph-lens
fn lens_menu(
    ui: &mut egui::Ui,
    lens: &mut Lens,
    show_changes: bool,
    palette: &mut std::collections::HashMap<String, egui::Color32>,
) -> bool {
    let mut palette_changed = false;
    ui.label(egui::RichText::new("Entities").small().color(theme::muted()));
    for (kind, on) in &mut lens.kinds {
        let default = entity_graph::kind_color(kind);
        palette_changed |= swatch_row(ui, on, kind_label(kind), kind.as_str(), default, palette);
    }
    ui.separator();
    ui.label(egui::RichText::new("Edges").small().color(theme::muted()));
    palette_changed |= swatch_row(ui, &mut lens.show_calls, "Calls", "edge:calls", entity_graph::edge_default_color("edge:calls"), palette);
    palette_changed |= swatch_row(ui, &mut lens.show_impls, "Implements", "edge:implements", entity_graph::edge_default_color("edge:implements"), palette);
    // Governs has no swatch — it's coloured by drift state (ok / drifted / missing), not one colour.
    ui.checkbox(&mut lens.show_governs, "Governs")
        .on_hover_text("Coloured by drift state: ok / drifted / missing");
    palette_changed |= swatch_row(ui, &mut lens.show_refs, "References", "edge:reference", entity_graph::edge_default_color("edge:reference"), palette);
    ui.separator();
    ui.checkbox(&mut lens.bundling, "Bundle")
        .on_hover_text("Collapse leaf symbols into their module bundle at low zoom; auto-expand on zoom-in");
    ui.checkbox(&mut lens.size_by_loc, "Size by LOC");
    // Stored as hide_orphans; the user-facing toggle is "Show disconnected" (the disconnected ring).
    let mut show_orphans = !lens.hide_orphans;
    if ui
        .checkbox(&mut show_orphans, "Show disconnected")
        .on_hover_text("Show degree-0 nodes (the disconnected ring), hidden by default in overview")
        .changed()
    {
        lens.hide_orphans = !show_orphans;
    }
    ui.add_enabled(show_changes, egui::Checkbox::new(&mut lens.changed_only, "Only changed"))
        .on_hover_text("Show only entities changed vs HEAD (needs Changes on)");
    if !show_changes {
        lens.changed_only = false;
    }
    palette_changed
}

/// One filter row: a clickable colour swatch + the visibility checkbox. Left-click the swatch opens
/// a picker (writes `palette[key]`); right-click resets it. Returns whether the palette changed.
/// status: graph-view-state-persist
fn swatch_row(
    ui: &mut egui::Ui,
    on: &mut bool,
    label: &str,
    key: &str,
    default: egui::Color32,
    palette: &mut std::collections::HashMap<String, egui::Color32>,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let mut col = palette.get(key).copied().unwrap_or(default);
        let resp = egui::color_picker::color_edit_button_srgba(ui, &mut col, egui::color_picker::Alpha::Opaque)
            .on_hover_text("Click to recolour; right-click to reset");
        if resp.changed() {
            palette.insert(key.to_string(), col);
            changed = true;
        }
        if resp.secondary_clicked() {
            palette.remove(key);
            changed = true;
        }
        ui.checkbox(on, label);
    });
    changed
}

/// The friendly label for an entity kind in the filter list (`code:type` → "type"; spec stays;
/// `spec:document` → "spec doc").
fn kind_label(kind: &str) -> &str {
    match kind {
        entity_graph::SPECDOC_KIND => "spec doc",
        _ => kind.strip_prefix("code:").unwrap_or(kind),
    }
}

/// Read-only click→detail: the selected node's kind + (code) definition `file:line` or (spec)
/// status + governing summary.
fn detail_line(ui: &mut egui::Ui, app: &AppState, dkey: &str) {
    let Some(doc) = app.panels.code_graph_docs.get(dkey) else { return };
    let Some(id) = &doc.selected else { return };
    let Some(node) = doc.graph.nodes.iter().find(|n| &n.id == id) else { return };
    let text = if node.kind == SPEC_KIND {
        let status = node.status.as_deref().unwrap_or("\u{2014}");
        let governs = doc.gov.governance().map(|g| g.targets_of(id).len()).unwrap_or(0);
        format!("\u{2192} spec {}  \u{b7}  status {status}  \u{b7}  governs {governs} entities", node.name)
    } else {
        let loc = doc
            .adapter
            .as_ref()
            .and_then(|a| a.locate(&NodeHandle { source: doc.src.clone(), id: node.id.clone() }))
            .map(|l| format!("{}:{}", l.file, l.start_line + 1))
            .unwrap_or_else(|| node.file.clone());
        let specs = doc
            .gov
            .governance()
            .map(|g| g.specs_of(&node.id))
            .filter(|s| !s.is_empty())
            .map(|s| format!("  \u{b7}  governed by {}", s.join(", ")))
            .unwrap_or_default();
        format!("\u{2192} {}  \u{b7}  {}  @ {loc}{specs}", node.name, node.kind)
    };
    ui.label(egui::RichText::new(text).color(theme::muted()).small());
}

/// The centroid (in main-display world positions) of the entities the spec `id` governs that are
/// visible in the main view — where to recenter when the clicked spec itself isn't shown there.
/// `None` for a non-spec id or when none of its governed entities are visible. status: spec-minimap-swap
fn footprint_centroid(doc: &CodeGraphDoc, prim: &LensView, id: &str) -> Option<egui::Vec2> {
    let g = doc.gov.governance()?;
    let mut sum = egui::Vec2::ZERO;
    let mut count = 0u32;
    for moniker in g.targets_of(id) {
        if let Some(i) = prim.display.nodes.iter().position(|n| n.id == *moniker) {
            if let Some(&p) = prim.engine.positions.get(i) {
                sum += p;
                count += 1;
            }
        }
    }
    (count > 0).then(|| sum / count as f32)
}

/// Render the latched node context menu and apply the picked verb. status: code-graph-open-diff-from-node
fn node_menu_ui(ui: &mut egui::Ui, app: &mut AppState, dkey: &str) {
    let has_git = app.vault_session.services.git_sync.is_some();
    let picked = {
        let Some(doc) = app.panels.code_graph_docs.get(dkey) else { return };
        let Some((id, _)) = app.panels.code_graph_node_menu.get(dkey).cloned() else { return };
        let node = doc.graph.nodes.iter().find(|n| n.id == id);
        let is_spec = node.is_some_and(|n| n.kind == SPEC_KIND);
        let file = node.map(|n| n.file.clone()).unwrap_or_default();
        let repo_root = doc.adapter.as_ref().map(|a| a.repo_root().to_path_buf());
        let menu = if is_spec {
            code_governance::spec_node_menu(&id)
        } else {
            let diff = if has_git { DiffVerb::Ready } else { DiffVerb::NoGit };
            code_governance::node_menu(&id, diff, &[])
        };
        let mut slot = app.panels.code_graph_node_menu.remove(dkey);
        let action = crate::item_menu::latched_menu_popup(
            ui,
            egui::Id::new("code-graph-node-menu"),
            &mut slot,
            menu,
        );
        // Re-insert the (possibly still-open) menu slot.
        if let Some(s) = slot {
            app.panels.code_graph_node_menu.insert(dkey.to_string(), s);
        }
        action.map(|a| (a, id, is_spec, file, repo_root))
    };
    let Some((action, node_id, is_spec, file, repo_root)) = picked else { return };
    let vault_root = app.vault_session.vault.root().to_path_buf();
    match action {
        NodeAction::OpenSource if is_spec => crate::editor_pane::open_file(app, &file, false),
        NodeAction::OpenSource => {
            let Some(repo_root) = repo_root else { return };
            let abs = repo_root.join(&file);
            match abs.strip_prefix(&vault_root) {
                Ok(rel) => crate::editor_pane::open_code_file(app, &rel.to_string_lossy()),
                Err(_) => app.push_toast(
                    format!("Source file is outside the vault: {}", abs.display()),
                    crate::state::ToastLevel::Error,
                ),
            }
        }
        NodeAction::OpenDiff => {
            let Some(repo_root) = repo_root else { return };
            let abs = repo_root.join(&file);
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
        NodeAction::SelectSpec(spec) => select_spec(app, dkey, &spec),
        NodeAction::FocusHops(n) => {
            // Select the right-clicked CODE node and remember the spotlight hop radius; plain clicks
            // thereafter reuse it. Both lenses read the shared doc. status: code-graph
            if let Some(doc) = app.panels.code_graph_docs.get_mut(dkey) {
                doc.selected = Some(node_id);
                doc.focus_hops = n.clamp(1, 3);
            }
        }
    }
}

fn summary(ui: &mut egui::Ui, app: &AppState, dkey: &str) {
    let Some(doc) = app.panels.code_graph_docs.get(dkey) else { return };
    let Some(adapter) = &doc.adapter else { return };
    let pkey = CodeSource::from_key(dkey).map(|s| primary_lens_key(&s));
    let display = pkey.as_deref().and_then(|k| app.panels.code_graph_lenses.get(k));
    let code_n = doc.graph.nodes.iter().filter(|n| n.kind != SPEC_KIND).count();
    let spec_n = doc.graph.nodes.len() - code_n;
    let gov = doc
        .gov
        .governance()
        .map(|g| {
            let [ok, drifted, missing, ungoverned] = code_governance::gov_counts(
                g,
                doc.graph.nodes.iter().filter(|n| n.kind != SPEC_KIND).map(|n| n.id.as_str()),
            );
            format!(" \u{b7} spec: {ok} ok / {drifted} drifted / {missing} missing / {ungoverned} ungoverned")
        })
        .unwrap_or_default();
    let (shown_n, edge_n) = display.map(|l| (l.display.nodes.len(), l.display.edges.len())).unwrap_or((0, 0));
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(format!(
            "[{}] showing {shown_n} of {code_n} code + {spec_n} spec \u{b7} {edge_n} edges \u{b7} impl-edges: {}{gov}",
            adapter.tool(),
            adapter.impl_source(),
        ))
        .color(theme::muted())
        .small(),
    );
}
