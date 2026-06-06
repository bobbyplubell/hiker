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

use crate::activity::{Activity, Ctx, View};
use crate::icons;

/// Zero-sized `Activity` impl for the Clusters activity. Pure descriptor:
/// holds no state. The real state lives in
/// `AppState::clusters_state`; the sidebar surface reaches it via
/// `Ctx::state.downcast_mut::<State>()` and routes broad effects
/// (open a tab / open a note) through `Ctx::defer`.
pub struct Clusters;

impl Activity for Clusters {
    fn id(&self) -> &'static str {
        "clusters"
    }
    fn label(&self) -> &'static str {
        "Clusters"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::ClusterTree)
    }
    fn views(&self) -> Vec<&dyn View> {
        vec![&ClustersSidebar]
    }
}

/// Snapshot the cluster tree `tree_id` (in its current on-disk / in-memory
/// edited state) into a fresh `.canvas` in the chosen `style` via the core
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

impl View for ClustersSidebar {
    fn id(&self) -> &'static str {
        "clusters"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
        egui::ScrollArea::vertical()
            .id_salt("panel-clusters-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                sidebar::render_body(ui, ctx);
            });
    }
}
