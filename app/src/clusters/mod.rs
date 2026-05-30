//! Clusters feature module — feature-registry-first home for the
//! cluster-trees workflow (sidebar tree picker + node browser, the
//! cluster-review tab body, and the per-feature UI state). Migrated out
//! of `panels::cluster_review` + `sidebar::clusters` as part of Feature
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
//! `crate::feature::Feature` so the registry can dispatch sidebar / icon /
//! label generically. Center-pane dispatch still routes through the
//! legacy `TabKind::ClusterReview` path until Phase 2/3 migrates that
//! consumer.

pub mod panel;
pub mod sidebar;
pub mod state;

use eframe::egui;

use crate::feature::Ctx;
use crate::feature::{Feature, SidebarSurface};
use crate::icons;
use crate::state::AppState;

/// Render entry point for the Clusters sidebar body. Bridges
/// `crate::panels_registry`'s static panel record to the legacy
/// `AppState::clusters_panel` inherent method that already owns the
/// rendering. Kept as a free fn so the static `DockPanel::render` field
/// (a `fn` pointer) can name it without naming the inherent method.
pub fn render_sidebar(ui: &mut egui::Ui, app: &mut AppState) {
    egui::ScrollArea::vertical()
        .id_salt("panel-clusters-body")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            app.clusters_panel(ui);
        });
}

/// Zero-sized `Feature` impl for the Clusters feature. Pure descriptor:
/// holds no state. The real state lives in
/// `AppState::clusters_state`; surfaces reach it via
/// `Ctx::state.downcast_mut::<State>()`.
pub struct Clusters;

impl Feature for Clusters {
    fn id(&self) -> &'static str {
        "clusters"
    }
    fn label(&self) -> &'static str {
        "Clusters"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::ClusterTree)
    }
    fn sidebar(&self) -> Option<&dyn SidebarSurface> {
        Some(&ClustersSidebar)
    }
}

struct ClustersSidebar;

impl SidebarSurface for ClustersSidebar {
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
        // v1 wraps the legacy free-fn body. The body still reaches
        // through `&mut AppState` for now; full ctx-only discipline is a
        // follow-up once the surrounding helpers all take `State`
        // by `&mut` directly rather than via `state.clusters_state.*`.
        // The downcast here proves the wiring is in place — the value is
        // discarded because the legacy renderer is invoked via the
        // panels_registry path, not through this trait, until Phase 2.
        let _state = ctx
            .state
            .downcast_mut::<state::State>()
            .expect("ClustersSidebar invoked with the wrong state type");
        ui.weak("(clusters sidebar — routed via panels_registry in v1)");
    }
}
