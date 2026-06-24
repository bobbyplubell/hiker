//! Read-only 1-hop GRAPH hover-preview for spec/code wikilink pills
//! (`spec-link-preview`): hovering a `[[spec:slug]]` or `[[code:repo/sym]]` pill in
//! the editor shows a small node-link map of the link target's 1-hop neighbourhood,
//! the spatial analogue of the note-body hover preview in `widgets::preview::note`.
//!
//! It runs the SAME frame-loop mechanism as the note preview, for the same reason: the
//! render needs `&mut AppState` + the graph engine + the store/vault (all `!Send`), so
//! it can't ride the `Send + Sync` thumbnail thunk. A pill registers a
//! [`LinkGraphPreviewRequest`] under its own egui-memory slot during the editor render;
//! [`render_link_graph_preview`] runs once per frame after the workbench, dropping a
//! stale request and otherwise drawing the side-anchored, non-interactable popup once the
//! short hover-hold elapses — identical hold + stale-drop + placement to the note
//! preview's PASSIVE path (`widgets::preview::expanded_area_min`).
//!
//! Both routes slice a 1-hop neighbourhood of the unified [`EntityGraph`]
//! ([`EntityGraph::one_hop`]) and render it through [`EntityGraphSource`]:
//!
//! - **Spec links** ALWAYS render something: a warm code-graph view that already knows the
//!   slug supplies its `graph` (so spec→code edges show too), else an adapter-free standalone
//!   build from the store/vault (spec→spec only, no SCIP bind).
//! - **Code links** preview ONLY when a code-graph view for that `repo_id` is already open
//!   and bound (warm) — we slice that view's `graph`. No warm view → nothing renders.
//!
//! status: spec-link-preview

use std::cell::RefCell;

use eframe::egui;

use hiker_code::CodeGraph;
use hiker_graph::LayoutKind;
use hiker_graph_view::graph_view::source::LayoutConfig;
use hiker_graph_view::graph_view::styling::Style;
use hiker_graph_view::graph_view::State;

use super::entity_graph::{EntityGraph, EntityGraphSource, SPEC_KIND};
use crate::state::AppState;
use crate::widgets::preview::{expanded_area_min, EXPAND_HOLD_SECS};

/// Which kind of link the hovered pill targets — a spec slug or a code symbol in a
/// specific repo. Drives both the egui-memory request identity (so the transient layout
/// rebuilds only when the target changes) and which graph the render slices.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum LinkPreviewKind {
    /// A `[[spec:slug]]` link — the unified graph's 1-hop slice around `slug`.
    Spec(String),
    /// A `[[code:repo_id/moniker]]` link — the 1-hop slice around `moniker`, rendered only
    /// when a view for `repo_id` is already warm.
    Code { repo_id: String, moniker: String },
}

impl LinkPreviewKind {
    /// A stable string identity for the previewed target — the thread-local layout key and
    /// the per-target hover-hold anchor (a new identity resets the hold + relayout).
    fn identity(&self) -> String {
        match self {
            LinkPreviewKind::Spec(slug) => format!("spec:{slug}"),
            LinkPreviewKind::Code { repo_id, moniker } => format!("code:{repo_id}/{moniker}"),
        }
    }
}

/// A pending spec/code link graph hover-preview, stashed in egui memory under
/// [`request_id`] during the editor render and consumed once after the workbench by
/// [`render_link_graph_preview`]. Carries the link target (not a note path) — the render
/// needs `&mut AppState` + the graph engine, so it happens at the frame-loop level.
#[derive(Clone)]
struct LinkGraphPreviewRequest {
    kind: LinkPreviewKind,
    /// The hovered pill's screen rect — the popup anchors to its RIGHT (`expanded_area_min`).
    anchor: egui::Rect,
    /// When the current uninterrupted hover began (the popup draws only after a short hold).
    hover_started: f64,
    /// `input.time` of the frame this request was written on. A request from a prior frame is
    /// stale and dropped — which makes the preview vanish when the pointer leaves the pill.
    written_at: f64,
}

/// egui-memory id the link-graph request lives under — distinct from the note-preview and
/// thumbnail slots so the three mechanisms never clobber each other.
fn request_id() -> egui::Id {
    egui::Id::new("preview-link-graph-request")
}

/// Logical content size of the link-graph preview popup (the graph render viewport).
const PREVIEW_SIZE: egui::Vec2 = egui::vec2(300.0, 240.0);

/// Inner padding from the popup frame to the graph render rect.
const PREVIEW_PAD: f32 = 6.0;

/// Force-directed layout box for the tiny 1-hop slice.
const PREVIEW_CFG: LayoutConfig = LayoutConfig { area: 400.0 * 400.0, seed_box: 40.0 };

/// Register a spec/code link graph hover over `kind`'s pill, anchored at `anchor`. Call from
/// the buffer hover tracker when the pointer is over a `[[spec:]]` / `[[code:]]` pill.
pub(crate) fn register_link_graph_hover(ui: &egui::Ui, anchor: egui::Rect, kind: LinkPreviewKind) {
    let ctx = ui.ctx();
    let now = ctx.input(|i| i.time);
    let id = request_id();

    let prev = ctx.data(|d| d.get_temp::<LinkGraphPreviewRequest>(id));
    let hover_started = match prev {
        Some(p) if p.kind == kind => p.hover_started,
        _ => now,
    };

    ctx.data_mut(|d| {
        d.insert_temp(id, LinkGraphPreviewRequest { kind, anchor, hover_started, written_at: now });
    });
}

thread_local! {
    /// The single live link-graph preview engine (only one preview shows at a time). The
    /// `State` holds the force-directed layout, which settles over several frames, so it must
    /// persist across frames for the previewed target; it's recreated (and re-laid-out) when
    /// the target identity changes. The `String` slot is that identity. Parked off `AppState`
    /// so [`render_link_graph_preview`] can hold `&mut app` and `&mut engine` at once.
    static PREVIEW_ENGINE: RefCell<Option<(String, State)>> = const { RefCell::new(None) };
}

/// Draw the one pending spec/code link graph preview, if any, AFTER the workbench has
/// rendered. Paints into a non-interactable `Order::Tooltip` `Area`. A no-op when nothing is
/// hovered, the hold hasn't elapsed, or the target resolves to no graph.
/// status: spec-link-preview
pub(crate) fn render_link_graph_preview(ctx: &egui::Context, app: &mut AppState) {
    let id = request_id();
    let Some(req) = ctx.data(|d| d.get_temp::<LinkGraphPreviewRequest>(id)) else {
        return;
    };
    let now = ctx.input(|i| i.time);
    if req.written_at < now {
        ctx.data_mut(|d| d.remove::<LinkGraphPreviewRequest>(id));
        return;
    }
    ctx.request_repaint();
    if now - req.hover_started < EXPAND_HOLD_SECS {
        return;
    }

    // Build the 1-hop slice for the target. `None` → nothing renders (the pill keeps its plain
    // label, per the spec's fall-back).
    let Some(graph) = build_slice(app, &req.kind) else {
        return;
    };

    PREVIEW_ENGINE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let identity = req.kind.identity();
        let needs_new = slot.as_ref().is_none_or(|(k, _)| k != &identity);
        if needs_new {
            let mut engine = State::new(Style::flat(), LayoutKind::ForceDirected);
            engine.palette_editable = false;
            let src = EntityGraphSource::new(&graph, false, None, None);
            engine.recompute_layout(&src, PREVIEW_CFG);
            *slot = Some((identity, engine));
        }
        let Some((_, engine)) = slot.as_mut() else {
            return;
        };
        paint_link_graph_area(ctx, &req, engine, &graph);
    });
}

/// Build the 1-hop graph slice for the hovered link. Spec links ALWAYS resolve (a warm view
/// that knows the slug, else an adapter-free standalone build). Code links resolve only against
/// an already-open, bound view for the repo (`None` otherwise). status: spec-link-preview
fn build_slice(app: &AppState, kind: &LinkPreviewKind) -> Option<EntityGraph> {
    let hop = match kind {
        LinkPreviewKind::Spec(slug) => build_spec_slice(app, slug),
        LinkPreviewKind::Code { repo_id, moniker } => build_code_slice(app, repo_id, moniker)?,
    };
    (!hop.nodes.is_empty()).then_some(hop)
}

/// The unified graph's 1-hop slice around `slug`: prefer a warm code-graph view that already
/// knows the spec (its `graph` carries spec→code edges); otherwise an adapter-free standalone
/// build from the store/vault (spec→spec only). Either way the preview renders something.
fn build_spec_slice(app: &AppState, slug: &str) -> EntityGraph {
    if let Some(v) = app
        .panels
        .code_graph_docs
        .values()
        .find(|v| v.graph.nodes.iter().any(|n| n.id == slug && n.kind == SPEC_KIND))
    {
        let hop = v.graph.one_hop(slug);
        if !hop.nodes.is_empty() {
            return hop;
        }
    }
    let vault = app.vault_session.vault.clone();
    let Ok(store) = app.vault_session.services.read_store.lock() else {
        return EntityGraph::default();
    };
    let empty = CodeGraph { nodes: Vec::new(), edges: Vec::new() };
    EntityGraph::build(&empty, None, &store, &vault).one_hop(slug)
}

/// The 1-hop slice around `moniker` — ONLY when a view bound to `repo_id` is already open (its
/// adapter + unified graph are warm; no SCIP bind per hover). `None` when no warm view matches
/// the repo or the moniker isn't in its graph.
fn build_code_slice(app: &AppState, repo_id: &str, moniker: &str) -> Option<EntityGraph> {
    let view = app.panels.code_graph_docs.values().find(|v| v.src.0 == repo_id)?;
    let hop = view.graph.one_hop(moniker);
    (!hop.nodes.is_empty()).then_some(hop)
}

/// Place + paint the link-graph preview's `Area`, driving the transient engine over the slice in
/// a non-interactable area (so the engine senses no pointer — the read-only/overview render path).
fn paint_link_graph_area(
    ctx: &egui::Context,
    req: &LinkGraphPreviewRequest,
    engine: &mut State,
    graph: &EntityGraph,
) {
    let pad = PREVIEW_PAD;
    let draw = PREVIEW_SIZE;
    let frame = draw + egui::vec2(pad, pad) * 2.0;
    let min = expanded_area_min(ctx, req.anchor, draw);

    egui::Area::new(egui::Id::new("preview-link-graph"))
        .order(egui::Order::Tooltip)
        .interactable(false)
        .fixed_pos(min)
        .show(ctx, |ui| {
            ui.set_max_size(frame);
            egui::Frame::popup(ui.style())
                .inner_margin(egui::Margin::same(pad as i8))
                .show(ui, |ui| {
                    let (rect, _) = ui.allocate_exact_size(draw, egui::Sense::hover());
                    let mut child =
                        ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(*ui.layout()));
                    child.set_clip_rect(rect);
                    let noop = |_: &egui::Painter, _: egui::Rect, _: &str, _: &str, _: egui::Pos2| {};
                    let src = EntityGraphSource::new(graph, false, None, None);
                    engine.ui(&mut child, &src, noop);
                });
        });
}
