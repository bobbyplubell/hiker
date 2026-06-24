//! The per-lens **view** — one per visible lens (a single graph-view engine + its filtered display
//! subgraph + its independent drill scope + the find picker). Keyed by
//! [`crate::tab::child_state_key`] and stored in `AppState::panels.code_graph_lenses`. A `LensView`
//! borrows its [`CodeGraphDoc`](super::doc::CodeGraphDoc) at render time to build its
//! [`EntityGraphSource`] descriptors, so the shared palette / importance / selection / governance
//! all flow from the doc while each lens keeps its own ONE layout.
//!
//! status: spec-graph-lens, code-graph-scope-hops

use eframe::egui;

use super::doc::CodeGraphDoc;
use super::CODE_CFG;
use crate::panels::entity_graph::{self, EntityGraph, EntityGraphSource, Lens, SPEC_KIND};
use crate::tab::Scope;
use hiker_graph::LayoutKind;
use hiker_graph_view::graph_view;
use hiker_graph_view::graph_view::styling::Style;

/// Per child slot (one per visible lens). Holds exactly one layout — the win over the old
/// three-engine `View`.
pub struct LensView {
    /// This view's lens (which kinds + edges it draws). status: spec-graph-lens
    pub(crate) lens: Lens,
    /// The ONE graph-view engine, laid out over this lens's [`display`](Self::display).
    pub(crate) engine: graph_view::State,
    /// This lens's filtered + reindexed display subgraph — what the engine lays out + renders.
    pub(crate) display: EntityGraph,
    /// The layout inputs the `display` was last built for (relayout when they change).
    pub(crate) applied: Option<DisplaySig>,
    /// Display scope: the whole graph, or 1–3 hops around the selection. Per-lens (independent
    /// drill). status: code-graph-scope-hops
    pub(crate) scope: Scope,
    /// The node a Hops drill is anchored on — DECOUPLED from the live selection. Set once when a
    /// drill begins (the then-selected node), cleared on Overview. While in a Hops view, deselecting
    /// (a background click) clears the selection HIGHLIGHT but leaves this unchanged, so the displayed
    /// subgraph stays put instead of collapsing back to the overview. status: code-graph-scope-hops
    pub(crate) hops_anchor: Option<String>,
    /// "Find / jump to node" popup (Ctrl+F). status: graph-find-popup
    pub(crate) find: crate::widgets::autocomplete_picker::PickerState,
    /// The focus-spotlight set last applied (display indices), to invalidate the paint cache only
    /// when the selection's footprint changes. status: code-graph-spec-lighting
    pub(crate) last_focus: Vec<usize>,
    /// Whether persisted per-lens view state has been applied yet. status: graph-view-state-persist
    pub(crate) view_restored: bool,
}

/// The inputs that determine a lens's `display` subgraph + its layout — when this changes, the
/// display is rebuilt and the force layout re-runs ("redo FA on filter"). status: spec-graph-lens
#[derive(Clone, PartialEq)]
pub(crate) struct DisplaySig {
    lens: Lens,
    scope: Scope,
    /// The hops anchor (the selected node, only while scope is Hops).
    anchor: Option<String>,
    /// The selected SPEC (if a spec is selected) — its governed entities are revealed in the
    /// display, so selecting/deselecting a spec rebuilds. status: code-graph-spec-lighting
    revealed_spec: Option<String>,
    /// Whether the change set is loaded (affects the `changed_only` filter).
    changes_loaded: bool,
}

impl LensView {
    /// A fresh lens-view for `lens`: a single flat-styled force-directed engine with the code-graph
    /// highlight tuning (the focus spotlight is the only selection signal). `minimap_styled` tunes
    /// the engine for the corner-minimap secondary (labels on + a label pill).
    pub(crate) fn new(lens: Lens) -> Self {
        let mut engine = graph_view::State::new(Style::flat(), LayoutKind::ForceDirected);
        engine.palette_editable = false;
        // The focus spotlight (`with_focus`) is the ONLY selection signal — dim everything but the
        // selection's footprint. Turn off the engine's additive selection effects that fought it:
        // the selected-node edge GLOW and the selection label-dimming (which semi-dimmed the 1-hop
        // neighbour labels the spotlight keeps full). Hover glow stays on, toned down.
        // status: code-graph-spec-lighting
        engine.highlight.selected_edges = false;
        engine.highlight.dim_labels = false;
        engine.highlight.width = 1.4;
        engine.highlight.opacity = 0.6;
        engine.highlight.softness = 0.35;
        // A label background pill — the code graph is dense, so labels need to lift off the
        // edges/nodes behind them to stay readable. status: graph-label-dim
        engine.style.label_bg = Some(graph_view::styling::LABEL_PILL);
        Self {
            lens,
            engine,
            display: EntityGraph::default(),
            applied: None,
            scope: Scope::Overview,
            hops_anchor: None,
            find: crate::widgets::autocomplete_picker::PickerState::default(),
            last_focus: Vec::new(),
            view_restored: false,
        }
    }

    /// The hops anchor: the STORED drill anchor (`hops_anchor`), only while scope is Hops — NOT the
    /// live `doc.selected`. This is what keys the displayed subgraph, so deselecting within a Hops
    /// view (a background click that clears `doc.selected`) leaves the subgraph put instead of
    /// collapsing to the overview. [`sync_hops_anchor`] keeps `hops_anchor` populated/cleared as the
    /// scope changes. status: code-graph-scope-hops
    fn anchor(&self, _doc: &CodeGraphDoc) -> Option<String> {
        match self.scope {
            Scope::Hops(_) => self.hops_anchor.clone(),
            Scope::Overview => None,
        }
    }

    /// This display's current layout signature (against the shared `doc`'s selection + changes).
    pub(crate) fn display_sig(&self, doc: &CodeGraphDoc) -> DisplaySig {
        DisplaySig {
            lens: self.lens.clone(),
            scope: self.scope,
            anchor: self.anchor(doc),
            revealed_spec: doc.selected_spec().map(str::to_string),
            changes_loaded: doc.changes_loaded(),
        }
    }
}

/// Keep the lens's stored Hops anchor ([`LensView::hops_anchor`]) in sync with its scope — the ONE
/// place every scope-change route funnels through, called once per frame in the render path so
/// every drill (toolbar dial, right-click FocusHops/SelectSpec, Find-jump, Esc/Overview reset) is
/// covered without sprinkling. While in a Hops view with no anchor yet, latch the then-selected
/// node as the anchor (the node the drill began on); on Overview, clear it. The anchor is what keys
/// the displayed subgraph, so a later background click that clears `doc.selected` no longer
/// collapses the view. status: code-graph-scope-hops
pub(crate) fn sync_hops_anchor(lens: &mut LensView, doc: &CodeGraphDoc) {
    match lens.scope {
        Scope::Hops(_) => {
            if lens.hops_anchor.is_none() {
                lens.hops_anchor = doc.selected.clone();
            }
        }
        Scope::Overview => lens.hops_anchor = None,
    }
}

/// Rebuild this lens's display subgraph from the doc's full graph (filter by kind/edge/scope/
/// changed-only) and re-run the force layout over it. status: spec-graph-lens
pub(crate) fn rebuild_display(lens: &mut LensView, doc: &CodeGraphDoc) {
    let mask = hops_mask(lens, doc);
    let anchor = lens.anchor(doc);
    // Reveal a selected spec + everything it governs, even if their kinds are filtered out, so the
    // footprint is complete. status: code-graph-spec-lighting
    let mut force_show = doc.governed_of_selected();
    if let Some(spec) = doc.selected_spec() {
        force_show.push(spec.to_string());
    }
    let changes = doc.change_set();
    lens.display = entity_graph::filter_for(
        &doc.graph,
        &lens.lens,
        mask.as_deref(),
        changes,
        anchor.as_deref(),
        &force_show,
    );
    let ring = doc.ring();
    let src = EntityGraphSource::new(&lens.display, lens.lens.size_by_loc, ring, doc.gov.governance());
    lens.engine.recompute_layout(&src, CODE_CFG);
    lens.applied = Some(lens.display_sig(doc));
}

/// The focus "spotlight" set for the doc's current selection OR hover (display indices): a SPEC
/// lights itself + the entities it governs that are visible in this display; a CODE node lights
/// itself + its 1-hop neighbours. A hover (from the Specs panel) takes precedence over the
/// click-selection but is transient. Empty when nothing is selected/hovered.
/// status: code-graph-spec-lighting
fn focus_set(lens: &LensView, doc: &CodeGraphDoc) -> Vec<usize> {
    let di = |nid: &str| lens.display.nodes.iter().position(|n| n.id == nid);
    // Hover (from the Specs panel) takes precedence — and may be MANY specs (a section / doc group):
    // the spotlight is the union of each spec + the entities it governs that are in the display.
    if !doc.hover_specs.is_empty() {
        let mut set: Vec<usize> = Vec::new();
        let push = |i: usize, set: &mut Vec<usize>| {
            if !set.contains(&i) {
                set.push(i);
            }
        };
        for spec in &doc.hover_specs {
            if let Some(i) = di(spec) {
                push(i, &mut set);
            }
        }
        if let Some(g) = doc.gov.governance() {
            for id in doc.graph.governed_ids_for(g, &doc.hover_specs) {
                if let Some(i) = di(id) {
                    push(i, &mut set);
                }
            }
        }
        return set;
    }
    let Some(id) = doc.selected.as_deref() else { return Vec::new() };
    let is_spec = doc.graph.nodes.iter().any(|n| n.id == id && n.kind == SPEC_KIND);
    if is_spec {
        // Spec: itself + the entities it governs that are present in the display.
        let mut set: Vec<usize> = di(id).into_iter().collect();
        if let Some(g) = doc.gov.governance() {
            for moniker in doc.graph.governed_ids(g, id) {
                if let Some(i) = di(moniker) {
                    if !set.contains(&i) {
                        set.push(i);
                    }
                }
            }
        }
        set
    } else {
        // Code node: itself + every neighbour within `doc.focus_hops` undirected hops in the
        // display. The BFS over `lens.display.edges` reduces to today's direct-neighbour set when
        // `focus_hops == 1` (regression-safe).
        let Some(center) = di(id) else { return Vec::new() };
        let depth = doc.focus_hops.max(1) as usize;
        let mask = crate::panels::graph_nav::hop_mask(
            lens.display.nodes.len(),
            lens.display.edges.iter().map(|&(a, b, _)| (a, b)),
            center,
            depth,
        );
        mask.iter().enumerate().filter_map(|(i, &on)| on.then_some(i)).collect()
    }
}

/// The hops mask (`Some` only in Hops scope with a placed anchor): a per-node `true`/`false` over
/// the FULL adjacency, so hop distance is structural. status: code-graph-scope-hops
fn hops_mask(lens: &LensView, doc: &CodeGraphDoc) -> Option<Vec<bool>> {
    let Scope::Hops(d) = lens.scope else { return None };
    // Key the structural hop mask off the STORED drill anchor (not the live selection), so the
    // filtered subgraph stays put when the selection clears within the view. status: code-graph-scope-hops
    let anchor = lens.anchor(doc)?;
    let fi = doc.graph.nodes.iter().position(|n| n.id == anchor)?;
    Some(crate::panels::graph_nav::hop_mask(
        doc.graph.nodes.len(),
        doc.graph.edges.iter().map(|&(a, b, _)| (a, b)),
        fi,
        d as usize,
    ))
}

/// Drive this lens's engine for one frame in `rect`; returns the clicked node id, if any. Borrows
/// the shared `doc` (read-only) to build the source descriptor — the selection / palette /
/// importance / governance all flow from it. Reported clicks (node / background / right-click) are
/// returned for the caller to apply onto the doc (so a pick reflects in every lens).
pub(crate) struct CanvasOut {
    pub(crate) clicked: Option<String>,
    /// A right-click target id + the pointer position the menu should open at.
    pub(crate) secondary: Option<(String, egui::Pos2)>,
    /// True when a click hit empty background (the caller clears the doc selection).
    pub(crate) background: bool,
}

/// Render the lens's main interactive canvas into the available area. status: spec-graph-lens
pub(crate) fn render_canvas(
    ui: &mut egui::Ui,
    lens: &mut LensView,
    doc: &CodeGraphDoc,
) -> CanvasOut {
    // Selection → the engine's selected-node edge highlight (mapped onto the DISPLAY indices).
    lens.engine.selected_node =
        doc.selected.as_ref().and_then(|id| lens.display.nodes.iter().position(|n| &n.id == id));
    // The focus spotlight: the selection's footprint stays full, everything else dims. Recompute
    // each frame; invalidate the GPU paint cache only when it actually changes (the dimmed fills are
    // baked into the batch). status: code-graph-spec-lighting
    let focus = focus_set(lens, doc);
    if focus != lens.last_focus {
        lens.engine.invalidate_paint_cache();
        lens.last_focus = focus.clone();
    }
    let ring = doc.ring();
    // Mirror the lens's bundling toggle onto the engine — the engine now owns the SPATIAL clustering
    // (on the FA2 positions); off → the interactive pane passes a 0.0 screen scale (no bundling).
    // status: code-graph-bundling
    lens.engine.bundling = lens.lens.bundling;
    let src = EntityGraphSource::new(&lens.display, lens.lens.size_by_loc, ring, doc.gov.governance())
        .with_focus(&focus)
        .with_palette(&doc.palette)
        .with_importance(&doc.label_importance);
    let size = egui::vec2(ui.available_width(), (ui.available_height() - 24.0).max(50.0));
    let inner = ui.allocate_ui(size, |ui| {
        lens.engine.ui(ui, &src, |p, r, t, b, a| {
            crate::panels::graph::paint_preview_card(p, r, t, b, a);
        })
    });
    let clicked = inner.inner;
    let secondary = lens.engine.take_secondary_click().map(|id| {
        let pos = ui.ctx().pointer_latest_pos().unwrap_or_else(|| ui.min_rect().center());
        (id, pos)
    });
    let background = clicked.is_none() && lens.engine.take_background_click();
    CanvasOut { clicked, secondary, background }
}

/// Drive one frame of the "Find / jump to node" popup over the unified node list. status: graph-find-popup
pub(crate) fn find_popup(
    ui: &mut egui::Ui,
    lens: &mut LensView,
    doc: &CodeGraphDoc,
) -> Option<String> {
    use crate::widgets::autocomplete_picker::{self, PickerOutcome};
    if !lens.find.is_open() {
        return None;
    }
    let source = crate::panels::graph_find::EntityNodeFindSource::new(&doc.graph.nodes);
    match autocomplete_picker::show(ui, &mut lens.find, &source, "Find node") {
        PickerOutcome::Selected(item) => Some(item.insert.to_string()),
        PickerOutcome::Cancelled | PickerOutcome::Open => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scope drill on the lens-view reads the doc's shared selection for its anchor + rebuilds the
    /// display (relayout), recording the new signature. The doc and lens are SEPARATE values (the
    /// Phase B split) — the lens borrows the doc to rebuild. status: container-tab
    #[test]
    fn scope_drill_reads_doc_selection_and_rebuilds() {
        let mut doc = CodeGraphDoc::empty();
        doc.selected = Some("sym/b".to_string());
        let mut lensview = LensView::new(Lens::all(&EntityGraph::default()));
        lensview.scope = Scope::Hops(2);
        rebuild_display(&mut lensview, &doc);
        // The display was (re)built and its signature captured the drill location.
        let sig = lensview.applied.clone().expect("display was rebuilt");
        assert!(sig == lensview.display_sig(&doc), "applied signature matches current");
        assert_eq!(lensview.scope, Scope::Hops(2));
    }

    /// Bug 2 (code-graph-scope-hops): in a Hops view the stored `hops_anchor` — NOT the live
    /// `doc.selected` — keys the displayed subgraph. So `sync_hops_anchor` latches the anchor from the
    /// selection at drill time, then clearing the selection (a background deselect) leaves
    /// `anchor(doc)` returning the SAME stored anchor (the subgraph persists, no collapse). Switching
    /// to Overview clears the stored anchor.
    #[test]
    fn hops_anchor_persists_through_deselect_and_clears_on_overview() {
        let mut doc = CodeGraphDoc::empty();
        doc.selected = Some("sym/anchor".to_string());
        let mut lensview = LensView::new(Lens::all(&EntityGraph::default()));
        lensview.scope = Scope::Hops(2);

        // Drill: sync latches the then-selected node as the stored anchor.
        sync_hops_anchor(&mut lensview, &doc);
        assert_eq!(lensview.hops_anchor.as_deref(), Some("sym/anchor"));
        assert_eq!(lensview.anchor(&doc).as_deref(), Some("sym/anchor"), "anchor keys to the stored node");

        // Deselect within the view (a background click clears the live selection).
        doc.selected = None;
        sync_hops_anchor(&mut lensview, &doc); // a later frame re-runs sync; anchor must NOT change
        assert_eq!(
            lensview.anchor(&doc).as_deref(),
            Some("sym/anchor"),
            "deselect leaves the stored anchor → subgraph persists, no collapse to overview"
        );

        // Selecting a DIFFERENT node moves the highlight without changing the anchor/subgraph.
        doc.selected = Some("sym/other".to_string());
        sync_hops_anchor(&mut lensview, &doc);
        assert_eq!(
            lensview.anchor(&doc).as_deref(),
            Some("sym/anchor"),
            "clicking another node in the view doesn't re-anchor the subgraph"
        );

        // Esc / Overview clears the stored anchor.
        lensview.scope = Scope::Overview;
        sync_hops_anchor(&mut lensview, &doc);
        assert_eq!(lensview.hops_anchor, None, "Overview resets the stored anchor");
        assert_eq!(lensview.anchor(&doc), None);
    }

    use crate::panels::entity_graph::{EntityEdge, EntityNode};

    /// A bare CODE node descriptor (only `id`/`kind` matter to `focus_set`).
    fn code_node(id: &str) -> EntityNode {
        EntityNode {
            id: id.to_string(),
            name: id.to_string(),
            kind: "code:fn".to_string(),
            file: String::new(),
            start_line: 0,
            lines: 0,
            status: None,
            parent: None,
        }
    }

    /// The code-node focus spotlight is a BFS to `doc.focus_hops` undirected hops over the display
    /// edges: `1` hop reproduces the historical direct-neighbour set, and each higher radius pulls in
    /// the next ring (neighbours-of-neighbours …). status: code-graph
    #[test]
    fn focus_set_code_node_bfs_widens_with_hop_radius() {
        // A path display: c -- a -- b -- d (indices 0,1,2,3) plus an isolated node e (index 4) the
        // spotlight must never reach.
        let display = EntityGraph {
            nodes: vec![code_node("c"), code_node("a"), code_node("b"), code_node("d"), code_node("e")],
            edges: vec![
                (0, 1, EntityEdge::Calls),
                (1, 2, EntityEdge::Calls),
                (2, 3, EntityEdge::Calls),
            ],
        };
        // The doc's full graph carries the same nodes so the `is_spec` check resolves "c" to a code
        // node (not a spec).
        let mut doc = CodeGraphDoc::empty();
        doc.graph = display.clone();
        doc.selected = Some("c".to_string());
        let mut lensview = LensView::new(Lens::all(&EntityGraph::default()));
        lensview.display = display;

        let as_set = |mut v: Vec<usize>| {
            v.sort_unstable();
            v
        };

        // 1 hop = center + DIRECT neighbours only (the old behaviour): {c, a}.
        doc.focus_hops = 1;
        assert_eq!(as_set(focus_set(&lensview, &doc)), vec![0, 1], "1 hop = center + direct neighbour");

        // 2 hops pulls in the neighbour-of-neighbour: {c, a, b}.
        doc.focus_hops = 2;
        assert_eq!(as_set(focus_set(&lensview, &doc)), vec![0, 1, 2], "2 hops adds the 2nd ring");

        // 3 hops reaches the far end of the path: {c, a, b, d}; the isolated e (4) is never lit.
        doc.focus_hops = 3;
        assert_eq!(as_set(focus_set(&lensview, &doc)), vec![0, 1, 2, 3], "3 hops adds the 3rd ring, never e");
    }
}
