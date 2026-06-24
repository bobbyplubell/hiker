//! The shared code-graph **document** — one per [`CodeSource`], stored in
//! `AppState::panels.code_graph_docs` keyed by [`CodeSource::key`]. Holds everything both lenses
//! must agree on: the bound SCIP adapter, the full unified [`EntityGraph`] universe, the governance
//! rollup behind the spec layer + drift colors, the git change set behind the change ring, the
//! palette + label-importance caches, and the SHARED interaction state (`selected` + `hover_specs`).
//! A pick in one lens reflects in the other because every lens-view reads the doc.
//!
//! status: code-graph-view-source, spec-graph-lens

use eframe::egui;

use crate::panels::code_governance::{Changes, GovCache};
use crate::panels::entity_graph::{self, EntityGraph, SPEC_KIND};
use hiker_code::ScipAdapter;
use spec_engine::SourceId;

/// Shared, ONE per source. Holds everything both lenses must agree on. The per-lens
/// concerns (engine/layout, display subgraph, scope, find) live on [`super::lens::LensView`].
pub struct CodeGraphDoc {
    /// `None` only when `error` is set.
    pub(crate) adapter: Option<ScipAdapter>,
    pub(crate) src: SourceId,
    /// The full unified universe (code + spec nodes + all edges). `pub(crate)`: the link preview
    /// slices a warm doc's 1-hop neighbourhood from it.
    pub(crate) graph: EntityGraph,
    /// The lazily-loaded governance rollup behind the spec layer + the `Governs` drift colors.
    pub(crate) gov: GovCache,
    /// The git change set behind the change ring; `None` until "Changes" turns on, `Err` = failure.
    pub(crate) changes: Option<Result<Changes, String>>,
    /// Whether the git-change ring is drawn (shared — both lenses ring the same set).
    pub(crate) show_changes: bool,
    /// Last "show changes" value, to refresh node rings (a paint-only change) on toggle.
    pub(crate) last_show_changes: bool,
    /// User color overrides per entity kind (`code:type`, `spec`, …); empty entries fall back to the
    /// built-in palette. Recolors apply to BOTH lenses. status: graph-view-state-persist
    pub(crate) palette: std::collections::HashMap<String, egui::Color32>,
    /// Cached per-node-id containment-subtree weight driving the label LOD; computed once from the
    /// full graph (whose `parent` survives, unlike a display's). status: graph-label-dim
    pub(crate) label_importance: std::collections::HashMap<String, f32>,
    pub(crate) error: Option<String>,
    /// The click-selected node's id (a SCIP moniker or a spec slug), SHARED across lenses so a pick
    /// in one reflects in the other. `pub(crate)`: spec→code nav.
    pub(crate) selected: Option<String>,
    /// Specs being HOVERED from the Specs side panel — drives a transient highlight (the focus
    /// spotlight, unioned over all of them) WITHOUT changing `selected`. Set each frame from
    /// `code_graph_hover_spec`; empty = no hover. status: code-graph-spec-lighting
    pub(crate) hover_specs: Vec<String>,
    /// Focus-spotlight HOP RADIUS for a selected CODE node in the OVERVIEW: the BFS depth (1/2/3)
    /// of neighbours brightened around the selection. SHARED so both lenses agree. Set from the
    /// node right-click "Highlight N hops" menu and remembered for subsequent plain clicks.
    /// Default `1` (reproduces the historical direct-neighbour spotlight). status: code-graph
    pub(crate) focus_hops: u8,
    /// Whether persisted shared state (palette / changes) has been applied yet.
    /// status: graph-view-state-persist
    pub(crate) view_restored: bool,
    /// Number of live lens-views referencing this doc. Tracked for an eviction policy; Phase B keeps
    /// the doc WARM at zero (source-keyed reuse, matching the historical `View`). status: container-tab
    pub(crate) refcount: u32,
}

impl CodeGraphDoc {
    /// An empty doc (no adapter bound yet) — the build path fills it or sets `error`.
    pub(crate) fn empty() -> Self {
        Self {
            adapter: None,
            src: SourceId(String::new()),
            graph: EntityGraph::default(),
            gov: GovCache::default(),
            changes: None,
            show_changes: false,
            last_show_changes: false,
            palette: std::collections::HashMap::new(),
            label_importance: std::collections::HashMap::new(),
            error: None,
            selected: None,
            hover_specs: Vec::new(),
            focus_hops: 1,
            view_restored: false,
            refcount: 0,
        }
    }

    /// The selected node's id, if it's a spec (else `None`) — drives footprint reveal + lighting.
    pub(crate) fn selected_spec(&self) -> Option<&str> {
        let id = self.selected.as_deref()?;
        self.graph.nodes.iter().any(|n| n.id == id && n.kind == SPEC_KIND).then_some(id)
    }

    /// The monikers a selected spec governs (its footprint targets) — revealed in a display +
    /// spotlit. Empty unless a spec is selected and governance is warm.
    pub(crate) fn governed_of_selected(&self) -> Vec<String> {
        match (self.selected_spec(), self.gov.governance()) {
            (Some(spec), Some(g)) => {
                self.graph.governed_ids(g, spec).iter().map(|s| (*s).to_string()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// The change set behind the rings (`Some` only when loaded successfully).
    pub(crate) fn change_set(&self) -> Option<&Changes> {
        self.changes.as_ref().and_then(|r| r.as_ref().ok())
    }

    /// The ring data for a render pass — the change set when "Changes" is on, else `None`.
    pub(crate) fn ring(&self) -> Option<&Changes> {
        if self.show_changes {
            self.change_set()
        } else {
            None
        }
    }

    /// True if the change set is loaded (used by a lens display signature for the `changed_only`
    /// filter — re-derives a display when it flips).
    pub(crate) fn changes_loaded(&self) -> bool {
        self.changes.is_some()
    }
}

/// Build the doc: bind a SCIP adapter → `code_graph()` → merge the spec layer into the unified
/// [`EntityGraph`] → compute the label-importance cache. The lenses' displays + layouts are built
/// by the caller (each lens-view), since they're per-lens.
pub(crate) fn build_doc(
    app: &mut crate::state::AppState,
    source: &crate::tab::CodeSource,
) -> CodeGraphDoc {
    use crate::tab::CodeSource;
    let mut doc = CodeGraphDoc::empty();
    let built = match source {
        CodeSource::Project(note) => super::bind_project(app, note),
        CodeSource::Index(scip) => super::bind_index(app, scip),
    };
    match built {
        Ok((adapter, src)) => {
            let code = adapter.code_graph();
            if doc.gov.links_present(adapter.repo_root()) {
                doc.gov.ensure(&adapter, &src);
            }
            let vault = app.vault_session.vault.clone();
            doc.graph = match app.vault_session.services.read_store.lock() {
                Ok(store) => EntityGraph::build(&code, doc.gov.governance(), &store, &vault),
                Err(_) => EntityGraph::from_code(&code),
            };
            doc.label_importance = entity_graph::label_importance(&doc.graph);
            doc.adapter = Some(adapter);
            doc.src = src;
        }
        Err(e) => doc.error = Some(e),
    }
    doc
}
