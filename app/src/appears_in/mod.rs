//! "Appears in" view — a sidebar surface listing the structural documents that
//! reference the active note: **canvases** (a File node pointing at it),
//! **boards** (a note card), **trails** (a waypoint), and **cluster-trees** (a
//! leaf). The inverse of "what does this note contain", and a companion to
//! backlinks (which lists notes that `[[wikilink]]` here). Surfaced by the
//! `context` container activity alongside backlinks + related.
//! status: canvas-appears-in
//!
//! Boards + trails have indexed store queries (`board_cards` / `trail_waypoints`
//! derived tables); canvases + trees are best-effort on-demand scans (no
//! reference index yet). All four are cached by active path so they only re-run
//! on a note switch — the same posture as the backlinks vault scan.

use std::sync::{Arc, Mutex};

use eframe::egui;
use hiker_core::store::Store;
use hiker_core::trees::types::Db;
use hiker_core::vault::Vault;
use hiker_theme as theme;

use egui_workbench::activity::View;
use crate::activity::{AppCtx, SurfaceCtx};
use crate::editor_pane;
use crate::icons::{self, Icon};

/// One referencing document: a vault path to open plus the display label for its
/// row. Built at compute time so render is a cheap clone-and-draw.
#[derive(Clone)]
struct Ref {
    path: String,
    label: String,
}

/// Cached per-active-note reverse references, recomputed on note switch. Owned by
/// `AppState::appears_in_state` (top-level, per `feature-state-ownership`). Four
/// buckets so the body can group them under type headers. Inlined here rather
/// than a sibling `state.rs` (too small to justify its own file under
/// `scripts/check-splits.py`).
#[derive(Default)]
pub struct State {
    /// The note the cache was computed for (vault-relative path).
    computed_for: Option<String>,
    canvases: Vec<Ref>,
    boards: Vec<Ref>,
    trails: Vec<Ref>,
    trees: Vec<Ref>,
}

/// Zero-sized `View` descriptor. State lives in `AppState::appears_in_state`;
/// the surface reaches it via `ctx.state.downcast_mut::<State>()`. Exposed so
/// the `context` container activity can list it among its `views()`.
pub struct AppearsInSidebar;

impl View<dyn AppCtx> for AppearsInSidebar {
    fn id(&self) -> &'static str {
        "appears-in"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut (dyn AppCtx + 'static)) {
        let Some(mut ctx) = ctx.surface_ctx(self.state_key()) else {
            return;
        };
        let ctx = &mut ctx;
        // The workbench accordion owns the section header + collapse; the body is
        // just the content. [feature-panel-single-accordion]
        ui.add_space(8.0);
        render_body(ui, ctx);
    }
}

fn render_body(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>) {
    let Some(active) = ctx.active_path.clone() else {
        ui.label(
            egui::RichText::new("(open a note to see where it appears)")
                .color(theme::muted())
                .small(),
        );
        return;
    };

    // Re-scan only when the active path changed since the last cache.
    let stale = ctx
        .state
        .downcast_ref::<State>()
        .expect("appears_in state")
        .computed_for
        .as_deref()
        != Some(active.as_str());
    if stale {
        // Clone the Arc handles so the compute borrows nothing of `ctx`, leaving
        // `ctx.state` free to write the result into.
        let store = ctx.services.read_store.clone();
        let trees = ctx.services.trees.clone();
        let vault = ctx.vault.clone();
        let (canvases, boards, trails, trees_v) = compute(&store, &trees, &vault, &active);
        let st = ctx
            .state
            .downcast_mut::<State>()
            .expect("appears_in state");
        st.canvases = canvases;
        st.boards = boards;
        st.trails = trails;
        st.trees = trees_v;
        st.computed_for = Some(active.clone());
    }

    // Snapshot the groups out of state so the immutable borrow ends before we
    // queue `ctx.defer` closures (which need `ctx` mutably).
    let groups: [(Icon, &str, Vec<Ref>); 4] = {
        let st = ctx
            .state
            .downcast_ref::<State>()
            .expect("appears_in state");
        [
            (Icon::Canvas, "Canvases", st.canvases.clone()),
            (Icon::Clipboard, "Boards", st.boards.clone()),
            (Icon::Boot, "Trails", st.trails.clone()),
            (Icon::ClusterTree, "Trees", st.trees.clone()),
        ]
    };
    if groups.iter().all(|(_, _, rows)| rows.is_empty()) {
        ui.label(
            egui::RichText::new("(not in any canvas, board, trail, or tree)")
                .color(theme::muted())
                .small(),
        );
        return;
    }
    for (icon, title, rows) in &groups {
        if rows.is_empty() {
            continue;
        }
        ui.add_space(4.0);
        ui.label(egui::RichText::new(*title).small().color(theme::muted()));
        for r in rows {
            draw_ref_row(ui, ctx, *icon, r, &active);
        }
    }
}

/// Draw one reference row and wire its open + hover-preview behavior. Canvas rows
/// (`.canvas`) get an inline spatial thumbnail before the label and a click that
/// snaps the canvas to the referencing node; the other groups (boards, trails,
/// trees) are plain `.md` docs that open in the editor and register a markdown
/// hover-preview. Extracted from `render_body` to keep it under the line cap.
fn draw_ref_row(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>, icon: Icon, r: &Ref, active: &str) {
    if r.path.ends_with(".canvas") {
        // Plain canvas icon in the normal row; the spatial canvas preview shows
        // only on hover (no inline thumbnail). status: canvas-appears-in
        let resp = ui
            .add(egui::Button::image_and_text(icons::ICONS.image(icon), &r.label))
            .on_hover_text(&r.path);
        if resp.hovered() {
            if let Ok(bytes) = ctx.vault.read_file(&r.path) {
                let provider = crate::panels::canvas::thumbnail::CanvasPreview::new(bytes);
                crate::widgets::preview::register_hover_only(
                    ui,
                    &provider,
                    ctx.vault.root(),
                    resp.rect,
                    crate::widgets::preview::ThumbnailOpts::default(),
                );
            }
        }
        if resp.clicked() {
            // Open the canvas and snap the view to the node that points at this note
            // (selected), not the whole board. status: canvas-appears-in
            let path = r.path.clone();
            let note = active.to_string();
            ctx.defer(move |app| crate::panels::canvas::open_focused(app, &path, &note));
        }
        return;
    }

    let resp = ui
        .add(egui::Button::image_and_text(icons::ICONS.image(icon), &r.label))
        .on_hover_text(&r.path);
    if resp.hovered() {
        crate::widgets::preview::register_note_hover(ui, resp.rect, &r.path);
    }
    if resp.clicked() {
        let path = r.path.clone();
        ctx.defer(move |app| editor_pane::open_file(app, &path, false));
    }
}

/// Gather the four reference sets for `note`. Canvases + trees are on-demand
/// scans; boards + trails are indexed store queries taken under a single lock.
/// Any failing source degrades to empty rather than failing the whole panel.
fn compute(
    store: &Arc<Mutex<Store>>,
    trees: &Arc<Db>,
    vault: &Arc<Vault>,
    note: &str,
) -> (Vec<Ref>, Vec<Ref>, Vec<Ref>, Vec<Ref>) {
    let canvases = hiker_core::canvas::canvases_referencing(vault, note)
        .unwrap_or_default()
        .into_iter()
        .map(|p| Ref { label: title_of(&p), path: p })
        .collect();

    let (boards, trails) = match store.lock() {
        Ok(s) => {
            let boards = s
                .boards_containing_note(note)
                .unwrap_or_default()
                .into_iter()
                .map(|h| Ref {
                    label: format!("{} \u{00b7} {}", title_of(&h.board_path), h.column_name),
                    path: h.board_path,
                })
                .collect();
            // A note can be several waypoints in one trail; dedup by trail doc path.
            let mut seen = std::collections::HashSet::new();
            let trails = s
                .trails_containing_note(note)
                .unwrap_or_default()
                .into_iter()
                .filter(|h| seen.insert(h.tree_path.clone()))
                .map(|h| Ref { label: title_of(&h.tree_path), path: h.tree_path })
                .collect();
            (boards, trails)
        }
        Err(_) => (Vec::new(), Vec::new()),
    };

    let trees_v = trees
        .trees_containing_note(note)
        .unwrap_or_default()
        .into_iter()
        .map(|h| Ref {
            label: if h.name.is_empty() { title_of(&h.path) } else { h.name },
            path: h.path,
        })
        .collect();

    (canvases, boards, trails, trees_v)
}

/// Basename without the `.md` / `.canvas` extension — the row's display title.
fn title_of(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.strip_suffix(".canvas")
        .or_else(|| base.strip_suffix(".md"))
        .unwrap_or(base)
        .to_string()
}
