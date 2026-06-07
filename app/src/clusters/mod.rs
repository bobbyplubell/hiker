//! Clusters activity module — activity-registry-first home for the
//! cluster-trees workflow (sidebar tree picker + node browser, the
//! cluster-review tab body, and the per-activity UI state). Migrated out
//! of `panels::cluster_review` + `sidebar::clusters` as part of activity
//! registry Phase 1 (`feature-cluster-migration`).
//!
//! Layout:
//!
//! - `state.rs` — `State` (was `state::ClusterUiState`). Owned
//!   by `AppState::clusters_state`.
//! - `sidebar/` — sidebar mode body (tree picker, node tree, inline
//!   rename, DnD reparent, stage queue forms, advanced params).
//! - `panel/` — cluster-review center-pane tab body.
//!
//! The `Clusters` zero-sized type at the bottom implements
//! `crate::activity::Activity` so the registry can dispatch sidebar / icon /
//! label generically. Center-pane dispatch still routes through the
//! legacy `TabKind::ClusterReview` path until Phase 2/3 migrates that
//! consumer.

pub mod panel;
pub mod preset;
pub mod sidebar;
pub mod state;

use eframe::egui;

use egui_workbench::activity::{Activity, View};
use crate::activity::AppCtx;
use crate::icons;

/// Zero-sized `Activity` impl for the Clusters activity. Pure descriptor:
/// holds no state. The real state lives in
/// `AppState::clusters_state`; the sidebar surface reaches it via
/// `ctx.state.downcast_mut::<State>()` and routes broad effects
/// (open a tab / open a note) through `SurfaceCtx::defer`.
pub struct Clusters;

impl Activity<dyn AppCtx> for Clusters {
    fn id(&self) -> &'static str {
        "clusters"
    }
    fn label(&self) -> &'static str {
        "Clusters"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::ClusterTree)
    }
    fn views(&self) -> Vec<&dyn View<dyn AppCtx>> {
        vec![&ClustersSidebar]
    }
}

/// Snapshot the cluster tree `tree_id` (in its current on-disk / in-memory
/// edited state) into a fresh `.canvas` in the chosen `style` via the core
/// One numeric parameter row in a clustering config grid: a label plus an
/// `egui::Slider`, ended with `end_row()`. Shared by the review-tab config
/// form (`panel`) and the sidebar quick-params popover (`sidebar`) so both
/// surfaces render every numeric knob identically — a track with an
/// editable readout you can also click to type an exact value into —
/// rather than the old mix of `Slider` and `DragValue`. `log = true` gives
/// low-end precision to wide integer ranges (iterations, min cluster size)
/// where the useful values cluster near the bottom. Booleans stay
/// checkboxes; they aren't numbers. An empty `hover` attaches no tooltip.
pub(crate) fn param_slider<N: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut N,
    range: std::ops::RangeInclusive<N>,
    log: bool,
    hover: &str,
) {
    let label_resp = ui.label(label);
    let slider_resp = ui.add(egui::Slider::new(value, range).logarithmic(log));
    if !hover.is_empty() {
        label_resp.on_hover_text(hover);
        slider_resp.on_hover_text(hover);
    }
    ui.end_row();
}

/// export builder, then open the new file framed-to-fit in the canvas view.
/// On success toasts the new basename; on failure surfaces the core error as
/// an error toast (never panics). The cluster-editor toolbar's "Export to
/// canvas" menu defers here, one entry per style.
/// status: canvas-export-tree-verb
pub(crate) fn export_tree_to_canvas(
    app: &mut crate::state::AppState,
    tree_id: &str,
    style: hiker_core::canvas::export::TreeCanvasStyle,
) {
    let result = hiker_core::canvas::export::write_tree_canvas(
        &app.vault_session.services.trees,
        &app.vault_session.vault,
        &app.vault_session.services.oplog,
        tree_id,
        style,
    );
    match result {
        Ok(new_rel) => {
            let base = new_rel.rsplit('/').next().unwrap_or(&new_rel).to_string();
            crate::panels::canvas::open_fresh(app, &new_rel);
            app.push_toast(format!("Exported to {base}"), crate::state::ToastLevel::Info);
        }
        Err(e) => app.push_toast(
            format!("Export to canvas failed: {e}"),
            crate::state::ToastLevel::Error,
        ),
    }
}

struct ClustersSidebar;

impl View<dyn AppCtx> for ClustersSidebar {
    fn id(&self) -> &'static str {
        "clusters"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut (dyn AppCtx + 'static)) {
        let Some(mut ctx) = ctx.surface_ctx(self.state_key()) else {
            return;
        };
        let ctx = &mut ctx;
        egui::ScrollArea::vertical()
            .id_salt("panel-clusters-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                sidebar::render_body(ui, ctx);
            });
    }
}
